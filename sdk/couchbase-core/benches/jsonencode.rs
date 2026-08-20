/*
 *
 *  * Copyright (c) 2025 Couchbase, Inc.
 *  *
 *  * Licensed under the Apache License, Version 2.0 (the "License");
 *  * you may not use this file except in compliance with the License.
 *  * You may obtain a copy of the License at
 *  *
 *  *    http://www.apache.org/licenses/LICENSE-2.0
 *  *
 *  * Unless required by applicable law or agreed to in writing, software
 *  * distributed under the License is distributed on an "AS IS" BASIS,
 *  * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *  * See the License for the specific language governing permissions and
 *  * limitations under the License.
 *
 */

//! What buffer a small JSON request body should be serialised into.
//!
//! Carried over from cbcore-rs — `benches/jsonencode.rs` at commit `dbf1c0b`
//! (2026-08-17), carried on 2026-08-18 — where the body in question was a
//! `RangeScanCreate` and building it was five of the seven allocations a scan
//! create made. Custom serialisers
//! remove three of those outright — `String`s for fields that are natively a
//! `u64`, a `u32` and a `&[u8]` — and were never in question. What is in
//! question is the buffer: a presized `Vec` per call is still one allocation, so
//! reaching a KV `get`'s floor would mean reusing a buffer rather than
//! allocating one, and a thread-local costs something to reach on every call
//! whether or not it saves an allocation.
//!
//! **These variants hold the serialisation constant and vary only the buffer**,
//! because that is the question. Every one of them writes the same document
//! through the same `Serialize` impls; they differ in where the bytes land.
//!
//! # The answer is half yes, and the variants record both halves
//!
//! **Presizing is worth taking.**
//! **Reusing a buffer on top of that is not** — a per-thread buffer measured a
//! flat 1-7%, which does not buy a retention hazard, a `RefCell` re-entrancy
//! panic and a teardown edge case. The expected allocator contention under a
//! fan-out simply does not appear, and mimalloc does not help on this path
//! either.
//!
//! So **the thread-local variants below are kept as the evidence for not
//! shipping one**, not as a design under consideration. Do not read their
//! presence as an unfinished migration. The figures, the per-thread breakdown
//! and the reasoning are in `docs/allocation-costs.md`.
//!
//! ```text
//! cargo bench -p couchbase-core --bench jsonencode
//! ```
//!
//! # What changed on the way across, and what it means for the numbers
//!
//! - **Harness.** cbcore-rs ran this under `divan`; this crate's benches are
//!   `criterion`, so the entries are criterion functions. The relative ordering
//!   of the variants is what the study turns on and that is harness-independent,
//!   but the absolute nanosecond figures in `docs/allocation-costs.md` were
//!   taken under divan on a different machine. Re-run all variants together
//!   before quoting any of them; a comparison is only worth as much as its
//!   control.
//! - **Allocation counting.** cbcore-rs had `profile-alloc` and `mimalloc`
//!   features that swapped the global allocator under the bench. This crate has
//!   neither, and its allocation evidence comes from `dhat` in
//!   `tests/allocations.rs` instead. So the *allocation-event* half of the
//!   finding (`to_vec`'s two versus `with_capacity`'s one) is recorded in the
//!   study document and is not re-derived here; these entries measure time.
//! - **The document.** couchbase-core has no range scan at this commit, so the
//!   structures below no longer mirror anything in-tree; they are kept as the
//!   original measurement's subject so the figures stay comparable. The live
//!   analogue is `queryx::query_options::QueryOptionsBody`, which
//!   `queryx::query.rs` serialises with `serde_json::to_vec` — the `to_vec`
//!   variant below, not `fresh_presized`.
//! - **`serde_with`** is not a dependency here, so `#[skip_serializing_none]`
//!   is written out as `skip_serializing_if` per field. Same emitted document.

use std::cell::RefCell;
use std::hint::black_box;
use std::time::Duration;

use base64::{display::Base64Display, engine::general_purpose::STANDARD as BASE64};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use serde::{Serialize, Serializer};

// ---------------------------------------------------------------------------
// The document, with the borrowing serialisers the real change would use
// ---------------------------------------------------------------------------

/// A `u64` that has to appear in JSON as a string, written without building
/// one: `collect_str` formats straight into the serialiser's writer.
fn u64_as_str<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
    s.collect_str(v)
}

/// A `u32` collection id as lower-case hex, likewise.
fn u32_as_hex<S: Serializer>(v: &u32, s: S) -> Result<S::Ok, S::Error> {
    struct Hex(u32);
    impl std::fmt::Display for Hex {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{:x}", self.0)
        }
    }
    s.collect_str(&Hex(*v))
}

/// Bytes as base64, streamed through `Base64Display` rather than encoded into
/// a `String` first.
fn bytes_as_base64<S: Serializer>(v: &Option<&[u8]>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(v) => s.collect_str(&Base64Display::new(v, &BASE64)),
        None => s.serialize_none(),
    }
}

#[derive(Serialize)]
struct RangeJson<'a> {
    #[serde(
        serialize_with = "bytes_as_base64",
        skip_serializing_if = "Option::is_none"
    )]
    start: Option<&'a [u8]>,
    #[serde(
        serialize_with = "bytes_as_base64",
        skip_serializing_if = "Option::is_none"
    )]
    end: Option<&'a [u8]>,
}

#[derive(Serialize)]
struct SnapshotJson {
    #[serde(serialize_with = "u64_as_str")]
    vb_uuid: u64,
    seqno: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    seqno_exists: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_ms: Option<u64>,
}

#[derive(Serialize)]
struct ConfigJson<'a> {
    #[serde(serialize_with = "u32_as_hex")]
    collection: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    range: Option<RangeJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_requirements: Option<SnapshotJson>,
}

/// A representative body: an unbounded-low, 16-byte-high range with a snapshot
/// requirement, which is exactly what a gateway's collection scan sends.
fn document() -> ConfigJson<'static> {
    const KEY_MAX: &[u8] = &[0xff; 16];
    ConfigJson {
        collection: 0x1a2b,
        key_only: None,
        range: Some(RangeJson {
            start: Some(b""),
            end: Some(KEY_MAX),
        }),
        snapshot_requirements: Some(SnapshotJson {
            vb_uuid: 194_620_573_618_909,
            seqno: 41_233,
            seqno_exists: Some(true),
            timeout_ms: Some(30_000),
        }),
    }
}

/// How big the presized variants reserve. The document measures ~150 bytes;
/// this is the "covers nearly everything" figure a caller would pick, and is
/// deliberately far above it because the base64 bounds are document keys and so
/// are not strictly bounded.
const RESERVE: usize = 1024;

thread_local! {
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

// ---------------------------------------------------------------------------
// The variants
// ---------------------------------------------------------------------------

/// **The status quo.** `to_vec` starts at 128 bytes and reallocs once when the
/// document outgrows it: two allocations before the request is even built.
fn encode_to_vec() {
    let v = serde_json::to_vec(&document()).unwrap();
    black_box(v);
}

/// A fresh presized buffer per call. One allocation, no realloc.
fn encode_fresh() {
    let mut v = Vec::with_capacity(RESERVE);
    serde_json::to_writer(&mut v, &document()).unwrap();
    black_box(v);
}

/// The thread-local, reused. Zero allocations after the first call — but pays
/// a TLS lookup, a `RefCell` borrow and a `clear` every time.
///
/// **A real caller would have to copy the result out**, because the bytes have
/// to outlive the borrow to become the request body. That copy is part of the
/// cost, and leaving it out would measure something no caller could use — which
/// is why the length is what escapes here and not a borrow of the buffer.
fn encode_thread_local() {
    SCRATCH.with(|s| {
        let mut buf = s.borrow_mut();
        buf.clear();
        serde_json::to_writer(&mut *buf, &document()).unwrap();
        black_box(buf.len());
    });
}

/// The thread-local with a high-water-mark guard: if one outsized document has
/// grown the buffer past the reserve, give the memory back rather than hold it
/// for the life of the thread.
///
/// The branch is the whole cost in the common case, and this is what makes the
/// retention answerable — see the module docs on cleanup.
fn encode_thread_local_with_shrink() {
    SCRATCH.with(|s| {
        let mut buf = s.borrow_mut();
        buf.clear();
        serde_json::to_writer(&mut *buf, &document()).unwrap();
        let n = buf.len();
        if buf.capacity() > RESERVE {
            buf.shrink_to(RESERVE);
        }
        black_box(n);
    });
}

/// The TLS access alone, with no serialisation — what the thread-local costs
/// before it has saved anything.
fn tls_access_only() {
    SCRATCH.with(|s| {
        let mut buf = s.borrow_mut();
        buf.clear();
        black_box(buf.capacity());
    });
}

/// A bare allocation of the reserve size and nothing else — what the allocation
/// the thread-local exists to avoid actually costs.
fn alloc_only() {
    let v: Vec<u8> = Vec::with_capacity(RESERVE);
    black_box(v);
}

fn buffers(c: &mut Criterion) {
    let mut group = c.benchmark_group("buffer");
    group.bench_function("to_vec", |b| b.iter(encode_to_vec));
    group.bench_function("fresh_presized", |b| b.iter(encode_fresh));
    group.bench_function("thread_local_reused", |b| b.iter(encode_thread_local));
    group.bench_function("thread_local_with_shrink", |b| {
        b.iter(encode_thread_local_with_shrink)
    });
    group.bench_function("tls_access_only", |b| b.iter(tls_access_only));
    group.bench_function("alloc_only", |b| b.iter(alloc_only));
    group.finish();
}

// ---------------------------------------------------------------------------
// Under concurrency
// ---------------------------------------------------------------------------
//
// Everything above is single-threaded, where `fresh_presized` and
// `thread_local_reused` measured the same to within half a nanosecond. If that
// holds here too then the thread-local has no case: it would be buying nothing
// at the price of a retention hazard and a re-entrancy footgun.
//
// The reason to look is that a fan-out allocates from many threads at once, and
// a per-thread buffer never touches the allocator at all — so if the allocator
// is the contended resource, this is where the difference appears.

/// **Batched, one spawn per thread per sample, not a per-sample barrier.**
/// divan's `threads` mode re-synchronises every sample, which measured ~1.8 µs
/// of barrier against a 175 ns operation — a 10x jump from one thread to two and
/// then flat, which is the shape of synchronisation cost rather than of
/// contention. Batching many encodes per spawn amortises it away and lets the
/// allocator be the thing under load. The same reasoning applies to criterion,
/// so the batching is kept: each sample is `threads` x `BATCH` encodes, and the
/// per-encode figure is that divided by the product.
const BATCH: usize = 10_000;

/// The thread counts swept. cbcore-rs reported 1, 4 and 16 in the study; 2 and 8
/// are measured because the *shape* of the curve is the evidence — allocator
/// contention would widen the gap monotonically with threads, and it does not.
const THREADS: [usize; 5] = [1, 2, 4, 8, 16];

fn batched(c: &mut Criterion) {
    let mut group = c.benchmark_group("batched");
    // A sample is BATCH encodes per thread, so criterion's per-iteration figure
    // is already the batch. Declaring the throughput lets it report per-encode.
    for threads in THREADS {
        group.bench_with_input(
            BenchmarkId::new("fresh_presized", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    std::thread::scope(|scope| {
                        for _ in 0..threads {
                            scope.spawn(|| {
                                for _ in 0..BATCH {
                                    encode_fresh();
                                }
                            });
                        }
                    })
                })
            },
        );
        group.bench_with_input(
            BenchmarkId::new("thread_local", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    std::thread::scope(|scope| {
                        for _ in 0..threads {
                            scope.spawn(|| {
                                for _ in 0..BATCH {
                                    encode_thread_local();
                                }
                            });
                        }
                    })
                })
            },
        );
    }
    group.finish();
}

criterion_group!(
    name = benches;
    // The single-encode variants are ~200 ns and the differences under test are
    // single-digit nanoseconds, so the default three-second window is not enough
    // to separate them from whatever else the machine is doing.
    config = Criterion::default()
        .sample_size(100)
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(10));
    targets = buffers, batched
);
criterion_main!(benches);
