# Query row streaming

What it costs to hand a query row to a caller, what slicing it out of its chunk
bought, and the retention regression that forced a size threshold.

> **Provenance.** Written on 2026-08-18 during the extraction of cbcore-rs into
> couchbase-core. Unlike the other studies here the measurements are
> couchbase-core's own, taken against this crate's code. It exists because the
> conclusion that was *predicted* and the conclusion that was *measured* are
> different, and the difference is the useful part.
>
> **Superseded.** Zero-copy row streaming was sold as "the largest per-row win"
> on the query path. **Wall-clock time did not move.** The gain is heap-only. Do
> not quote the original claim; the numbers are below.

## The two harnesses

| File | Question |
|---|---|
| `benches/query_rows.rs` | How long does draining a megabyte of rows take? `cargo bench -p couchbase-core --bench query_rows` |
| `tests/row_retention.rs` | What does a *held* row keep alive? `cargo test -p couchbase-core --test row_retention --features dhat-heap` |

Both replay the same synthetic response through the real decoder, scanner and
row streamer, built by `tests/common/rowdata.rs` — shared deliberately, so the
two measure the same bytes. The response is chunked at 16 KB, in the middle of
the 8–64 KB range reqwest hands out, and each chunk is `Bytes::copy_from_slice`d
rather than sliced out of one buffer, because slicing would make every chunk
share a lifetime and hide exactly what the retention test is looking for.

There is no network in either. That is the point: the row path is what is under
test, and a real query would bury it.

## What did not happen

Run back-to-back under matched machine load, **all four criterion cases were
within noise**. The largest apparent change was −4.4% at p = 0.05; everything
else came in at p > 0.5.

The reason is that the byte-at-a-time DFA that finds row boundaries dominates
this path at roughly 350 MB/s. Copying a megabyte is a few percent of the work
next to scanning it.

**Earlier runs appearing to show −45% to −60% were an artifact and were
discarded rather than reported.** They compared against a baseline captured at
load average 25 versus 10. This is the reason `benches/query_rows.rs` sets a
15-second measurement window and a 3-second warm-up with a comment saying why: a
megabyte of rows parses in single-digit milliseconds, and the default window
leaves the result at the mercy of whatever else the machine is doing. Run the
before and after in the same session, on the same machine, or do not run them.

## What did happen

The heap figures are the whole win, and they are large. Draining 8 KB rows:

| | allocations | bytes |
|---|---|---|
| before | 257 | 1,102,151 |
| after | **192** | **545,375** |

The staging buffer and its per-refill `copy_within` slide are gone for every row
shape, not just large ones.

## The regression it nearly shipped, and the threshold that bounds it

Handing a row out as a slice of its chunk costs nothing to produce — and holds
its whole chunk for as long as the caller holds it. That is a bargain for a row
that is most of its chunk and a disaster for one that is a hundredth of it.

Measured with a 1 MB response, 16 KB chunks, a caller keeping one row in a
hundred, 64-byte rows:

| | retained | ratio to payload |
|---|---|---|
| before (everything copied) | 15,708 B for 9,564 B payload | 1.6× |
| borrow everything | **981,752 B** | **102.7×** |
| threshold at ratio 4 | 17,756 B | 1.9× |

The 102.7× row is the predicted regression, confirmed. A caller keeping a
handful of small rows would have held the entire response.

`httpx::decoder`'s `MAX_PIN_RATIO = 4` is the fix: a value is sliced only when it
is at least a quarter of its chunk, and copied otherwise. That caps what a held
row can retain at four times its own length, whatever size the transport's chunks
happen to be. A value straddling two chunks is always copied.

**The threshold is not free.** A caller materialising a whole large-row result
set now retains 50% more: 1,051,250 B → 1,569,686 B for 8 KB rows. That is the
price of bounding the small-row case, and it is the right way round — the
unbounded case is unbounded.

So the accurate form of the original claim is: **zero allocations and zero copies
for rows at or above a quarter of a chunk; below that, copying is correct and the
numbers prove it.**

## Two loose ends

**The test and the constant are not linked.** `MAX_PIN_RATIO` is private to
`httpx::decoder`, and `tests/row_retention.rs` asserts `retained <= payload * 4`
with its own literal `4`. They agree today by inspection, not by construction.
Anyone changing the constant has to remember the test; the test will not remind
them, it will just start passing loosely or failing.

**`draining_a_response_does_not_copy_every_row` asserts nothing.** It measures
and prints bytes allocated per byte of rows. That is deliberate — the useful
form of that number is a trend across changes, not a threshold — but it means a
regression there is silent unless someone reads the output.

**analyticsx has no integration test.** The analytics response reader changed
alongside the query one and is compile-verified plus shared unit tests only.
Nothing in this crate exercises it against a server.

## Related studies

- [allocation-costs.md](allocation-costs.md) — the KV side of the same question,
  and the harness rules both suites follow.
