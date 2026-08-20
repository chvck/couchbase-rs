# Response correlation

How a memcached reply finds the caller waiting for it, and the measurement that
rejected a lock-free slot table.

> **Provenance.** Written on 2026-08-18 during the extraction of cbcore-rs into
> couchbase-core, to give a rejected design's evidence a home. cbcore-rs kept
> that evidence in `benches/opaquemap.rs`; the bench itself could not be carried,
> because it benchmarks a type that does not exist in this crate. This document
> is what survives it.
>
> **Superseded.** cbcore-rs shipped the slot table on the reasoning that it costs
> zero allocations per one-shot operation. That reasoning is sound and the
> allocation claim is true. It was re-measured during the extraction and the
> design is **not faster** — the claim that mattered did not survive.

## What memdx has to do

Many operations are in flight on one socket, correlated by the four-byte opaque
field the server echoes. Something has to hold, for each outstanding opaque, the
channel to wake; hand a response to it; and guarantee that an abandoned operation
does not leave an entry behind — while a reader task writes into that structure
from a different thread from every caller.

## What couchbase-core does

A `HashMap<u32, SenderContext>` behind a plain `std::sync::Mutex`, in an `Arc`
(`memdx::client`). The opaque is a `SeqCst` `AtomicU32` counter incremented
*inside* that same lock, so the atomic is a monotonic counter rather than
lock-free work.

- **One response channel per operation**: `mpsc::channel(1)`, created at dispatch.
- **One spawned task**, the read loop. Writes are done inline by the calling task
  under a `tokio::sync::Mutex` on the framed writer, so there is no writer task
  and no shared packet queue. The mutex on the writer *is* the serialisation
  point.
- **One-shot versus streaming is a `bool`.** `SenderContext::is_persistent`. The
  read loop always removes the entry and re-inserts it if persistent, so "not
  registered" and "already completed" are the same lookup miss. Nothing in-tree
  passes `true` yet — the capability is plumbed and unused, because there is no
  DCP, range-scan or observe opcode here.
- **Leaks are prevented by three RAII paths and a drain**, not by a state word:
  `ClientPendingOp::Drop`, a `DispatchOpaqueGuard` covering the window between
  registering an opaque and the write succeeding, and `drain_opaque_map` on
  close. A miss with no entry goes to the orphan handler if one is installed.
- **No generation tag, and no ABA protection.** A wrapped-around `u32` opaque
  colliding with a live entry would be silently overwritten. The design relies on
  the 2³² opaque space vastly exceeding the in-flight window. In exchange there
  is no fixed ceiling: the map grows.
- **No `unsafe`.**

The counterpart rule is the one thing here that catches people out: **dropping a
dispatched operation unregisters its opaque**, so the reply already on its way
back arrives with nowhere to go and is logged as an orphan. Any path that
abandons an operation it has already written must go through
`discard_op_with_deadline`, which reads the response and throws it away. That is
what the pipelined bootstrap's every abort branch does, and there is a test
asserting it.

## What cbcore-rs did instead

`memdx::opaquemap::OpaqueMap`: a fixed array of **2048 slots**, each holding its
payload in an `UnsafeCell` with a packed generation-and-state word making access
to it sound, plus a hand-rolled `Future` and `Waker`. **Zero allocations per
one-shot operation** — no channel, no map entry, no boxing. The state word
distinguished one-shot from streaming.

Two costs came with it, both visible from reading:

- `expect("OpaqueMap slots exhausted!")` — a **panic** at 2048 operations in
  flight on one connection, which is a hard cap rather than backpressure.
- `unregister()` (on the caller's thread, from a drop guard) writes
  `*slot.result.get() = None` while `invoke()` (on the reader thread) may write
  `Some(..)`; the state check and the write are not atomic. A possible stranded
  slot. Recorded as a read-only finding — it was never reproduced, and it was not
  the reason the design was rejected.

## The measurement

Two independent runs, both on 2026-08-18, on an M-series darwin machine, release
build. Both compare three implementations of the same register/invoke/await
pattern: the slot table, a `DashMap` + `oneshot`, and a `Mutex<HashMap>` +
`oneshot`.

**Run 1 — cbcore-rs's own `cargo bench --bench opaquemap`.** 8 tasks × 1250 ops,
median:

| implementation | median |
|---|---|
| `dashmap` | **662.5 µs** |
| `lockfree_slotmap` (the slot table) | 2.51 ms |
| `mutex_hashmap` | 2.095 ms |

That bench constructs a fresh map inside the timed region, and an `OpaqueMap` is
2048 slots and 4096 mutexes where a `DashMap` is nearly nothing — so the result
was not trusted on its own.

**Run 2 — a standalone harness with construction hoisted out of the timed
region.** 30 rounds × 10 000 ops × 8 tokio tasks, multi-threaded runtime, median:

| implementation | median |
|---|---|
| `lockfree_slotmap` (cbcore-rs's `OpaqueMap`) | 2586 µs |
| `dashmap` | **839 µs** |
| `mutex_hashmap` | 2635 µs |
| `OpaqueMap::new()`, one-off | 19.53 µs (`DashMap::new()`: 522 ns) |

**Construction bias is not the explanation.** The two runs are different
harnesses with different op counts, so their absolute figures are not comparable
row for row — what settles it is that constructing an `OpaqueMap` costs 19.53 µs
once, against gaps measured in milliseconds. It cannot account for them.

What both runs agree on is the shape: the slot table is roughly 3× `DashMap`, and
within a couple of percent of `Mutex<HashMap>`. Note that the slot/mutex ordering
*flips* between the two runs — slower in run 1, faster in run 2. That is what a
tie looks like, and it is the reason the conclusion below is stated as a tie
rather than as a winner.

## The conclusion, stated precisely

The slot table is **~3.1× slower than `DashMap` + `oneshot`**, and
**statistically tied with `Mutex<HashMap>` + `oneshot`** — 2586 µs against
2635 µs, a 2% difference across implementations that differ completely.

Read carefully, that is not "the alternative couchbase-core keeps is 3.1×
faster". It is not. couchbase-core keeps the `Mutex<HashMap>`, and the slot table
matches it. The case against carrying the slot table is therefore:

- it buys **nothing** in wall-clock over what is already here;
- it costs `unsafe`, a hand-rolled `Future`, a 2048-operation hard cap that
  panics, and an unresolved race between two of its own methods;
- and its one real advantage — zero allocations per operation — is worth less
  than it sounds against a round trip measured in tens of microseconds. The same
  argument, with numbers, is in
  [allocation-costs.md](allocation-costs.md#the-request-body-presize-but-do-not-reuse):
  removing the last allocation from a hot path bought 1–7% there and was also
  declined.

The allocation side of the trade, for completeness: during the pre-extraction
assessment couchbase-core's dispatch measured ~6432 B + 512 B per operation for
the response channel, against the slot table's zero. That figure predates the
pipelined bootstrap and the unboxed dispatcher, so it is history, not a current
budget; `tests/allocations.rs` is the current one.

## The finding this leaves open

**`DashMap` measured 3.1× faster than the structure this crate uses**, on the
same harness, in both runs. Nobody has pursued it, and this is the note saying it
is there. Four caveats before anyone does:

1. It is a microbenchmark. `register` is followed immediately by `invoke` with no
   network in between, so lock contention is the *whole* of the measured work. On
   a real connection the round trip is 50–500 µs and this structure is a few
   hundred nanoseconds of it.
2. `dashmap` is not currently a dependency, and the crate has no other use for
   it.
3. Contention here is a function of how many operations are in flight per
   connection, and the pool default is one connection per endpoint with no
   concurrency cap — so the contended case is reachable, but it is not the
   default shape of a small workload.
4. The `Mutex<HashMap>` also serialises the opaque counter, which `DashMap`
   would not; part of the gap may be that rather than the map.

The honest summary is that this is a candidate for measurement under a realistic
load, not a change with evidence behind it. What the evidence *does* settle is
that the lock-free slot table was not the answer.
