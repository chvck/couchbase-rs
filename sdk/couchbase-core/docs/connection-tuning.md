# Connection tuning

> **Provenance.** Carried from cbcore-rs `docs/connection-tuning.md` at commit
> `dbf1c0b` (2026-08-17), on 2026-08-18. **Every figure below was measured
> against cbcore-rs**, on a 3-node Couchbase 8.0.3 cluster, and is reproduced
> verbatim — including the negative results, the spread, and the one row that is
> no longer reproducible. None of it has been re-run against couchbase-core, for
> the reason in the next section.
>
> **Status here: the conclusion is not implemented, and at this commit it does
> not need to be.** couchbase-core has one connection manager, a fixed-size pool
> and no `PoolShape`. It also has no operation that answers with a stream of
> packets — no range scan, no `stats` sweep, no `get_all_vb_seqnos` — which is
> the entire cause of the problem this study is about. The study is kept because
> those operations are being ported, and because the day the first one lands is
> the day this becomes a live design question rather than a historical one.

Why cbcore-rs had two connection managers rather than one tunable pool.

This was the study behind its `pool::multi_manager`, `AgentOptions::bulk_pool`,
and the dispatch split between point and streaming operations. It lived in
`docs/` rather than in any of them because all three were shaped by it, and
because a measurement has parts — the cluster, the seeding, the spread across
runs — that no single module header is the right home for.

It was reproduced with:

```bash
RCBCONNSTR=couchbase://<host> cargo bench --bench rangescan
```

`RCBSCANDOCS` set how many documents to seed (default 1000). The collection was
created once and reused, so a second run did not re-seed.

> **The harness was not carried.** `benches/rangescan.rs` drove `Agent::connect`
> with a `PoolShape` per manager and ran a whole-collection range scan;
> couchbase-core has none of those three things. Rebuilding it would mean
> inventing an API to measure. It should be re-ported alongside the range-scan
> work, at which point the sweeps below are re-runnable and this document should
> gain a second column.
>
> **The control was.** `benches/agent.rs`'s `kv/get` entry is the same
> measurement as `get_alone` below — one `get` through the whole agent with
> nothing else running. It is the floor every probe figure here is quoted
> against, and it has to be re-measured in the same session as any comparison,
> because it moves with the cluster.

## The two operation classes

A client that has both serves two kinds of work, and they are opposites in every
way that bears on tuning:

| | `get` | range scan |
|---|---|---|
| ops per logical request | 1 | one per vbucket (128–1024) |
| server-side service time | tens of µs | ms to seconds |
| blocks its connection | no | **yes** |

A whole-collection scan is one scan per vbucket. There is no pruning available,
because placement is by hash of the key rather than by key order, so the unit of
work is a fan-out of `num_vbuckets()` concurrent scans rather than an operation.

The blocking is the crux. `RangeScanContinue` returns `would_block` server-side
and finishes on a background task, and kv_engine's
`Connection::executeCommandsCallback` stops executing a connection's queue at the
first active command that may not be reordered. So a `get` queued behind a
fan-out waits for scans, not for itself.

## The sweeps

Measured on a 3-node 8.0.3 cluster, 128 vbuckets, 1000 documents. `connections`
varies how many connections the fan-out has; `per_connection` varies how deep the
client will queue on each one; `get_per_connection` does the second over an
operation that never blocks.

| | 1 | 4 | 16 | 32 | 64 | 128 |
|---|---|---|---|---|---|---|
| `connections` (median) | 78 ms | 62 ms @4 | 56 ms @16 | 55 ms | — | — |
| `per_connection` | 88 ms | 76 ms | 74 ms | 76 ms | 75 ms | 75 ms |
| `get_per_connection` | 11.6 ms | 3.2 ms | 1.2 ms | 711 µs | **468 µs** | 480 µs |

**The two classes want opposite tuning, by a wide margin.** A `get` fan-out keeps
improving to about 64 outstanding on one connection — twice the server's
`max_concurrent_commands_per_connection` of 32, which is what you would predict
from having to cover the wire as well as the server, and it does not care how
many connections there are. A scan fan-out stops improving at about 4 per
connection and wants *connections* instead: 78 ms at one to 55 ms at sixteen.

So one static `connection_concurrency` is not a setting with a good value. Set
for scans it leaves a `get` at 3.2 ms instead of 468 µs; set for gets it stacks
64 scans on a socket that carries four usefully.

> **What this says about couchbase-core's current shape.** There is no
> `connection_concurrency` here at all: `KvClientPool::get_client` is a
> round-robin over a fixed set of connections and nothing caps how many
> operations are outstanding on one. For point operations that is roughly the
> right end of the `get_per_connection` curve — unbounded depth is nearer 64 than
> 1, and the curve is flat from 64 to 128. cbcore-rs's own default was the
> opposite mistake: `connection_concurrency` defaulted to 1 with a token held for
> the whole round trip, so exactly one KV operation was in flight per node and
> everything this study measures was unreachable out of the box.
>
> The uncapped choice becomes wrong in one direction only, and it is the
> direction Track D is heading: an unbounded queue of *blocking* operations on
> one socket is the 88 ms column, and there is no knob here to stop it.

## The probes

Three entries at the bottom of cbcore-rs's `benches/rangescan.rs` measured a
single `get`'s latency directly, varying only what else is running. All in one
session:

| probe | p50 | p90 | fan-out's own median |
|---|---|---|---|
| `get_alone` — nothing else running | 216 µs | 252 µs | — |
| `get_behind_scan` — one manager¹ | 26.0 ms | 36.5 ms | 73.0 ms |
| `get_beside_scan` — two managers | **1.1–2.5 ms** | 7.9–11.5 ms | **56.3 ms** |

**A hundred and twenty times the control, down to five or six** — and the fan-out
itself 23% faster on the same change, because sixteen connections beat one. Not a
latency-for-throughput trade; both improve.

It is still 5–6× the control at p50 and 31× at p90, so a second manager does not
make a fan-out free. It stops it being a *connection* problem. What is left is on
the client's runtime and the server's, and neither is addressed by which socket a
command went out on.

### On the spread

The treatment's p50 is quoted as a range because it moves: five runs of the same
code gave 1.08, 1.39, 2.49, 1.43 and 1.64 ms. Its p90 is 8–11 ms, so a heavy tail
drags the median around, and the cluster is shared with other work.

What says the spread is the probe's tail rather than the change is that the
fan-out's own median sat at 56 ms in every one of those runs. Nothing here turns
on the third digit — the effect against 26.0 ms is better than tenfold at the
worst reading. Run the control alongside; the comparison is only worth as much as
its control.

### ¹ Why the middle row is recorded rather than reproducible

That row was measured when the bulk pool could be told to share the primary
manager's connections, which is the only way to put a `get` and a fan-out on one
socket — pool shapes alone cannot do it, because two managers are distinct
connections however they are shaped.

That option no longer existed by the end of cbcore-rs: `PoolShape` carried only
min, max, concurrency and idle timeout, every manager was minted from the
multi-manager, and a range scan always dispatched on the bulk one. So the 26.0 ms
stands as a recorded result and the live comparison is the other two rows. It is
kept because it is the number that motivated the change, and a study that drops
its own baseline is not a study.

Note that in couchbase-core, as of this commit, the *one-manager* row is the only
one that could be measured at all — it is the configuration the crate has.

## Two things tried first

Both are worth knowing about, because both look like they should work.

### `UnorderedExecution` — recovers less than half

The HELO feature is negotiated and on, and this is not an argument against it. But
it does not solve this problem.

It changes **neither sweep**. Both are homogeneous: everything queued behind a
scan is another scan of similar length, so being made to wait costs about what
being scheduled would have. Where it shows up is a `get` sharing a connection with
the fan-out:

| `get` sharing a connection with the fan-out | p50 | p90 |
|---|---|---|
| ordered | 49.8 ms | 52.4 ms |
| unordered | **26.7 ms** | 38.0 ms |
| (same `get`, idle connection) | 0.46 ms | — |

Half the damage, on a row that a second manager takes to ~1.2 ms. The reason it
cannot do better: reordering only helps among commands the server has already
*accepted*, and at the 32-command cap kv_engine calls `disableReadEvent()` and
stops reading the socket. A `get` behind 96 unread scans waits for the socket
regardless of what may be reordered.

That is the finding that made a knob the wrong shape of answer. The problem is
which socket the command went out on, and no per-command flag reaches it.

### Tuning `connection_concurrency` for both — no such value

Covered by the sweeps above; recorded here because it is the obvious first move.
The two curves have their optima an order of magnitude apart and neither is flat
near the other's. A single knob has to pick a loser.

## The conclusion, as cbcore-rs implemented it

Two managers, minted from `pool::multi_manager`:

- **`primary`** — every point operation. Shaped by the agent's connection
  settings.
- **`bulk`** — anything answering with a stream of packets: a range scan's
  continues, a `stats` sweep, `get_all_vb_seqnos`. `PoolShape::BULK_DEFAULT` was
  min 0, max 16, concurrency 4, which is where the sweeps point. Min 0 means it
  costs nothing until something asks for a connection.

Membership was decided by **response count, not opcode** — an operation that
answers with a stream holds its connection for as long as the answer takes,
whatever it is called. And it was settled in two places only: the field on the
agent and the operation's dispatch site. There was deliberately no third place to
decide it, and no public accessor, because "which manager does this use" should
follow from what the operation does.

### What that means for this crate

Nothing is required today, and something is required before the first streaming
KV operation ships. In rough order of how much they buy:

1. **A second manager, or a per-operation connection budget.** The 26.0 ms → 1.2 ms
   row is the whole case, and no amount of tuning one pool reaches it.
2. **Response count as the membership rule, not opcode.** It is the rule that
   keeps working when the next streaming operation is added.
3. **A concurrency cap of some kind on the blocking class.** Uncapped depth is
   the 88 ms column; four per connection is the 76 ms one.

None of that is an argument for adding a `PoolShape` today. It is the reason to
re-port `benches/rangescan.rs` with the range-scan work rather than after it —
this table is only useful if it can be re-run.
