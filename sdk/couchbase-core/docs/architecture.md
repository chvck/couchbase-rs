# Architecture

> **Provenance.** Carried from cbcore-rs `docs/architecture.md` at commit
> `dbf1c0b` (2026-08-17), adapted to couchbase-core on 2026-08-18. cbcore-rs was
> a second Rust port of the same Go client; several structures it describes were
> deliberately **not** taken in this extraction, and those are marked *Not taken
> here* rather than edited out. Where a cbcore-rs module name appears with no
> counterpart in this crate, that is said explicitly — there are no dangling
> references left pointing at code that does not exist.

A Couchbase client. It talks memcached binary protocol to the data service, GSI
queryport to the indexing service, and HTTP to everything else, and it hides
from its caller which node holds what.

The shape follows `gocbcorex`, the Go client this is a port of. Where a name
here matches a name there, the correspondence is deliberate and worth trusting;
where it does not, the divergence is usually documented at the site.

## The one organising idea

Every layer resolves exactly one question, and owns the recovery for that
question alone.

A `get` needs four things answered before a packet can go out: which retry
attempt this is, what collection id `scope.collection` currently has, which
vbucket the key hashes to and which node holds it, and which connection to that
node to use. Each answer can go stale independently, and each has a different
repair — re-ask the server for a collection id, re-read the cluster config for a
vbucket map, open a new connection.

So rather than one function that knows all four, each concern is a trait with a
resolver, and a matching **orchestrator** that wraps a closure and owns the
invalidate-and-retry for that concern. `CrudComponent::orchestrate_simple_crud`
(`src/crudcomponent.rs`) composes them by nesting:

```
orchestrate_retries                     retry.rs             — backoff, and what is retriable at all
└─ orchestrate_memd_collection_id       collectionresolver.rs — name -> (collection id, manifest rev)
   └─ orchestrate_memd_routing          vbucketrouter.rs      — key -> (endpoint, vbucket id)
      └─ orchestrate_endpoint_kv_client kv_orchestration.rs   — endpoint -> connection
         └─ operation(collection_id, vbucket_id, client)
```

Read it outward from the operation. The operation closure is handed everything
it needs and knows none of it. Read it inward from the top and each layer is a
chance to recover: the collection orchestrator catches an unknown-collection
error from anywhere inside itself, invalidates its cache, re-resolves and calls
back in — so an operation that raced a collection drop retries once,
transparently.

The consequence worth knowing: **an operation body never handles a stale
routing fact.** If you find yourself wanting to, the recovery belongs in
whichever layer owns that fact.

> **Not taken here — the `*_orch` trait.** cbcore-rs expressed each orchestrator
> as a companion *trait* (`collection_resolver_orch`, and so on) with a blanket
> impl over the base trait, and composed them on `Agent::orchestrate_basic`. This
> crate uses a free `async fn` per concern, in the same module as the trait it
> orchestrates, composed on `CrudComponent`. The nesting is identical; only the
> vehicle differs. There is also no counterpart to cbcore-rs's
> `agent_crud::dispatch_basic` — each operation in `crudcomponent.rs` inlines its
> own encode-request/await-response closure rather than sharing a thin wrapper.

## Layers

Bottom to top. The rule at every boundary is the same: **a protocol module knows
how to be one connection and nothing about which one.**

### Protocol

| Module | Owns |
|---|---|
| [`memdx`](../src/memdx/) | memcached binary protocol: framing, opcodes, HELO negotiation, SASL, per-operation request/response types |
| [`indexerx`](../src/indexerx/) | GSI queryport: the six-byte frame, the protobuf schema, scan streams, and `/getIndexStatus` over HTTP |
| [`httpx`](../src/httpx/) | An HTTP client, an auth trait, a chunk decoder and a streaming JSON row reader |
| [`queryx`](../src/queryx/), [`searchx`](../src/searchx/), [`analyticsx`](../src/analyticsx/), [`mgmtx`](../src/mgmtx/) | Per-service request encoding, response reading and error taxonomy, over `httpx` |
| `scram` (private) | SCRAM-SHA, wrapped for the shape `memdx`'s SASL flow wants |

`memdx` and `indexerx` are peers with one structural difference that shapes
everything above them:

- **`memdx` multiplexes.** Many operations are in flight on one socket,
  correlated by the opaque field. How that correlation is done, and the
  measurement that settled it, is in
  [response-correlation.md](response-correlation.md).
- **`indexerx` does not.** There is no request id on the wire; ordering *is* the
  correlation, so a request owns its connection until the server sends a
  terminator. Hence no opaque map, no dispatcher, and a `Client` that serves one
  request at a time.

That difference is why the pooling above them cannot be shared.

> **Not taken here — `indexerclient_provider`.** cbcore-rs put a lease-based
> connection provider above `indexerx`: dropping the lease returns the
> connection, and a scan *consumes* the lease because it owns its connection
> until the stream ends. couchbase-core has no pooling above `indexerx` at all.
> `indexerx` is `pub mod indexerx` in `src/lib.rs` and nothing in `agent.rs`
> refers to it — the protocol module is ported and has no consumer yet. A router
> and a client provider are the two pieces still missing.

### Connections and pooling

For KV, three layers, each adding one thing:

| Module | Adds |
|---|---|
| [`kvclient_babysitter`](../src/kvclient_babysitter.rs) | Keeps *one* connection alive: reconnect-on-failure, throttling, and an `ArcSwap` holding the current client so the dispatch path does not wait on the reconnect loop |
| [`kvclientpool`](../src/kvclientpool.rs) | *N* connections to one endpoint, `N = KvConfig::num_connections`, selected round-robin off an `ArcSwap` fast map |
| [`kvendpointclientmanager`](../src/kvendpointclientmanager.rs) | Every endpoint's pool, behind an `ArcSwap` fast state so the dispatch path does not take a lock |

The pool is **fixed-size and uncapped**: it mints exactly `num_connections`
babysitters at construction — the default is 1 — and `get_client` is
`client_idx.fetch_add(1) % len` over the fast map. There is no minimum, no
maximum, no idle timeout, no per-connection outstanding-operation limit and no
waiter queue. A connection carries as many concurrent operations as callers
hand it. The one adjacent knob is `KvConfig::on_demand_connect`, which is
global rather than per-pool and defers the connect rather than reaping it.

> **Not taken here — `pool::multi_manager` and `PoolShape`.** cbcore-rs had a
> fourth layer above the endpoint manager holding *every manager*, plus the
> topology and credentials they share, and minted two managers from it: a
> `primary` for point operations and a `bulk` one for operations that answer with
> a stream of packets and so hold a connection for as long as the answer takes.
> The shape of each was a `PoolShape` — min, max, `connection_concurrency`, idle
> timeout. None of that is here: one manager, one fixed pool shape, no
> concurrency token.
>
> That is a defensible position *at this commit*, because couchbase-core has no
> long-lived multi-response operation to protect a point operation from — no
> range scan, no `stats` sweep, no `get_all_vb_seqnos`. The measurements that
> made cbcore-rs split its managers, and what they say about when that stops
> being true, are in [connection-tuning.md](connection-tuning.md). Read it before
> landing the first streaming KV operation, not after.

### Providers

Callers want a connection two ways, and both are methods on one trait,
`KvEndpointClientManager`:

- `get_client()` — any healthy connection. What a config poll wants. The current
  implementation picks the first pool in the map and says so in a comment.
- `get_endpoint_client(endpoint)` — a connection to *this* endpoint. What
  anything routed by vbucket wants.
- `get_client_per_endpoint()` — one from each, for a fan-out.

The first two have an orchestrator each in `kv_orchestration.rs`
(`orchestrate_kv_client`, `orchestrate_endpoint_kv_client`), identical apart
from which getter they call.

> **Not taken here — two provider *traits*.** cbcore-rs split these into
> `KvClientProvider` and `KvEndpointClientProvider`, both implemented by one
> type, on the argument that an operation naming an endpoint it did not route to
> is a bug and the type system is where that gets caught. Here the distinction is
> at method level, so it is a convention rather than a constraint.

### Routing

| Module | Resolves |
|---|---|
| [`vbucketmap`](../src/vbucketmap.rs) | key -> vbucket -> server index |
| [`vbucketrouter`](../src/vbucketrouter.rs) | vbucket + replica index -> endpoint, over an `ArcSwap` so a config update is a pointer store |
| [`collectionresolver`](../src/collectionresolver.rs) | `scope.collection` -> collection id. `_memd` asks the server, `_cached` remembers, `orchestrate_memd_collection_id` recovers |

`collection_resolver_cached` is the one to read if you read one: an `ArcSwap`
fast path for the hit, a slow path that collapses concurrent misses onto a
single in-flight resolve through a per-key `Notify`, and a stack-formatted
`scope.collection` key (`FormattedCollectionPath`, 256 bytes with an owned
fallback) so a hit allocates nothing. An invalidation carrying an older manifest
revision than the cached entry is declined.

> **Not taken here — the flat vbucket table.** cbcore-rs stored the vbucket map
> as one flat table and deserialised the wire JSON straight into it, on the
> measurement that a `Vec<Vec<i32>>` costs an allocation per vbucket — 1025 on a
> standard bucket — and then another 1025 to convert. couchbase-core's
> `VbucketMap` is `Vec<Vec<i16>>`, moved directly out of
> `cbconfig::VBucketServerMap`. So the conversion pass is not paid, but the
> per-vbucket allocation on parse is. That cost lands on config parsing, not on
> the dispatch path, and no allocation test covers config parsing — so the size
> of it here is *unmeasured*, and cbcore-rs's figure is a prediction rather than
> a result. See [cluster-topology.md](cluster-topology.md).

> **Not taken here — `EndpointId(Intern<..>)`.** cbcore-rs interned endpoint
> identities so a routing result was `Copy` and cost no allocation. The
> trade-off it accepted was that `internment::Intern` leaks permanently.
> couchbase-core keys endpoints by `String` and hands out `Arc<str>` from the
> router.

> **Absent here — `indexrouter` and `vbuuid_cache`.** cbcore-rs had a router
> mapping index name -> definition id -> which host holds which partition, and a
> per-agent cache of vbucket UUIDs. Neither exists in couchbase-core.
> `indexerx::status` parses everything a router would need and nothing consumes
> it; `vbuuid` appears only as a field on `MutationToken`. The rule cbcore-rs
> derived about what a config revision can and cannot prove is recorded in
> [cluster-topology.md](cluster-topology.md) anyway, because it is the sort of
> thing that gets got wrong twice.

### Retry

[`retry.rs`](../src/retry.rs) is deliberately small. `orchestrate_retries` is a
loop; `RetryManager::maybe_retry` returns an optional delay. Its interesting
half is `error_to_retry_reason` — a positive match returning
`Option<RetryReason>`, so an unrecognised error falls through to `None` and
fails rather than spins.

Two gates cbcore-rs did not model: `RetryRequest::is_idempotent`, set per
operation, and `RetryReason::allows_non_idempotent_retry()`.

**The default strategy is fail-fast.** `DEFAULT_RETRY_STRATEGY` is a
`FailFastRetryStrategy`, and every options `Default` in `src/options/` clones
it. `BestEffortRetryStrategy` — exponential backoff, min 1 ms, max 1000 ms,
factor 2 — is opt-in per operation. Its own doc comment claims it is the
default; that comment is wrong, and is noted here because a reader who trusts it
will mis-predict every failure path. (cbcore-rs defaulted to best-effort, with
bounds of 10 ms to 500 ms.)

> **Deliberately different — unknown collections and scopes.** cbcore-rs
> excluded `CollectionUnknown` and `ScopeUnknown` from its retriable set on the
> argument that they are handled a layer down by the collection orchestrator,
> which can actually fix them, and that retrying at the retry layer would re-run
> the whole operation against the same stale cache. couchbase-core does the
> opposite: all three of `UnknownCollectionID`, `UnknownCollectionName` and
> `UnknownScopeName` map to `RetryReason::KvCollectionOutdated`, which is in
> `always_retry()` and short-circuits `maybe_retry` before the strategy is
> consulted at all, on the fixed ladder 1/10/50/100/500/1000 ms. So even a
> fail-fast `get` retries a renamed collection. The one place the cbcore-rs
> instinct survives is `CrudComponent::get_collection_id`, which runs its own
> loop and returns non-retriably for a name that does not exist — a name that
> will never resolve must not spin.

### The agent, and components

[`agent::Agent`](../src/agent.rs) is the assembly point and the only type a
caller needs. `Agent::new` bootstraps over the seed endpoints, then builds the
manager, router, resolver and config watcher from the config it got back, and
republishes subsequent configs through `AgentComponentConfigs::gen_from_config`.

Operations are split by kind rather than by anything deeper: `agent_ops.rs` is
the public surface, `crudcomponent.rs` the KV operations and their
orchestration, and one component per HTTP service — `querycomponent`,
`searchcomponent`, `analyticscomponent`, `mgmtcomponent` — each sitting on
`httpcomponent` and the agent's endpoint list rather than on any of the routing
above, because an HTTP service call is not routed by key; any node running the
service will do. User management and metakv2 live inside `mgmtcomponent`, not as
components of their own.

## Conventions

**Orchestration means invalidate-and-retry for one concern.** A free `async fn`
next to the trait it wraps. If you add a resolver whose answer can go stale, it
gets one.

**A protocol module never learns about topology.** `memdx` and `indexerx` take
an address. If you need a protocol module to know which node to talk to, the
knowledge goes in the layer above.

**Measurements are load-bearing.** Several structures here are shaped by a
measured number rather than a judgement — the stack-formatted collection key,
the single-pass query body, `httpx::decoder`'s `MAX_PIN_RATIO`, the pipelined
bootstrap. Those numbers are in [allocation-costs.md](allocation-costs.md) and
[row-streaming.md](row-streaming.md), and `tests/allocations.rs` fails if the
counts move **in either direction** — it asserts equality, not a ceiling, so an
improvement reports as a failure and gets written down. Changing one of those
structures means re-running the measurement, not re-arguing it.

**There is no `unsafe` in this crate.** Not one occurrence, in `src/`, `tests/`
or `benches/`. cbcore-rs had it in two modules, and the larger of the two — a
slot table whose payload lived in an `UnsafeCell`, made sound by a packed
generation-and-state word — was measured during the extraction and not carried;
see [response-correlation.md](response-correlation.md). Note that the absence is
a fact about the code as written, not a compiler-enforced guarantee: there is no
`#![forbid(unsafe_code)]` on the crate root.
