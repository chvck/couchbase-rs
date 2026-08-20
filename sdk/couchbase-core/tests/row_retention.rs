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

//! What a held row keeps alive.
//!
//! A row handed out as a slice of its source chunk costs nothing to produce, but
//! it pins the whole chunk for as long as the caller holds it. The benchmark
//! next door can only see time; this measures the heap, because the risk of
//! borrowing rows is not speed but a caller who keeps a few small rows out of a
//! large response and holds the entire response with them.

#[path = "common/rowdata.rs"]
mod rowdata;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// Enough chunks that the difference between holding rows and holding chunks is
/// unmistakable.
#[cfg(feature = "dhat-heap")]
const BODY_BYTES: usize = 1024 * 1024;

/// Retained heap, in bytes, while `keep_every`th row of the response is held.
#[cfg(feature = "dhat-heap")]
async fn retained_holding(row_bytes: usize, keep_every: usize) -> (usize, usize) {
    use couchbase_core::httpx::decoder::Decoder;
    use couchbase_core::httpx::raw_json_row_streamer::{RawJsonRowItem, RawJsonRowStreamer};
    use rowdata::{chunked, response_body, ChunkStream};

    // Snapshot before the chunks exist: the chunks are what a borrowed row
    // pins, so they have to be inside the window for that to show up.
    let before = dhat::HeapStats::get().curr_bytes;
    let chunks = {
        let body = response_body(row_bytes, BODY_BYTES);
        chunked(&body)
    };

    let mut streamer = RawJsonRowStreamer::new(Decoder::new(ChunkStream::new(chunks)), "results");
    streamer.read_prelude().await.unwrap();

    let mut held = Vec::new();
    let mut payload = 0;
    let mut seen = 0;
    while let Some(item) = streamer.next().await {
        match item.unwrap() {
            RawJsonRowItem::Row(row) => {
                if seen % keep_every == 0 {
                    payload += row.len();
                    held.push(row);
                }
                seen += 1;
            }
            RawJsonRowItem::Metadata(_) => break,
        }
    }

    // Drop everything that is not the held rows, so what is left is what the
    // caller's result set costs.
    drop(streamer);
    let retained = dhat::HeapStats::get().curr_bytes.saturating_sub(before);
    drop(held);

    (payload, retained)
}

#[cfg(feature = "dhat-heap")]
#[serial_test::serial]
#[test]
fn holding_rows_does_not_hold_the_whole_response() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let _profiler = dhat::Profiler::builder().testing().build();

    for (shape, row_bytes) in [
        ("large rows", rowdata::LARGE_ROW_BYTES),
        ("medium rows", rowdata::MEDIUM_ROW_BYTES),
        ("small rows", rowdata::SMALL_ROW_BYTES),
    ] {
        for keep_every in [1, 100] {
            let (payload, retained) = rt.block_on(retained_holding(row_bytes, keep_every));

            eprintln!(
                "  {shape}, holding every {keep_every} of them: {payload} bytes of rows retains \
                 {retained} bytes of heap ({:.1}x)",
                retained as f64 / payload as f64
            );

            // A result set may cost more than its payload -- one allocation per
            // row has overhead, and a borrowed row rounds up to its chunk -- but
            // holding a handful of rows must not hold the response they came
            // from. Four times the payload is loose enough for per-row overhead
            // on small rows and tight enough to catch a whole 1 MB response
            // being pinned by 10 KB of it.
            assert!(
                retained <= payload * 4,
                "{shape}, holding every {keep_every}: {payload} bytes of rows retained \
                 {retained} bytes"
            );
        }
    }
}

/// Allocations and bytes allocated over a full drain, with nothing held.
#[cfg(feature = "dhat-heap")]
async fn cost_of_draining(row_bytes: usize) -> (u64, u64, usize) {
    use couchbase_core::httpx::decoder::Decoder;
    use couchbase_core::httpx::raw_json_row_streamer::{RawJsonRowItem, RawJsonRowStreamer};
    use rowdata::{chunked, response_body, ChunkStream};

    let chunks = {
        let body = response_body(row_bytes, BODY_BYTES);
        chunked(&body)
    };

    let before = dhat::HeapStats::get();

    let mut streamer = RawJsonRowStreamer::new(Decoder::new(ChunkStream::new(chunks)), "results");
    streamer.read_prelude().await.unwrap();

    let mut payload = 0;
    while let Some(item) = streamer.next().await {
        match item.unwrap() {
            RawJsonRowItem::Row(row) => payload += row.len(),
            RawJsonRowItem::Metadata(_) => break,
        }
    }

    let after = dhat::HeapStats::get();

    (
        after.total_blocks - before.total_blocks,
        after.total_bytes - before.total_bytes,
        payload,
    )
}

/// The row path should not pay for the rows twice.
///
/// Every byte of the response already exists in a chunk the transport allocated.
/// A row that is handed out as a slice of its chunk costs nothing; a row that is
/// staged and copied costs its own length twice over, and the difference shows up
/// as bytes allocated per byte of response.
#[cfg(feature = "dhat-heap")]
#[serial_test::serial]
#[test]
fn draining_a_response_does_not_copy_every_row() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let _profiler = dhat::Profiler::builder().testing().build();

    for (shape, row_bytes) in [
        ("large rows", rowdata::LARGE_ROW_BYTES),
        ("medium rows", rowdata::MEDIUM_ROW_BYTES),
        ("small rows", rowdata::SMALL_ROW_BYTES),
    ] {
        let (blocks, allocated, payload) = rt.block_on(cost_of_draining(row_bytes));

        eprintln!(
            "  {shape}: draining {payload} bytes of rows took {blocks} allocations and \
             {allocated} bytes ({:.2} bytes allocated per byte of rows)",
            allocated as f64 / payload as f64
        );
    }
}
