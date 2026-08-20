# Allocation costs

> **Provenance.** Carried from cbcore-rs `docs/allocation-costs.md` at commit
> `dbf1c0b` (2026-08-17), adapted on 2026-08-18. The figures in **The recorded
> figures** were measured against cbcore-rs and are reproduced verbatim; they
> count something different from this crate's budgets and are not comparable with
> them — see the warning below. The buffer study in **The request body: presize,
> but do not reuse** is live: its harness was carried across as
> `benches/jsonencode.rs`.

What an operation costs in allocations, how that is measured without the
measurement being meaningless, and what was decided against.

## Where the live numbers are

`tests/allocations.rs` is the source of truth for what couchbase-core's
operations cost, and it is a **test, not a benchmark**, on purpose: these are
counts rather than timings, so they are exact and can be asserted.

| File | Question | Run with |
|---|---|---|
| `tests/allocations.rs` | What do the real operations cost, end to end? Pinned, so neither can quietly grow | `cargo test -p couchbase-core --test allocations --features dhat-heap` |
| `tests/row_retention.rs` | What does a *held* row keep alive? | `cargo test -p couchbase-core --test row_retention --features dhat-heap` |
| `benches/jsonencode.rs` | Where should a small JSON request body be serialised into? | `cargo bench -p couchbase-core --bench jsonencode` |
| `benches/agent.rs` | What does an operation cost in *time*, end to end against a cluster? | `cargo bench -p couchbase-core --bench agent` |

**The numbers themselves are deliberately not restated here.** They live in the
assertions, they move whenever the code improves, and a copy in a document is a
copy that will disagree. What this document owns is the reasoning the file
cannot hold, and the history the file has no room for.

Two things about the harness are worth stating once, because they are what makes
the numbers mean anything:

- **It asserts equality, not a ceiling.** An improvement fails the test. That is
  the point: lowering the expected count is a change to the assertion, made in
  the same commit as the improvement, so a budget cannot rot upward inside a
  ceiling that was set generously years ago.
- **The statistic is the minimum of 200 runs after 100 warm-ups**, for the reason
  in *The statistic is the minimum* below.

## The recorded figures

**Measured against cbcore-rs. Not comparable with this crate's budgets.**
cbcore-rs counted with a thread-local counter under a custom global allocator,
over a KV stack with no TLS, no compression, no error map, no orphan reporting
and no not-my-vbucket handling. couchbase-core counts *blocks* with `dhat` over a
stack that has all of those. A `get` costing "2" there and "11" here is not a
regression of nine; it is two different measurements. Quote them side by side and
someone will draw the wrong conclusion.

**A `get` was two allocations, exactly**, for a whole round trip.

**A whole-collection scan was 7.0 per vbucket and 0.02–0.03 per document.** Its
allocation cost is set by the bucket's shape and barely moves with how much data
it reads: measured at 128 vbuckets, repeatably, 920 allocations for 1000 documents
and 944 for 2000. Doubling the data added 2.6%, and the fixed term is 97% of the
total. A 1024-vbucket bucket would pay eight times that fixed term to read the
same rows, the same way it pays eight times the round trips.

### What the 7.0 was made of, and whose it was

Mostly not that crate's to spend. Of the create's three and the drain's four:

| Term | Count | Whose |
|---|---|---|
| Packet buffer + shared-representation promotion, both halves | 2 + 2 | `RequestPacket::to_owned`, paid by any packet — the same two a `get` pays |
| Presized JSON body, on the create | 1 | The client. See below |
| Unbounded mpsc channel, per continue | 2 | The streaming dispatch |

That channel was ~5.2 KB and by far the largest term by bytes: **~660 KB per
fan-out at 128 vbuckets**, allocated and dropped per continue. Two allocations
out of seven and most of the memory traffic.

**This is the one row that is a live warning rather than history.** couchbase-core
does not have a range scan yet, so nothing pays it here — but its dispatcher
already allocates an `mpsc::channel(1)` per operation, on the same pattern, and a
streaming operation will be tempted to size it up. If the scan lands with an
unbounded channel per continue, this is the term it will be paying. Measure it
when it lands.

### What the two optimisations bought

The per-vbucket figure was 12.0 as first measured. Two changes took it to 7.0:

| Change | Effect |
|---|---|
| `RangeScanCreate` stopped building its JSON body through owned `String`s | create half 7.0 -> 3.0 |
| `RangeScanContinue` stopped heap-allocating a 28-byte extras block | drain 5.2 -> 4.2 |

The before-and-after is here because the "is this worth doing" question is
answered by the size of the move, and that is not visible from the finished
state. Both changes are worth making again when the scan is ported; the second
one is the cheaper and is easy to miss, because a 28-byte extras block does not
look like it costs anything.

## Why the measurement is shaped the way it is

Three decisions, each of which the measurement is meaningless without. cbcore-rs
took all three; couchbase-core takes the second and third, and reaches the first
a different way.

### A separate test binary

Counting allocations needs a `#[global_allocator]`, which is process-wide. Cargo
builds each file in `tests/` as its own binary, so declaring one in a test file
affects that file and nothing else — no way for another test's allocations to land
in this one's counter.

couchbase-core does the same, and adds a feature gate: `dhat` is an optional
dependency and the whole of `tests/allocations.rs` is behind
`#[cfg(feature = "dhat-heap")]`, so an ordinary `cargo test` does not build a
profiler in. The cost of the gate is that a plain test run does not run these at
all, which is why the command is spelled out at the top of this document.

### A per-thread counter, and a current-thread runtime

A `get` is not one thread's work. The memcached client spawns a read loop, so the
request is encoded on one task and the response decoded by another. On a
multi-threaded runtime those land on different workers: a per-thread counter
would miss most of what it is trying to count, while a process-wide counter would
pick up every unrelated task in the runtime.

A **current-thread** runtime puts them on the thread running `block_on`, so the
counter sees the whole round trip and nothing else. That is the only reason the
measurement is well defined. Both crates use one; couchbase-core shares a single
`LazyLock<Runtime>` across the suite and serialises the tests with `#[serial]`.

### The statistic is the minimum

An agent runs background work on the same runtime: the config watcher polls every
2.5 s, and pools reconnect. On a current-thread runtime those interleave at the
operation's await points, and when they do they add allocations that are not the
operation's.

They can only ever **add**. So the minimum across many iterations is the clean run
— the one where no background task happened to be polled. A mean or a median
would measure the background work's duty cycle instead, and would drift with it.

## The request body: presize, but do not reuse

`benches/jsonencode.rs` exists to answer one question, and the answer is half
yes.

The body cbcore-rs measured was a range-scan create, and building it was five of
the seven allocations a create made: three `String`s for fields that are natively
a `u64`, a `u32` and a `&[u8]`, plus `serde_json::to_vec`'s initial 128-byte
buffer and the realloc when it outgrew it.

The three `String`s are removed by custom serialisers that format straight into
the serialiser's writer, and were never in question. What was in question is the
buffer: a presized `Vec` per call is still one allocation, so reaching zero would
mean reusing a buffer rather than allocating one.

**Presizing is worth taking.** `to_vec` starts at 128 bytes and always reallocs
for this document, so `Vec::with_capacity` + `to_writer` is 196.8 ns to 164.4 ns
and two allocation events to one.

**Reusing a buffer on top of that is not.** Per encode, median over batches of
10 000, both allocators:

| threads | glibc fresh | glibc reused | mimalloc fresh | mimalloc reused |
|---|---|---|---|---|
| 1 | 194.4 ns | 181.6 ns | 183.7 ns | 175.5 ns |
| 4 | 214.2 ns | 199.3 ns | 206.3 ns | 200.8 ns |
| 16 | 390.1 ns | 386.1 ns | 393.6 ns | 379.7 ns |

A per-thread buffer is worth a flat **1–7%**. The reason to have expected more was
allocator contention under a fan-out — which does not appear: were it real the gap
would widen with threads, and on glibc it is *narrowest* at sixteen. Both variants
degrade about equally from one thread to sixteen (194 -> 390 ns), which is the
machine rather than the allocator.

So the last allocation costs ~5–13 ns to keep, on an operation whose round trip is
50–500 µs. That does not buy a retention hazard, a `RefCell` re-entrancy panic and
a teardown edge case. **The thread-local variants in the bench are kept as the
evidence for not shipping one** — they are not a design under consideration.

> **Two caveats on that table, both about where it came from.** It was measured
> under `divan` on cbcore-rs's machine, and the two right-hand columns needed a
> `mimalloc` feature to swap the global allocator. couchbase-core's port is
> criterion and has neither that feature nor a `profile-alloc` one, so it
> reproduces the *ordering* of the variants and not these absolute figures. The
> allocation-event half of the finding — `to_vec`'s two against
> `with_capacity`'s one — is not re-derived by the bench either; it is recorded
> here. Re-run every variant together before quoting any of them.

> **This crate is on the un-presized side of it.** `queryx::query.rs` builds the
> query request body with `serde_json::to_vec(&QueryOptionsBody { .. })` — the
> `to_vec` variant in the bench, two allocation events for a document that always
> outgrows 128 bytes. `Vec::with_capacity` plus `to_writer` is a one-line change
> and the `query` budget in `tests/allocations.rs` would move by one. Nobody has
> done it, and this is the note saying it is there.

### mimalloc is not the answer here either

0–6% on this path, and nothing at sixteen threads. That was a statement about
*that* workload — one small allocation next to ~180 ns of serialisation — and not
about a gateway or any other consumer, which allocate far more variously.
couchbase-core has no `mimalloc` feature at all; the finding is recorded so that
adding one is a decision with a number attached rather than a guess.

## Related studies

- [row-streaming.md](row-streaming.md) — what a query row costs to hand out, and
  what it keeps alive. The other half of this crate's heap story.
- [response-correlation.md](response-correlation.md) — a design whose entire
  selling point was zero allocations per operation, measured and rejected anyway.
