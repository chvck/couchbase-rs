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

//! A streaming scan, and the connection it owns while it runs.
//!
//! ### The rule the whole module exists for
//!
//! **A connection is reusable only after its stream has ended.** Queryport has
//! no request ids, so ordering is the correlation: a client that stops reading
//! mid-scan and issues another request on the same connection reads the
//! previous scan's rows as its own. That is a wrong answer, silently, and it is
//! the failure this module is shaped to prevent.
//!
//! Ownership is how it is prevented rather than documentation being how.
//! [`Client::scan`](super::client::Client::scan) takes the client **by value**
//! and puts it inside the [`ScanStream`], and only [`ScanStream::finish`] gives
//! it back — after draining. A caller that simply drops the stream drops the
//! connection with it, which wastes a socket and cannot corrupt anything.
//!
//! ### Why `finish` and not `Drop`
//!
//! Draining is asynchronous and `Drop` is not. The Go client resolves that by
//! spawning a goroutine per abandoned scan; the Rust equivalent would need a
//! runtime handle in the type and would move the failure — a drop with no
//! reactor running — somewhere nobody is looking. An explicit, awaitable
//! `finish` puts the cost where the caller can see it, and makes "I want this
//! connection back" a thing you say rather than a thing you hope for.

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;

use super::client::Client;
use super::error::{Error, ErrorKind};
use super::packet_codec::Frame;
use super::proto::{self, ParseError, Response, ScanEntry};

/// Where a stream is in its lifecycle.
///
/// The three terminal states are distinct because they decide different things
/// about the *connection*, which is what the caller actually needs to know:
///
/// - `Ended` — the terminator arrived. Hand the connection straight back.
/// - `Refused` — the indexer reported an error inside a well-formed packet. The
///   scan is over, but the server always writes its terminator right after such
///   an error (`scan_coordinator.go`'s `tryRespondWithError`), so the
///   connection is recoverable once that frame is read.
/// - `Failed` — the socket or the framing broke. Nothing can be trusted about
///   what comes next, so the connection is discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Streaming,
    Ended,
    Refused,
    Failed,
}

/// The entries of one scan, from one indexer node.
///
/// Yields `None` at the end of the stream. An error is yielded once and ends
/// the stream — there is nothing after it, because a scan that failed part way
/// through has no more rows to give and the connection it was using is
/// suspect.
pub struct ScanStream {
    client: Option<Client>,
    /// Entries decoded from the last packet but not yet yielded. The wire
    /// batches rows and the `Stream` contract is one at a time.
    buffered: VecDeque<ScanEntry>,
    state: State,
    read_units: Option<u64>,
}

impl ScanStream {
    pub(super) fn new(client: Client) -> ScanStream {
        ScanStream {
            client: Some(client),
            buffered: VecDeque::new(),
            state: State::Streaming,
            read_units: None,
        }
    }

    /// The read units this scan consumed, once it has ended.
    ///
    /// `None` before the stream ends, and on a cluster that does not meter.
    pub fn read_units(&self) -> Option<u64> {
        self.read_units
    }

    /// Whether the scan reached its end, as opposed to being abandoned or
    /// having failed.
    pub fn is_ended(&self) -> bool {
        self.state == State::Ended
    }

    /// Recover the connection **without awaiting**, if the stream already
    /// ended of its own accord.
    ///
    /// A stream that reached its terminator has nothing left to drain, so the
    /// connection is immediately reusable — and that is the case a `Drop` can
    /// handle, where [`finish`](Self::finish) cannot because draining is
    /// asynchronous. Returns `None` in every other state, including `Ended`
    /// twice.
    pub fn take_if_ended(&mut self) -> Option<Client> {
        if self.state == State::Ended {
            self.client.take()
        } else {
            None
        }
    }

    /// End the scan and recover the connection.
    ///
    /// Drains whatever the server still has to send — first asking it to stop,
    /// if the stream is still running — and returns a connection that is safe
    /// to reuse.
    ///
    /// Returns `Err` when the connection cannot be trusted afterwards, in which
    /// case it is dropped rather than returned. A caller pooling connections
    /// should treat that as "this one is gone", not as "this scan failed" — the
    /// scan's own failure was already yielded by the stream.
    pub async fn finish(mut self) -> Result<Client, Error> {
        let mut client = match self.client.take() {
            Some(client) => client,
            // Already finished once. Nothing to drain and nothing to give back.
            None => return Err(Error::new_unexpected_eof_error()),
        };

        match self.state {
            State::Ended => return Ok(client),
            State::Failed => return Err(Error::new_unexpected_eof_error()),
            // The server is already ending the stream of its own accord, so
            // asking it to stop would be a *new* request on a connection that
            // still owes us a terminator — which is how a desynchronised
            // connection gets made. Drain only.
            State::Refused => {}
            State::Streaming => {
                // Ask the server to stop. Packets may already be in flight, so
                // this stops the source and the loop below absorbs the rest.
                client.send_end_stream().await?;
            }
        }

        loop {
            match client.read_frame().await? {
                Frame::EndOfResponse => return Ok(client),
                Frame::Payload(payload) => match proto::decode_response(payload) {
                    // Rows we asked not to receive, arriving because they were
                    // already on their way. Discarded, not an error.
                    Ok(Response::Entries(_)) => continue,
                    Ok(Response::StreamEnd { read_units }) => {
                        self.read_units = read_units;
                        return Ok(client);
                    }
                    Ok(other) => {
                        return Err(ParseError::UnexpectedMessage {
                            expected: "StreamEndResponse",
                            got: other.kind(),
                        }
                        .into());
                    }
                    // A server-side error during the drain still ends the
                    // stream, so the connection is fine even though the scan
                    // was not. Keep draining until the terminator.
                    Err(e) if matches!(e.kind(), ErrorKind::Server(_)) => continue,
                    Err(e) => return Err(e),
                },
            }
        }
    }
}

impl Stream for ScanStream {
    type Item = Result<ScanEntry, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            if let Some(entry) = this.buffered.pop_front() {
                return Poll::Ready(Some(Ok(entry)));
            }

            if this.state != State::Streaming {
                return Poll::Ready(None);
            }

            let client = match this.client.as_mut() {
                Some(client) => client,
                None => return Poll::Ready(None),
            };

            let frame = match client.poll_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(Some(frame))) => frame,
                // The socket closed without a terminator. The rows we already
                // yielded may be a prefix of the answer rather than the whole
                // of it, so this is an error and not an end.
                Poll::Ready(Ok(None)) => {
                    this.state = State::Failed;
                    return Poll::Ready(Some(Err(Error::new_unexpected_eof_error())));
                }
                Poll::Ready(Err(e)) => {
                    this.state = State::Failed;
                    return Poll::Ready(Some(Err(e)));
                }
            };

            let payload = match frame {
                // Servers told a client version below 7.6 terminate with this
                // instead of a StreamEndResponse. We report a current version
                // so we should never see it — but treating it as the end is
                // both correct and free, and the alternative is a spurious
                // failure against an older cluster.
                Frame::EndOfResponse => {
                    this.state = State::Ended;
                    return Poll::Ready(None);
                }
                Frame::Payload(payload) => payload,
            };

            match proto::decode_response(payload) {
                Ok(Response::Entries(entries)) => {
                    // An empty batch is legal and means nothing; loop round and
                    // read the next frame rather than yielding a spurious end.
                    this.buffered.extend(entries);
                }
                Ok(Response::StreamEnd { read_units }) => {
                    this.read_units = read_units;
                    this.state = State::Ended;
                    return Poll::Ready(None);
                }
                Ok(other) => {
                    this.state = State::Failed;
                    return Poll::Ready(Some(Err(ParseError::UnexpectedMessage {
                        expected: "ResponseStream",
                        got: other.kind(),
                    }
                    .into())));
                }
                // The indexer said no, inside a well-formed packet. The scan
                // is over but the connection is not: its terminator is the
                // next frame, so `finish` can still recover it.
                Err(e) if matches!(e.kind(), ErrorKind::Server(_)) => {
                    this.state = State::Refused;
                    return Poll::Ready(Some(Err(e)));
                }
                Err(e) => {
                    this.state = State::Failed;
                    return Poll::Ready(Some(Err(e)));
                }
            }
        }
    }
}
