# Documentation

Most of this crate's documentation is in the source, as `//!` module headers and
`///` items. That is deliberate, and this directory is the narrow exception.

> **Provenance.** This directory, and the rule below, came from cbcore-rs —
> `docs/` at commit `dbf1c0b` (2026-08-17) — carried across during the extraction
> of that repository into couchbase-core on 2026-08-18. cbcore-rs is being
> archived; these studies are the part of it that is not reconstructable from
> code. Each document says at its head what it recorded there, and what
> couchbase-core does differently.

## Where a piece of documentation belongs

**In the code, at the site — the default.** Anything answering "why is *this*
the way it is" belongs on the thing it describes: why `httpx::decoder`'s
`MAX_PIN_RATIO` copies a small value instead of slicing it, why every optional
field in `indexerx::status` carries `deserialize_with = "null_as_default"` when
it already has `#[serde(default)]`, why every abandoned bootstrap path has to
drain through `discard_op_with_deadline` instead of dropping its pending op.
That question gets asked while reading the line, by someone who will otherwise
"fix" it, and a comment is the only form of documentation that gets reviewed in
the same diff as the code it describes.

This includes rejected alternatives. The reason `benches/jsonencode.rs` keeps
its thread-local variants is that they are the evidence for not shipping one,
and that evidence is worth most next to the buffer it is about.

**Here in `docs/`, cited from the code — for cross-cutting empirical studies.**
When a measurement is the reason more than one module is shaped the way it is,
restating it in each of them is how you end up with three versions of a number.
That already happened once in cbcore-rs: the idle-connection control for the
two-manager split was written down as both 216 µs and 218 µs in the same
session, across three files, one of them contradicting its own table eight lines
later.

A study also has parts no code comment has a good home for — the cluster it ran
on, how the data was seeded, the runs whose spread you have to see before
trusting a median, the row that is no longer reproducible and why. So each study
gets one home, and the code keeps the conclusion and the link.

**In `docs/scratch/`, and gitignored — for work in progress.** Plans, task
breakdowns, measurements not yet confirmed, open questions. Nothing there needs
to outlive the branch.

## Contents

| Document | What it is |
|---|---|
| [architecture.md](architecture.md) | The layering, the orchestration stack, and the conventions that follow from them |
| [cluster-topology.md](cluster-topology.md) | How the client learns the cluster's shape, and what may and may not be trusted about it |
| [connection-tuning.md](connection-tuning.md) | Why cbcore-rs grew two connection managers. The sweeps, the probes, and the two things tried first — kept because the operations that provoked it are being ported |
| [allocation-costs.md](allocation-costs.md) | What an operation costs in allocations, how that is measured without the measurement being meaningless, and what was decided against |
| [response-correlation.md](response-correlation.md) | How a memcached reply finds its waiter, and the measurement that rejected a lock-free slot table |
| [row-streaming.md](row-streaming.md) | Slicing query rows out of their chunk: what it bought, what it did not, and the retention regression that forced a size threshold |

Two of these — `connection-tuning.md` and `allocation-costs.md` — carry figures
measured against cbcore-rs, a codebase that no longer exists. They are kept
verbatim, including the negative results, because a measurement's value does not
expire with its repository, and because re-deriving one costs a day. Every such
figure is labelled with where it was taken, and every place couchbase-core does
something different is said in the document rather than quietly edited out.

## Reading a carried document

Three labels are used consistently, and mean different things:

- **Recorded in cbcore-rs** — measured there, against code that is not here.
  Trustworthy as history, not as a prediction about this crate.
- **Superseded** — the conclusion was re-tested during the extraction and did not
  hold. The original claim is left in place with the correction beside it,
  because a claim silently deleted is a claim someone will make again.
- **Not taken here** — couchbase-core does something else. The document says
  what, without arguing that either choice was wrong.

## Working on a branch

`docs/scratch/` is ignored, so create it freely and write whatever the work
needs. When the branch is ready, each note in it has one of three ends:

- **The work landed** — fold the durable part into the code at the site, or into
  a study here if more than one module depends on it, and delete the note. What
  goes is the arc: "this used to be X", "the header first claimed Y", "the figure
  was 12.0 when first measured" as narration rather than as a recorded result.
  The finished state is what the next reader needs; git history holds the rest.
- **The work is unfinished** — leave the note in scratch. It does not belong in a
  committed document, and deleting it loses the only record.
- **The work was abandoned** — if there is a lesson in *why*, that lesson is
  durable and belongs at the site of whatever was kept instead. The plan that
  produced it is not.
