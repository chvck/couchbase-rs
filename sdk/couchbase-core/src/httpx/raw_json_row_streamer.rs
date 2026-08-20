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

use crate::httpx::decoder::{Decoder, Token};
use crate::httpx::error::Error;
use crate::httpx::error::Result as HttpxResult;
use bytes::Bytes;
use futures::{stream, FutureExt, Stream, TryStreamExt};
use serde_json::Value;
use std::cmp::{PartialEq, PartialOrd};
use std::collections::HashMap;

#[derive(PartialEq, Eq, PartialOrd, Debug)]
enum RowStreamState {
    Start = 0,
    Rows = 1,
    PostRows = 2,
    End = 3,
}

pub struct RawJsonRowStreamer {
    stream: Decoder,
    rows_attrib: String,
    attribs: HashMap<String, Value>,
    state: RowStreamState,
}

pub enum RawJsonRowItem {
    /// A slice of the chunk the row arrived in, so holding it holds that chunk.
    Row(Bytes),
    Metadata(Vec<u8>),
}

impl RawJsonRowStreamer {
    pub fn new(stream: Decoder, rows_attrib: impl Into<String>) -> Self {
        Self {
            stream,
            rows_attrib: rows_attrib.into(),
            attribs: HashMap::new(),
            state: RowStreamState::Start,
        }
    }

    async fn begin(&mut self) -> HttpxResult<()> {
        if self.state != RowStreamState::Start {
            return Err(Error::new_message_error(
                "unexpected parsing state during begin",
            ));
        }

        let first = self.stream.token().await?;

        if first != Token::Delim('{') {
            return Err(Error::new_message_error(
                "expected an opening brace for the result",
            ));
        }

        loop {
            if !self.stream.more().await {
                self.state = RowStreamState::End;
                break;
            }

            let token = self.stream.token().await?;
            let key = match token {
                Token::String(s) => s,
                _ => {
                    return Err(Error::new_message_error(
                        "expected a string key for the result",
                    ));
                }
            };

            if key == self.rows_attrib.as_str() {
                let token = self.stream.token().await?;
                match token {
                    Token::Delim('[') => {
                        self.state = RowStreamState::Rows;
                    }
                    Token::Value(v) => {
                        if v.as_ref() == b"null" {
                            continue;
                        }

                        return Err(Error::new_message_error(
                            "expected an opening bracket for the rows",
                        ));
                    }
                    _ => {
                        return Err(Error::new_message_error(
                            "expected an opening bracket for the rows",
                        ));
                    }
                }

                if self.stream.more().await {
                    self.state = RowStreamState::Rows;
                    break;
                }

                // There are no rows so we can just read the remaining metadata now.
                let token = match self.stream.token().await {
                    Ok(t) => t,
                    Err(e) => return Err(e),
                };

                match token {
                    Token::Delim(']') => {}
                    _ => {
                        return Err(Error::new_message_error(
                            "expected closing ] for the result",
                        ));
                    }
                }

                self.state = RowStreamState::PostRows;
                continue;
            }

            let value = self.stream.decode().await?;
            let value = serde_json::from_slice(&value)
                .map_err(|e| Error::new_message_error(format!("failed to parse value: {e}")))?;

            self.attribs.insert(key, value);
        }

        Ok(())
    }

    pub async fn has_more_rows(&mut self) -> bool {
        if self.state != RowStreamState::Rows {
            return false;
        }

        self.stream.more().await
    }

    pub async fn read_prelude(&mut self) -> HttpxResult<Vec<u8>> {
        self.begin().await?;
        serde_json::to_vec(&self.attribs)
            .map_err(|e| Error::new_message_error(format!("failed to read prelude: {e}")))
    }

    pub fn epilog(&mut self) -> HttpxResult<Vec<u8>> {
        serde_json::to_vec(&self.attribs)
            .map_err(|e| Error::new_message_error(format!("failed to read epilogue: {e}")))
    }

    pub async fn next(&mut self) -> Option<HttpxResult<RawJsonRowItem>> {
        if self.state == RowStreamState::End {
            return None;
        }

        loop {
            if self.state == RowStreamState::PostRows {
                let token = match self.stream.token().await {
                    Ok(t) => t,
                    Err(e) => return Some(Err(e)),
                };

                let key = match token {
                    Token::String(s) => s,
                    Token::Delim('}') => {
                        self.state = RowStreamState::End;

                        let metadata = match serde_json::to_vec(&self.attribs).map_err(|e| {
                            Error::new_message_error(format!("failed to encode metadata: {e}"))
                        }) {
                            Ok(m) => m,
                            Err(e) => return Some(Err(e)),
                        };

                        return Some(Ok(RawJsonRowItem::Metadata(metadata)));
                    }
                    _ => {
                        return Some(Err(Error::new_message_error(
                            "expected a string key for the result",
                        )));
                    }
                };

                let value = match self.stream.decode().await {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };

                let value = match serde_json::from_slice::<Value>(&value) {
                    Ok(v) => v,
                    Err(e) => {
                        return Some(Err(Error::new_message_error(format!(
                            "failed to parse value: {e}"
                        ))))
                    }
                };

                self.attribs.insert(key, value);
                continue;
            }

            let row = match self.stream.decode().await {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };

            if !self.stream.more().await {
                let token = match self.stream.token().await {
                    Ok(t) => t,
                    Err(e) => return Some(Err(e)),
                };

                match token {
                    Token::Delim(']') => {}
                    _ => {
                        return Some(Err(Error::new_message_error(
                            "expected closing ] for the result",
                        )));
                    }
                }

                self.state = RowStreamState::PostRows;
            }

            return Some(Ok(RawJsonRowItem::Row(row)));
        }
    }

    pub fn into_stream(self) -> impl Stream<Item = HttpxResult<RawJsonRowItem>> {
        stream::unfold(self, |mut stream| async move {
            stream.next().await.map(|row| (row, stream))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::httpx::decoder::Decoder;
    use crate::httpx::error;
    use bytes::Bytes;
    use futures_core::Stream;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    struct ChunkStream {
        chunks: Vec<Bytes>,
        next: usize,
    }

    impl Unpin for ChunkStream {}

    impl Stream for ChunkStream {
        type Item = error::Result<Bytes>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            if self.next >= self.chunks.len() {
                return Poll::Ready(None);
            }
            let chunk = self.chunks[self.next].clone();
            self.next += 1;
            Poll::Ready(Some(Ok(chunk)))
        }
    }

    /// Splits `body` into independent `size`-byte chunks, as the transport would.
    fn chunked(body: &str, size: usize) -> ChunkStream {
        let bytes = body.as_bytes();
        let mut chunks = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let end = (offset + size).min(bytes.len());
            chunks.push(Bytes::copy_from_slice(&bytes[offset..end]));
            offset = end;
        }
        ChunkStream { chunks, next: 0 }
    }

    async fn read_all(body: &str, chunk_size: usize) -> (Vec<String>, Value) {
        let mut streamer =
            RawJsonRowStreamer::new(Decoder::new(chunked(body, chunk_size)), "results");
        streamer.read_prelude().await.unwrap();

        let mut rows = Vec::new();
        let mut metadata = None;
        while let Some(item) = streamer.next().await {
            match item.unwrap() {
                RawJsonRowItem::Row(row) => {
                    rows.push(String::from_utf8(row.to_vec()).unwrap());
                }
                RawJsonRowItem::Metadata(meta) => {
                    metadata = Some(serde_json::from_slice(&meta).unwrap());
                    break;
                }
            }
        }

        // A response with no row array at all is finished by the time the
        // prelude has been read, so its metadata comes from the epilog.
        let metadata = match metadata {
            Some(metadata) => metadata,
            None => serde_json::from_slice(&streamer.epilog().unwrap()).unwrap(),
        };

        (rows, metadata)
    }

    const RESPONSE: &str = r#"{"requestID":"abc","signature":{"*":"*"},"results":[{"a":1,"s":"one"},{"a":2,"s":"two, with a comma"},{"a":3,"s":"and a \"quote\""},[4,5,6]],"status":"success"}"#;

    /// The row bounds have to survive being fed in at any granularity: a value
    /// that straddles a chunk boundary is the one case that cannot be handed out
    /// as a slice, and every boundary is a straddle at a chunk size of one.
    #[tokio::test]
    async fn rows_survive_every_chunk_boundary() {
        let expected = vec![
            r#"{"a":1,"s":"one"}"#.to_string(),
            r#"{"a":2,"s":"two, with a comma"}"#.to_string(),
            r#"{"a":3,"s":"and a \"quote\""}"#.to_string(),
            "[4,5,6]".to_string(),
        ];

        for chunk_size in 1..=RESPONSE.len() {
            let (rows, metadata) = read_all(RESPONSE, chunk_size).await;

            assert_eq!(expected, rows, "chunked {chunk_size} bytes at a time");
            assert_eq!(
                Value::from("success"),
                metadata["status"],
                "chunked {chunk_size} bytes at a time"
            );
            assert_eq!(Value::from("abc"), metadata["requestID"]);
        }
    }

    /// The query service answers a failed statement with a null in place of the
    /// row array and the reason in an errors array beside it. Nothing but the
    /// named attribute is a row, so the errors must not be read as one.
    #[tokio::test]
    async fn a_null_rows_attribute_yields_no_rows() {
        let body = r#"{"requestID":"abc","results":null,"errors":[{"code":5000,"msg":"boom"}],"status":"fatal"}"#;

        for chunk_size in 1..=body.len() {
            let (rows, metadata) = read_all(body, chunk_size).await;

            assert!(rows.is_empty(), "chunked {chunk_size} bytes at a time");
            assert_eq!(Value::from("fatal"), metadata["status"]);
            assert_eq!(Value::from(5000), metadata["errors"][0]["code"]);
        }
    }
}
