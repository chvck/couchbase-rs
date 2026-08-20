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

//! A synthetic query response, replayed in chunks.
//!
//! Shared by the row-path benchmark and the row-retention test so both measure
//! the same bytes. Two row shapes pull in opposite directions: a few large rows,
//! where per-row work dominates, and many small rows, where a row that borrows
//! from its chunk keeps the whole chunk alive.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{BufMut, Bytes, BytesMut};
use couchbase_core::httpx::error::Result as HttpxResult;
use futures_core::Stream;

/// Size the response is replayed in.
///
/// reqwest hands out whatever the transport gave it, typically 8-64 KB. A fixed
/// size in the middle of that keeps runs comparable.
pub const CHUNK_SIZE: usize = 16 * 1024;

/// Roughly the payload of a document-shaped row.
pub const LARGE_ROW_BYTES: usize = 8 * 1024;

/// Roughly a document-shaped row from a narrow collection.
pub const MEDIUM_ROW_BYTES: usize = 1024;

/// Roughly a counted or single-field projection.
pub const SMALL_ROW_BYTES: usize = 64;

/// Replays a pre-chunked body, so nothing but the parse is measured.
pub struct ChunkStream {
    chunks: Vec<Bytes>,
    next: usize,
}

impl ChunkStream {
    pub fn new(chunks: Vec<Bytes>) -> Self {
        Self { chunks, next: 0 }
    }
}

impl Unpin for ChunkStream {}

impl Stream for ChunkStream {
    type Item = HttpxResult<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.next >= self.chunks.len() {
            return Poll::Ready(None);
        }
        let chunk = self.chunks[self.next].clone();
        self.next += 1;
        Poll::Ready(Some(Ok(chunk)))
    }
}

/// A query response of about `body_bytes`, whose rows are `row_bytes` each.
pub fn response_body(row_bytes: usize, body_bytes: usize) -> Bytes {
    // Enough to leave room for the id and the punctuation around it.
    let filler_len = row_bytes.saturating_sub(24);
    let filler = "x".repeat(filler_len);

    let mut body = BytesMut::with_capacity(body_bytes + 512);
    body.put_slice(
        br#"{"requestID":"e5f4a1c9-0000-0000-0000-000000000000","signature":{"*":"*"},"results":["#,
    );

    for i in 0..(body_bytes / row_bytes) {
        if i > 0 {
            body.put_slice(b",");
        }
        body.put_slice(format!(r#"{{"id":{i},"v":"{filler}"}}"#).as_bytes());
    }

    body.put_slice(
        br#"],"status":"success","metrics":{"elapsedTime":"1ms","executionTime":"1ms","resultCount":1,"resultSize":1}}"#,
    );

    body.freeze()
}

/// Splits a body into `CHUNK_SIZE` pieces, as the transport would.
///
/// Each piece is its own allocation, because that is what reqwest hands over:
/// slicing one buffer would make every chunk share a lifetime and hide exactly
/// what the retention test is looking for.
pub fn chunked(body: &Bytes) -> Vec<Bytes> {
    let mut chunks = Vec::with_capacity(body.len() / CHUNK_SIZE + 1);
    let mut offset = 0;
    while offset < body.len() {
        let end = (offset + CHUNK_SIZE).min(body.len());
        chunks.push(Bytes::copy_from_slice(&body[offset..end]));
        offset = end;
    }
    chunks
}
