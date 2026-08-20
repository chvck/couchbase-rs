# Cluster topology

> **Provenance.** Carried from cbcore-rs `docs/cluster-topology.md` at commit
> `dbf1c0b` (2026-08-17), adapted to couchbase-core on 2026-08-18. cbcore-rs
> learned the cluster's shape over HTTP and watched it two ways; couchbase-core
> learns it over memcached and watches it one way. Both are described below —
> the mechanism sections say what this crate does, and the cbcore-rs behaviour is
> kept where it is the reason a rule exists.

How the client learns the cluster's shape, how a change reaches the things that
route on it, and — the part that has bitten more than once — what a piece of
topology may and may not be trusted to prove.

## Bootstrap

`Agent::new` calls `Agent::get_first_config`, which tries two sources in order,
in a loop that does not give up:

1. **memcached first.** For each seed KV address it opens a throwaway client and
   sends `GetClusterConfig`, then closes it. Mutation tokens and server
   durations are forced off for this client — it exists to fetch one config.
2. **HTTP second**, only after every KV seed has failed:
   `pools/default/b/<bucket>` when a bucket is named,
   `pools/default/nodeServices` otherwise.

If both phases fail, it logs and sleeps a hardcoded one second, then starts
again. There is no attempt limit and no accumulated failure list: each failure
is warned individually and the next seed is tried.

> **Different in cbcore-rs, and worth knowing which trade you are on.**
> cbcore-rs bootstrapped over HTTP only, walked the seeds once, took the first
> that answered, and — if all of them failed — returned the *accumulated* list of
> failures as the error, on the argument that a client which cannot reach any
> seed should say what it tried. couchbase-core instead retries forever, which
> survives a cluster that is merely not up yet but gives a caller nothing to
> print. Neither is free: there is no aggregate error type anywhere in this
> crate.
>
> One asymmetry in the current loop is worth a sentence, because it is easy to
> read past: an *unreachable* seed retries forever, but a *reachable* node that
> answers with malformed JSON aborts `Agent::new` outright — that path uses `?`.

`ParsedConfig` is the internal form, and the seam that matters: nothing above
`configparser::ConfigParser` sees Couchbase's wire JSON. `cbconfig` holds the
wire types (`TerseConfig` and friends) and does the `$HOST` substitution before
deserialising; `ConfigParser::parse_terse_config` turns that into `ParsedConfig`.
That seam is what lets routing be tested against hand-built configs with no
cluster present, and `configparser.rs`'s own tests are built on a verbatim
four-node 8.0.3 capture for exactly that reason.

**One thing the parser does that is easy to miss** is take the vbucket map
straight off the wire as `Vec<Vec<i16>>` and move it into `VbucketMap` unchanged.
cbcore-rs deserialised into a flat table instead, on the measurement that a
nested `Vec` costs an allocation per vbucket — 1025 on a standard bucket — and
then another 1025 to convert. couchbase-core does not pay the conversion, but
does pay the per-vbucket allocation. Nothing measures config parsing here, so
treat cbcore-rs's figure as a prediction about this crate, not a result.

## Watching

**One watcher: `config::watcher_memd`, over an existing KV connection.**
`ConfigWatcherMemd` polls `GetClusterConfig` every 2500 ms
(`ConfigPollerConfig::default`, with a matching 2500 ms fetch timeout), trying
endpoints in turn and treating a failure or a timeout on one as "move to the
next" rather than as a config error, because a single unreachable node is not a
topology failure. Only a fully failed sweep costs an extra polling period before
the endpoint list resets. There is no jitter on the sleep.

The poll is nearly free — it rides a connection the client already has, so there
is no extra socket and no extra authentication — and on a modern cluster it
mostly does not happen at all. Two push paths pre-empt it:

- **`ClusterMapChangeNotificationBrief`.** If the connection negotiated it, the
  poll's `skip_fetch_cb` returns true and `poll_one` answers `Ok(None)` without a
  round trip; the server instead sends unsolicited `Set` packets whose first
  sixteen bytes of extras are `rev_epoch` then `rev_id`, big-endian, which
  `Agent::unsolicited_packet_handler` turns into an out-of-band version.
- **Not-my-vbucket.** A NMVB response carries a config, which
  `StdNotMyVbucketConfigHandler` feeds straight in.

`known_version` is only sent when there is a revision to send and the connection
negotiated `ClusterMapKnownVersion`.

> **Not taken here — the HTTP watcher.** cbcore-rs ran `config::watcher_http`
> against a mgmt endpoint when no bucket was open, and the memd watcher when one
> was. couchbase-core has no HTTP config watcher: `Agent::fetch_http_config`
> exists and is called from bootstrap only. A bucketless agent therefore watches
> over KV like any other.

> **Not taken here — the buckets watcher.** cbcore-rs had
> `config::buckets_watcher` answering a different question — which buckets exist
> — for callers that need to enumerate rather than route. couchbase-core has no
> reactive equivalent; `OnDemandAgentManager` creates a per-bucket agent lazily
> on first use, with an `ArcSwap` fast map, a mutex-guarded slow map and a
> `Notify` per key to collapse concurrent misses.

## Distribution

A new `ParsedConfig` reaches consumers two ways, and the split is by whether the
consumer is on the dispatch path:

- **A `watch` channel** carries the config itself.
  `ConfigManager::watch()` hands out a `watch::Receiver<ParsedConfig>`;
  `Agent::start_config_watcher` loops on `changed()`, clones out of
  `borrow_and_update()` inside a block so the lock drops early, and applies it. A
  second `watch` carries just the `ConfigVersion` back to the poller so it can
  build `known_version`.
- **`ArcSwap`** for the *derived* structures read per operation: the vbucket
  router's `VbucketRoutingInfo`, the endpoint manager's fast state, each pool's
  fast map, each babysitter's current client, the cached collection manifest, the
  error map, the search component's state, the HTTP client.

> **One correction to cbcore-rs's framing, because it does not carry.**
> cbcore-rs held the agent's `latest_config` in an `ArcSwap` too, and described
> the split as "`ArcSwap` for anything read per operation, a `watch` channel for
> anything that reacts". Here the `ParsedConfig` is behind mutexes — a
> `tokio::sync::Mutex` on `AgentState` and a `std::sync::Mutex` inside
> `ConfigManagerMemd` — and only the things *derived* from it are behind
> `ArcSwap`. The dispatch path never reads the config directly, so this is not a
> hot-path cost; the framing is just different.
>
> The derived layer is also not uniform, which is worth knowing before you
> assume it: `HttpComponent` and `DiagnosticsComponent` hold their state in a
> plain `Mutex`, so query, analytics and mgmt endpoint selection takes a lock per
> operation. Only `SearchComponent` uses `ArcSwap` for the same job.

The fan-out from one config is `AgentComponentConfigs::gen_from_config`, applied
by `AgentInner::update_state_locked` in a deliberate order — add new endpoints,
then swap the routing table, then remove departed ones — with the race that
ordering avoids explained at the site.

> **Not taken here — a manager that owns the endpoint list for all the others.**
> cbcore-rs put endpoints and credentials on `pool::multi_manager` and pushed
> them into every manager it minted, so a manager constructed anywhere else would
> silently never receive an update. With one manager here that structural
> guarantee has nothing to guard, but it is the thing to restore if a second one
> is ever added — see [connection-tuning.md](connection-tuning.md).

## What topology can and cannot prove

This is the part worth reading twice, because two of the items below were
originally got wrong in a way that produced no error at all.

### A config revision that has not moved is not evidence a cached fact is good

**Recorded in cbcore-rs.** Specifically: it does not validate a vbucket UUID
map. cbcore-rs's config revision came from the config watcher, which preferred
the HTTP source; the UUIDs came over memcached. An unmoved revision therefore
says only "ns_server has not told us yet", over a link with its own latency — so
using it as a *validity* proof serves a stale UUID beside a fresh seqno for as
long as the two channels disagree.

Used the other way it is sound: a revision that **has** moved invalidates,
because a fork moves the vbucket map. That direction can only discard a good
map, which costs one read.

**Where couchbase-core stands.** There is no vbucket-UUID cache here, and no
identifier called `config_revision` — the revision is always the pair
`rev_id`/`rev_epoch`, and it is used only for monotonicity:
`can_update_config` accepts a strictly greater revision and logs the rest. So
the trap has no consumer to spring on. It is recorded because the moment
anything caches a per-vbucket fact and reaches for "has the config moved?" as a
freshness check, it is back — and because the same asymmetry (a revision may
invalidate but must not validate) is the right default for any such cache.

One live exception to the monotonicity rule, since it is the sort of thing a
reader will assume away: a bucket-type change, including `Some` ↔ `None`, forces
acceptance as a bucket takeover and bypasses the revision comparison entirely.

### `/getIndexStatus` reports mgmt ports, not index ports

`hosts`, and the keys of `partitionMap`, carry the node's **mgmt** port —
`:8091` — not its index HTTP port and not its scan port. Measured against
Couchbase 8.0 in cbcore-rs, and independently reconfirmed here during the
extraction; it is contrary to the comment on the Go `IndexStatus` type, which
says `host:index_http_port`.

Scanning needs `indexScan`, so anything routing a scan has to join the two
against `nodeServices`, which is keyed the same way and therefore makes the join
trivial. `indexerx::status` deliberately does not do this itself: it has no
cluster config and reports what the server said. **The join has no home in
couchbase-core yet** — there is no index router; see
[architecture.md](architecture.md).

A second measured fact from the same response, also contrary to the Go client's
comment: `num_partition` is **per row, and a row is per host**, not the index
total. `IndexStatus::declared_partitions()` reads the real total out of the
echoed DDL instead.

### An absent field is not the same as an explicit `null`

`#[serde(default)]` fills in a field the server *omitted*. It does nothing for a
field the server sent as `null`, which `/getIndexStatus` does — measured against
8.0 for an index in `Scheduled for Creation`, which nulls both `partitionMap`
and `alternateShardIds`. The difference between the two is the difference
between "this index is not ready" and a parse failure, and the blast radius is
not one index: a topology refresh reads every index in the cluster, so one index
mid-creation made the whole response unreadable.

`indexerx::status` therefore carries **both** attributes on every optional
field — `#[serde(default, deserialize_with = "null_as_default")]` — because they
cover different cases. `defn_id` is the one field left required, deliberately:
an index status with no id addresses nothing.

### There is no `indexScanSSL`, and the SSL port set repeats the plain one

Measured during the extraction against 8.0.3: both `/pools/default/nodeServices`
and `/pools/default/b/<bucket>` advertise `indexScan`, `indexHttp` and
`indexHttps` per node, and no fourth index port. So `ConfigParser` copies
`index_scan` into the SSL set unchanged — a TLS scan is that same port with TLS
on top of it, and a `None` there would make a TLS-only cluster's indexer
unreachable. The reasoning and the four tests that pin it live at the site
(`configparser.rs`), which is where the join is; this is the pointer, not a
second copy.

What is **not** verified, and cannot be from here: whether port 9101 actually
accepts TLS on an encryption-strict cluster. The extraction's cluster is
non-TLS. The claim is an inference from the absent field, exercised end to end
only against a loopback TLS server.

### A rebalance in flight is a normal answer

**Recorded in cbcore-rs.** A partition that `/getIndexStatus` does not currently
place, or places on a host that then refuses the scan, is what a rebalance looks
like from here — not a broken response. cbcore-rs's router refreshed and
retried, and the next `/getIndexStatus` was what resolved it. couchbase-core has
no router yet; this is the behaviour the one that lands should have.
