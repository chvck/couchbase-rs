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

//! The query row path, with the network taken out of it.
//!
//! A synthetic query response is replayed as fixed-size chunks — the shape
//! reqwest hands us — through the same decoder, scanner and row streamer a real
//! response goes through. Each row shape is measured drained-and-dropped and
//! drained-and-held, because only the held case can retain anything.
//!
//! **The study these entries belong to is `docs/row-streaming.md`** — what
//! slicing rows out of their chunk bought (heap, not wall-clock), the 102.7x
//! retention regression it nearly shipped, and the size threshold that bounds
//! it. Read that for the conclusions; this file and `tests/row_retention.rs`
//! are how they are reproduced.

use std::time::Duration;

use bytes::Bytes;
use couchbase_core::httpx::decoder::Decoder;
use couchbase_core::httpx::raw_json_row_streamer::{RawJsonRowItem, RawJsonRowStreamer};
use criterion::{criterion_group, criterion_main, Criterion};

#[path = "../tests/common/rowdata.rs"]
mod rowdata;

use rowdata::{chunked, response_body, ChunkStream, LARGE_ROW_BYTES, SMALL_ROW_BYTES};

/// Same total body for both shapes, so they differ only in row count.
const BODY_BYTES: usize = 1024 * 1024;

/// Drains every row, keeping only its length.
async fn drain(chunks: Vec<Bytes>) -> usize {
    let mut streamer = RawJsonRowStreamer::new(Decoder::new(ChunkStream::new(chunks)), "results");
    streamer.read_prelude().await.unwrap();

    let mut total = 0;
    while let Some(item) = streamer.next().await {
        match item.unwrap() {
            RawJsonRowItem::Row(row) => total += row.len(),
            RawJsonRowItem::Metadata(_) => break,
        }
    }
    total
}

/// Drains every row and holds all of them, the way a caller collecting a result
/// set does.
async fn drain_holding(chunks: Vec<Bytes>) -> Vec<impl AsRef<[u8]>> {
    let mut streamer = RawJsonRowStreamer::new(Decoder::new(ChunkStream::new(chunks)), "results");
    streamer.read_prelude().await.unwrap();

    let mut rows = Vec::new();
    while let Some(item) = streamer.next().await {
        match item.unwrap() {
            RawJsonRowItem::Row(row) => rows.push(row),
            RawJsonRowItem::Metadata(_) => break,
        }
    }
    rows
}

fn rows(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    for (name, row_bytes) in [
        ("few_large_rows", LARGE_ROW_BYTES),
        ("many_small_rows", SMALL_ROW_BYTES),
    ] {
        let chunks = chunked(&response_body(row_bytes, BODY_BYTES));

        c.bench_function(&format!("{name}_dropped"), |b| {
            b.to_async(&rt).iter(|| drain(chunks.clone()))
        });

        c.bench_function(&format!("{name}_held"), |b| {
            b.to_async(&rt).iter(|| drain_holding(chunks.clone()))
        });
    }
}

criterion_group!(
    name = benches;
    // A megabyte of rows takes single-digit milliseconds to parse, so the
    // default five-second window leaves the result at the mercy of whatever
    // else the machine is doing.
    config = Criterion::default()
        .sample_size(100)
        .warm_up_time(Duration::from_secs(3))
        .measurement_time(Duration::from_secs(15));
    targets = rows
);
criterion_main!(benches);
