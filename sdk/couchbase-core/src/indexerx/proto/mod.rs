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

//! The wire schema, and the boundary that keeps it here.
//!
//! `generated` is `prost` output from `query.proto` and is **private**. Nothing
//! outside this module handles a generated type, for two reasons:
//!
//! - proto2 makes every field optional at the type level even when the schema
//!   says `required`, so a generated struct can represent states the protocol
//!   cannot. Validating once, here, and handing out a domain type means no
//!   caller can be handed an illegal state to mishandle.
//! - a `ResponseStream` carries its failure in an `Option<Error>` *beside* its
//!   rows. A caller holding that struct can forget to look. A caller holding
//!   `Result<Response, Error>` cannot.
//!
//! So [`decode_response`] is the only way in, and it resolves the server's
//! error into a `Result` at the moment the bytes are parsed.
//!
//! Regenerate `generated.rs` with:
//!
//! ```sh
//! cargo run --features proto-codegen --bin gen-indexerx-proto
//! ```

#![allow(clippy::doc_markdown)]

use std::time::Duration;

use bytes::Bytes;
use prost::Message;

use super::error::{AuthCode, Error, ErrorKind, ServerError};
use super::span::Scan;

#[rustfmt::skip]
#[allow(clippy::all, clippy::pedantic)]
mod generated;

use generated as wire;

/// The protocol version this client speaks, `(major << 4) | minor` with major 0
/// and minor 1.
///
/// A server rejects a payload whose version is *higher* than its own, so this
/// is a constant we track rather than one we raise. It is checked on the way in
/// as well as set on the way out: a mismatch means the two ends disagree about
/// the schema, and the failure should be named here rather than turning up as
/// a missing field somewhere downstream.
pub const PROTOBUF_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The payload is not a well-formed `QueryPayload`.
    Undecodable(String),
    /// A `QueryPayload` with no recognised message set. Either the server sent
    /// something from the part of the schema we trimmed, or the frame is not
    /// what we think it is.
    UnknownPayload,
    /// The payload's version is not one we speak.
    UnsupportedVersion { got: u32, expected: u32 },
    /// A well-formed message, but not the one this exchange expects — an
    /// `AuthResponse` where a `CountResponse` should be. The protocol has no
    /// request ids, so ordering *is* the correlation and a surprise here means
    /// the stream is desynchronised.
    UnexpectedMessage {
        expected: &'static str,
        got: &'static str,
    },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Undecodable(e) => write!(f, "malformed QueryPayload: {e}"),
            ParseError::UnknownPayload => {
                write!(f, "QueryPayload carried no message this client recognises")
            }
            ParseError::UnsupportedVersion { got, expected } => {
                write!(f, "protocol version {got} is not the expected {expected}")
            }
            ParseError::UnexpectedMessage { expected, got } => {
                write!(f, "expected a {expected} but the server sent a {got}")
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// How the indexer should encode the key values it returns.
///
/// **`Json` is the one to use.** It costs the indexer a decode per row that
/// `CollateJson` does not, but it means a client needs no collatejson
/// implementation at all — the 4,000-line codec becomes a later, measured
/// optimization rather than a prerequisite. `CollateJson` is here because the
/// wire has the field and pretending otherwise would be a lie about the
/// protocol, not because anything sends it yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DataEncoding {
    #[default]
    Json = 0,
    CollateJson = 1,
}

impl DataEncoding {
    fn as_wire(self) -> u32 {
        self as u32
    }
}

/// Which parts of an entry the indexer should send back.
///
/// Omitting it entirely returns every key position, which is usually more than
/// a caller wants: a scan that only needs document keys should ask for none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    /// Index key positions to return, by index. Empty means none.
    pub entry_keys: Vec<i64>,
    /// Whether to return the document key.
    pub primary_key: bool,
}

impl Projection {
    /// Document keys and no key values — the ordinary scan.
    pub fn keys_only() -> Projection {
        Projection {
            entry_keys: Vec::new(),
            primary_key: true,
        }
    }

    /// Document keys plus the named key positions.
    pub fn keys_and(entry_keys: Vec<i64>) -> Projection {
        Projection {
            entry_keys,
            primary_key: true,
        }
    }
}

/// How current the index must be before it is read.
///
/// **`Session` is what a caller wanting read-your-own-writes asks for, and it
/// needs no timestamp vector.** The indexer queries the KV nodes for their
/// current sequence numbers itself and waits for a snapshot at least that
/// recent. The Go client only supplies a vector for `Session` when `Helo`
/// reported version 0 — Couchbase 4.0.x — which is not a release this client
/// supports, so that path does not exist here.
///
/// **`Query` is the same guarantee with the fetch moved to the caller.** It
/// carries the vector rather than taking it beside, because a `Query` request
/// with no vector is `Any` — `setConsistency` says so in as many words ("if
/// vector == nil, it is similar to AnyConsistency") — and that is not a state
/// worth being able to spell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Consistency {
    /// Whatever the indexer has. Fastest, and stale by an unbounded amount.
    Any,
    /// At least as recent as now, decided by the indexer. This is what N1QL
    /// calls `request_plus`.
    Session,
    /// At least as recent as the vector, decided by the caller. What N1QL calls
    /// `at_plus`, and equivalent to [`Consistency::Session`] when the vector was
    /// read at or after the request arrived. See [`ScanVector`].
    Query(ScanVector),
}

impl Consistency {
    fn as_wire(&self) -> u32 {
        match self {
            Consistency::Any => 1,
            Consistency::Session => 2,
            Consistency::Query(_) => 3,
        }
    }
}

/// One vbucket's position, as a scan vector names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanVectorEntry {
    pub vbucket: u16,
    pub seqno: u64,
    /// **Never zero.** See [`ScanVector::new`].
    pub vbuuid: u64,
}

/// Sequence numbers for a [`Consistency::Query`] scan.
///
/// ### A zero vbuuid means "do not wait", and it fails silently
///
/// The indexer compares a snapshot against this vector with `TsVbuuid.AsRecent`,
/// which skips any vbucket the *request* gave a vbuuid of zero — and skips the
/// sequence-number check with it:
///
/// ```text
/// if other.Vbuuids[i] == 0 { continue }
/// if vbuuid != other.Vbuuids[i] || ts.Seqnos[i] < other.Seqnos[i] { return false }
/// ```
///
/// So a vector of zero vbuuids is [`Consistency::Any`] under another name: every
/// scan succeeds, latency collapses, and the reads are unbounded-stale while
/// looking like a win. [`ScanVector::new`] refuses one rather than letting that
/// be measured.
///
/// **The same applies to an omitted vbucket.** The indexer allocates the
/// bucket's full vbucket count and fills only what it is sent, so anything
/// absent has vbuuid zero and is skipped. A vector that stands in for
/// `Session` therefore has to name *every* vbucket, which is why this holds a
/// dense `Vec` and not a sparse map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanVector {
    entries: Vec<ScanVectorEntry>,
}

impl ScanVector {
    /// Refuses a zero vbuuid, for the reason on the type.
    pub fn new(entries: Vec<ScanVectorEntry>) -> Result<ScanVector, &'static str> {
        if entries.iter().any(|e| e.vbuuid == 0) {
            return Err("a scan vector entry with vbuuid 0 is silently ignored by the indexer");
        }
        Ok(ScanVector { entries })
    }

    pub fn entries(&self) -> &[ScanVectorEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn as_wire(&self) -> wire::TsConsistency {
        wire::TsConsistency {
            vbnos: self.entries.iter().map(|e| u32::from(e.vbucket)).collect(),
            seqnos: self.entries.iter().map(|e| e.seqno).collect(),
            vbuuids: self.entries.iter().map(|e| e.vbuuid).collect(),
            // The indexer only checks this for `Session`, where it compares
            // against a snapshot's own crc; for `Query` it reads the vbuuids
            // directly and never looks here.
            crc64: None,
        }
    }
}

/// One index entry, as the indexer sent it.
///
/// `entry_key` is absent when the request projected no key columns, which is
/// the ordinary case for a scan that wants document keys alone. When present
/// and `dataEncFmt` was `Json`, it is a JSON array of the projected positions —
/// this layer does not interpret it, because what those positions *mean* is
/// the caller's schema and not the indexing service's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanEntry {
    pub entry_key: Option<Bytes>,
    pub primary_key: Bytes,
}

/// A parsed response, with every failure already lifted out into `Err`.
///
/// There is no error variant, and that is the point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Auth(AuthCode),
    Helo {
        version: u32,
    },
    Entries(Vec<ScanEntry>),
    Count(i64),
    /// The last packet of a stream. `read_units` is present only on a metered
    /// cluster.
    StreamEnd {
        read_units: Option<u64>,
    },
}

impl Response {
    /// The name used when a response turns up where another was expected.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Response::Auth(_) => "AuthResponse",
            Response::Helo { .. } => "HeloResponse",
            Response::Entries(_) => "ResponseStream",
            Response::Count(_) => "CountResponse",
            Response::StreamEnd { .. } => "StreamEndResponse",
        }
    }

    /// Assert the response is the one this exchange asked for.
    ///
    /// Used by the single-response operations, where anything else means the
    /// connection is out of step and must not be reused.
    pub fn expect_kind(self, expected: &'static str) -> Result<Response, Error> {
        if self.kind() == expected {
            Ok(self)
        } else {
            Err(ParseError::UnexpectedMessage {
                expected,
                got: self.kind(),
            }
            .into())
        }
    }
}

/// Decode a frame payload into a response, resolving the server's own errors
/// into `Err` as they are found.
///
/// This is the only entry point to the generated types, and the only place a
/// `protoQuery.Error` is ever read.
///
/// **It takes `Bytes`, not `&[u8]`, and that is the whole zero-copy story.**
/// `IndexEntry`'s two byte fields are generated as `Bytes` (see the `bytes`
/// configuration in `bin/gen_indexerx_proto.rs`), and prost can only hand out a
/// slice of the buffer it is decoding from when that buffer is refcounted —
/// given a `&[u8]` it copies each field into a fresh allocation instead. A
/// frame payload already arrives as [`Bytes`](crate::indexerx::Frame), so the
/// rows a scan yields are views into the socket's read buffer and nothing per
/// row is allocated.
pub fn decode_response(payload: Bytes) -> Result<Response, Error> {
    let payload =
        wire::QueryPayload::decode(payload).map_err(|e| ParseError::Undecodable(e.to_string()))?;

    if payload.version != PROTOBUF_VERSION {
        return Err(ParseError::UnsupportedVersion {
            got: payload.version,
            expected: PROTOBUF_VERSION,
        }
        .into());
    }

    // Order matters only for cost: the streaming responses are the hot ones and
    // are checked first.
    if let Some(stream) = payload.stream {
        check(stream.err)?;
        return Ok(Response::Entries(
            stream
                .index_entries
                .into_iter()
                .map(|e| ScanEntry {
                    entry_key: e.entry_key,
                    primary_key: e.primary_key,
                })
                .collect(),
        ));
    }

    if let Some(end) = payload.stream_end {
        check(end.err)?;
        return Ok(Response::StreamEnd {
            read_units: end.read_units,
        });
    }

    if let Some(count) = payload.count_response {
        check(count.err)?;
        return Ok(Response::Count(count.count));
    }

    if let Some(auth) = payload.auth_response {
        return Ok(Response::Auth(AuthCode::from(auth.code)));
    }

    if let Some(helo) = payload.helo_response {
        return Ok(Response::Helo {
            version: helo.version,
        });
    }

    Err(ParseError::UnknownPayload.into())
}

/// Lift a `protoQuery.Error` into a `Result`.
///
/// **An empty string means success.** The Go schema comments say so and the Go
/// server relies on it, so presence of the message is not the test — a check
/// that only looked for `Some` would turn every successful response on some
/// code paths into a failure with an empty message.
fn check(err: Option<wire::Error>) -> Result<(), Error> {
    match err {
        Some(e) if !e.error.is_empty() => Err(ServerError::classify(e.error).into()),
        _ => Ok(()),
    }
}

/// A request, already wrapped in its `QueryPayload`.
///
/// Opaque on purpose. The generated types do not leave this module in *either*
/// direction: a caller names what it wants through the constructors below, so
/// nothing outside has to know that `Span` is required-but-usually-empty or
/// that the payload wrapper exists at all.
pub(crate) struct Request(wire::QueryPayload);

impl Request {
    fn wrap(f: impl FnOnce(&mut wire::QueryPayload)) -> Request {
        let mut payload = wire::QueryPayload {
            version: PROTOBUF_VERSION,
            ..Default::default()
        };
        f(&mut payload);
        Request(payload)
    }

    pub(crate) fn auth(user: &str, pass: &str, client_version: u32) -> Request {
        Request::wrap(|p| {
            p.auth_request = Some(wire::AuthRequest {
                user: user.to_owned(),
                pass: pass.to_owned(),
                client_version: Some(client_version),
            });
        })
    }

    pub(crate) fn helo() -> Request {
        Request::wrap(|p| {
            p.helo_request = Some(wire::HeloRequest {
                version: PROTOBUF_VERSION,
            });
        })
    }

    /// A count over the whole of an index.
    ///
    /// `span` is `required` in the schema and is sent empty, which is what the
    /// Go client does whenever the real bounds live in `scans` — and, with no
    /// `scans` either, is how "every entry" is spelled.
    pub(crate) fn count_all(
        defn_id: u64,
        consistency: Consistency,
        partitions: Vec<u64>,
    ) -> Request {
        Request::wrap(|p| {
            p.count_request = Some(wire::CountRequest {
                defn_id,
                span: wire::Span::default(),
                cons: consistency.as_wire(),
                ts_vector: match &consistency {
                    Consistency::Query(v) => Some(v.as_wire()),
                    _ => None,
                },
                partition_ids: partitions,
                ..Default::default()
            });
        })
    }

    /// Ask the server to stop streaming.
    ///
    /// Sent to abandon a scan early. The server may still have packets in
    /// flight, so the caller must drain to the terminator afterwards — this
    /// request stops the *source*, it does not truncate what is already on the
    /// wire.
    pub(crate) fn end_stream() -> Request {
        Request::wrap(|p| p.end_stream = Some(wire::EndStreamRequest {}))
    }

    /// **Takes the options by value**, because a `ScanOptions` is built for one
    /// request and dropped with it. Borrowing meant every owned field — the
    /// request id, the spans and their bounds, the projection, the partition
    /// list, the user — was cloned on the way into the wire structs, one layer
    /// after `indexcomponent` had already cloned them to build this. Moving
    /// costs nothing and the caller had no use for them afterwards.
    pub(crate) fn scan(opts: ScanOptions) -> Request {
        Request::wrap(move |p| {
            p.scan_request = Some(wire::ScanRequest {
                defn_id: opts.defn_id,
                // Required by the schema, and empty because the real bounds
                // live in `scans`. See `count_all`.
                span: wire::Span::default(),
                distinct: opts.distinct,
                limit: opts.limit,
                cons: opts.consistency.as_wire(),
                // Set together with `cons` and from the same value, so a `Query`
                // request cannot go out without the vector that gives it its
                // meaning.
                ts_vector: match &opts.consistency {
                    Consistency::Query(v) => Some(v.as_wire()),
                    _ => None,
                },
                request_id: Some(opts.request_id),
                scans: opts.scans.into_iter().map(scan_to_wire).collect(),
                indexprojection: opts.projection.map(|p| wire::IndexProjection {
                    entry_keys: p.entry_keys,
                    primary_key: Some(p.primary_key),
                }),
                offset: Some(opts.offset),
                partition_ids: opts.partitions,
                sorted: Some(true),
                data_enc_fmt: Some(opts.data_encoding.as_wire()),
                user: opts.user,
                req_timeout: opts
                    .timeout
                    .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
                // `reverse` is deliberately not set. The indexer parses it into
                // `r.Reverse` and then never reads it, so setting it would
                // claim a capability that does not exist; a descending read
                // needs a DESC-keyed index instead.
                ..Default::default()
            });
        })
    }

    /// The encoded size, for the frame's length prefix.
    ///
    /// One traversal of the message, which a length-prefixed frame cannot avoid
    /// — but it writes nothing, so it is not the "encode to a scratch buffer to
    /// learn the length" pass it might look like.
    pub(crate) fn encoded_len(&self) -> usize {
        prost::Message::encoded_len(&self.0)
    }

    /// Encode into a buffer the caller has already reserved room in.
    ///
    /// `encode_raw` rather than `encode` because there is nothing to report: the
    /// only way prost's `encode` fails is a buffer too small for
    /// [`encoded_len`](Self::encoded_len), which the caller has just reserved.
    /// This is what `encode_to_vec` does with a `Vec`; the point of doing it
    /// here is that the buffer is the socket's, so the encoded bytes are written
    /// once instead of being built in a `Vec` and copied in.
    pub(crate) fn encode_into(&self, dst: &mut impl bytes::BufMut) {
        prost::Message::encode_raw(&self.0, dst);
    }

    /// The encoded request, as its own allocation.
    ///
    /// Only for tests that assert what went on the wire — the send path encodes
    /// through [`encode_into`](Self::encode_into) and never builds this.
    #[cfg(test)]
    pub(crate) fn encode(self) -> Vec<u8> {
        self.0.encode_to_vec()
    }
}

fn scan_to_wire(scan: Scan) -> wire::Scan {
    // `equals` wins over `filters` at the indexer, which reads it first and
    // ignores the rest. `Scan`'s constructors keep them mutually exclusive, so
    // this only has to preserve that rather than arbitrate it.
    wire::Scan {
        filters: scan
            .filters
            .into_iter()
            .map(|f| wire::CompositeElementFilter {
                low: f.low,
                high: f.high,
                inclusion: f.inclusion.as_wire(),
            })
            .collect(),
        equals: scan.equals,
    }
}

/// Everything a scan needs, in this client's vocabulary rather than the wire's.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub defn_id: u64,
    /// Echoed into the indexer's own logs, and what a support engineer
    /// correlates on. Cheap, so always set it.
    pub request_id: String,
    /// The partitions *this node* holds. Empty for a non-partitioned index.
    pub partitions: Vec<u64>,
    pub consistency: Consistency,
    /// A disjunction: several runs of entries, read as one stream.
    pub scans: Vec<Scan>,
    pub projection: Option<Projection>,
    /// Deduplicate on the **index key**, not the document key. An array index
    /// can still return one document several times with this set.
    pub distinct: bool,
    pub offset: i64,
    /// [`i64::MAX`] for unbounded, which is what
    /// [`ScanOptions::new`] uses — there is no "absent" value for it in the
    /// schema, where it is required.
    pub limit: i64,
    pub data_encoding: DataEncoding,
    /// The end user this scan runs on behalf of, for the indexer's auditing.
    /// Distinct from the credentials, which authenticate the *connection*.
    pub user: Option<String>,
    /// **A wall-clock budget for the whole scan**, measured from when the
    /// indexer accepts the request.
    ///
    /// `None` leaves the server's `indexer.settings.scan_timeout`, which
    /// defaults to **120 seconds** — and that is a *total duration*, not an
    /// idle timeout. The indexer will block indefinitely on a slow reader
    /// (measured: a 30-second stall mid-scan loses nothing), but it kills any
    /// scan still running at the deadline with "Index scan timed out",
    /// regardless of whose fault the slowness is.
    ///
    /// So a consumer that might take longer than the budget — a cursor held
    /// open across several round trips, say — must either raise this or stop
    /// holding the scan open. Setting it **replaces** the server default rather
    /// than capping it, so it can raise as well as lower.
    pub timeout: Option<Duration>,
}

impl ScanOptions {
    /// Every entry of an index, document keys only, read-your-own-writes.
    ///
    /// The defaults are the safe ends of each choice: `Session` rather than
    /// `Any`, no limit, no offset, and JSON encoding.
    pub fn new(defn_id: u64, request_id: impl Into<String>) -> ScanOptions {
        ScanOptions {
            defn_id,
            request_id: request_id.into(),
            partitions: Vec::new(),
            consistency: Consistency::Session,
            scans: vec![Scan::all()],
            projection: Some(Projection::keys_only()),
            distinct: false,
            offset: 0,
            limit: i64::MAX,
            data_encoding: DataEncoding::Json,
            user: None,
            timeout: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(f: impl FnOnce(&mut wire::QueryPayload)) -> Bytes {
        let mut p = wire::QueryPayload {
            version: PROTOBUF_VERSION,
            ..Default::default()
        };
        f(&mut p);
        p.encode_to_vec().into()
    }

    #[test]
    fn entries_decode_with_their_keys() {
        let bytes = payload(|p| {
            p.stream = Some(wire::ResponseStream {
                index_entries: vec![
                    wire::IndexEntry {
                        entry_key: Some(Bytes::from_static(b"[3,5]")),
                        primary_key: Bytes::from_static(b"doc-1"),
                    },
                    wire::IndexEntry {
                        entry_key: None,
                        primary_key: Bytes::from_static(b"doc-2"),
                    },
                ],
                err: None,
            });
        });

        let Response::Entries(entries) = decode_response(bytes).expect("decode") else {
            panic!("expected entries");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].entry_key.as_deref(), Some(&b"[3,5]"[..]));
        assert_eq!(entries[0].primary_key, Bytes::from_static(b"doc-1"));
        assert_eq!(entries[1].entry_key, None);
    }

    #[test]
    fn rows_are_views_into_the_frame_rather_than_copies() {
        // Why `IndexEntry` is generated with `bytes = "bytes"` and why
        // `decode_response` takes a `Bytes`: a scan returning a million rows
        // must not allocate twice per row on the way out of prost.
        //
        // Asserted on addresses rather than on contents, because the version
        // that copies passes every equality test in this file — which is how
        // the copy survived in the first place.
        let frame = payload(|p| {
            p.stream = Some(wire::ResponseStream {
                index_entries: vec![wire::IndexEntry {
                    entry_key: Some(Bytes::from_static(b"[3,5]")),
                    primary_key: Bytes::from_static(b"doc-1"),
                }],
                err: None,
            });
        });

        let start = frame.as_ptr() as usize;
        let end = start + frame.len();

        let Response::Entries(entries) = decode_response(frame.clone()).expect("decode") else {
            panic!("expected entries");
        };
        let row = &entries[0];
        let entry_key = row.entry_key.as_ref().expect("entry key");

        for (name, field) in [
            ("entry_key", entry_key.as_ptr() as usize),
            ("primary_key", row.primary_key.as_ptr() as usize),
        ] {
            assert!(
                (start..end).contains(&field),
                "{name} sits at {field:#x}, outside the frame at {start:#x}..{end:#x}, \
                 so prost copied it instead of slicing the buffer"
            );
        }
    }

    #[test]
    fn an_error_inside_a_stream_becomes_an_err() {
        // The whole reason the generated types stay private: a caller must not
        // be able to receive this as a success with rows it should ignore.
        let bytes = payload(|p| {
            p.stream = Some(wire::ResponseStream {
                index_entries: vec![],
                err: Some(wire::Error {
                    error: "Not my partition".into(),
                }),
            });
        });

        let err = decode_response(bytes).expect_err("expected a server error");
        let ErrorKind::Server(server) = err.kind() else {
            panic!("expected a server error, got {err:?}");
        };
        assert!(server.is_retryable_after_refresh());
        assert_eq!(server.raw(), "Not my partition");
    }

    #[test]
    fn an_empty_error_string_is_success() {
        // The schema says an empty string means success, and the server does
        // send one. Treating presence as failure would break every scan on the
        // code paths that populate it unconditionally.
        let bytes = payload(|p| {
            p.stream_end = Some(wire::StreamEndResponse {
                err: Some(wire::Error {
                    error: String::new(),
                }),
                read_units: Some(7),
            });
        });

        assert_eq!(
            decode_response(bytes).expect("decode"),
            Response::StreamEnd {
                read_units: Some(7)
            }
        );
    }

    #[test]
    fn an_error_on_the_stream_end_becomes_an_err_too() {
        let bytes = payload(|p| {
            p.stream_end = Some(wire::StreamEndResponse {
                err: Some(wire::Error {
                    error: "Index not ready for serving queries".into(),
                }),
                read_units: None,
            });
        });

        let err = decode_response(bytes).expect_err("expected index-not-ready");
        assert!(matches!(
            err.kind(),
            ErrorKind::Server(ServerError::IndexNotReady(_))
        ));
    }

    #[test]
    fn a_count_error_beats_its_count() {
        let bytes = payload(|p| {
            p.count_response = Some(wire::CountResponse {
                count: 0,
                err: Some(wire::Error {
                    error: "Index not found".into(),
                }),
            });
        });

        let err = decode_response(bytes).expect_err("expected index-not-found");
        assert!(matches!(
            err.kind(),
            ErrorKind::Server(ServerError::IndexNotFound(_))
        ));
    }

    #[test]
    fn a_wrong_version_is_named_here_rather_than_downstream() {
        let mut p = wire::QueryPayload {
            version: 2,
            ..Default::default()
        };
        p.helo_response = Some(wire::HeloResponse { version: 2 });

        let err = decode_response(p.encode_to_vec().into()).expect_err("expected a version error");
        assert_eq!(
            err.kind(),
            &ErrorKind::Parse(ParseError::UnsupportedVersion {
                got: 2,
                expected: 1,
            })
        );
    }

    #[test]
    fn an_empty_payload_is_not_a_response() {
        let bytes = payload(|_| {});
        let err = decode_response(bytes).expect_err("expected unknown-payload");
        assert_eq!(err.kind(), &ErrorKind::Parse(ParseError::UnknownPayload));
    }

    #[test]
    fn garbage_is_reported_as_malformed() {
        let err = decode_response(Bytes::from_static(&[0xFF, 0xFF, 0xFF, 0xFF]))
            .expect_err("expected undecodable");
        assert!(matches!(
            err.kind(),
            ErrorKind::Parse(ParseError::Undecodable(_))
        ));
    }

    #[test]
    fn the_wrong_response_kind_is_caught() {
        let bytes = payload(|p| p.auth_response = Some(wire::AuthResponse { code: 1 }));
        let resp = decode_response(bytes).expect("decode");

        let err = resp
            .expect_kind("CountResponse")
            .expect_err("expected a message-kind error");
        assert_eq!(
            err.kind(),
            &ErrorKind::Parse(ParseError::UnexpectedMessage {
                expected: "CountResponse",
                got: "AuthResponse",
            })
        );
    }

    #[test]
    fn a_request_round_trips_through_the_payload_wrapper() {
        let bytes = Request::helo().encode();
        let decoded = wire::QueryPayload::decode(&bytes[..]).expect("decode");
        assert_eq!(decoded.version, PROTOBUF_VERSION);
        assert_eq!(
            decoded.helo_request.expect("helo").version,
            PROTOBUF_VERSION
        );
    }

    #[test]
    fn auth_carries_the_credentials_and_the_client_version() {
        let bytes = Request::auth("Administrator", "password", 8).encode();
        let decoded = wire::QueryPayload::decode(&bytes[..]).expect("decode");
        let auth = decoded.auth_request.expect("auth");
        assert_eq!(auth.user, "Administrator");
        assert_eq!(auth.pass, "password");
        assert_eq!(auth.client_version, Some(8));
    }

    #[test]
    fn a_count_sends_the_empty_span_the_schema_requires() {
        // `span` is proto2-required, so omitting it produces a payload the
        // server rejects outright. Sending it empty is how "every entry" is
        // spelled, and this test is here because the failure is otherwise a
        // confusing decode error from the far end.
        let bytes = Request::count_all(8891, Consistency::Session, vec![0]).encode();
        let decoded = wire::QueryPayload::decode(&bytes[..]).expect("decode");
        let count = decoded.count_request.expect("count");
        assert_eq!(count.defn_id, 8891);
        assert_eq!(count.cons, 2, "Session is request_plus, wire value 2");
        assert_eq!(count.partition_ids, vec![0]);
        assert_eq!(count.span.range, None);
        assert!(count.span.equals.is_empty());
    }
}

/// Building the *server's* side of the protocol, for tests.
///
/// It lives here rather than in a test module because it needs the generated
/// types, and those do not leave this file — the same rule the rest of the
/// module follows, applied to the one legitimate reason to break it.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;

    fn payload(f: impl FnOnce(&mut wire::QueryPayload)) -> Bytes {
        let mut p = wire::QueryPayload {
            version: PROTOBUF_VERSION,
            ..Default::default()
        };
        f(&mut p);
        p.encode_to_vec().into()
    }

    pub(crate) fn auth_response(code: u32) -> Bytes {
        payload(|p| p.auth_response = Some(wire::AuthResponse { code }))
    }

    pub(crate) fn helo_response(version: u32) -> Bytes {
        payload(|p| p.helo_response = Some(wire::HeloResponse { version }))
    }

    pub(crate) fn count_response(count: i64) -> Bytes {
        payload(|p| p.count_response = Some(wire::CountResponse { count, err: None }))
    }

    /// One `ResponseStream` batch. Entries are `(entry_key, primary_key)`.
    pub(crate) fn entries(rows: &[(Option<&[u8]>, &[u8])]) -> Bytes {
        payload(|p| {
            p.stream = Some(wire::ResponseStream {
                index_entries: rows
                    .iter()
                    .map(|(entry_key, primary_key)| wire::IndexEntry {
                        entry_key: entry_key.map(Bytes::copy_from_slice),
                        primary_key: Bytes::copy_from_slice(primary_key),
                    })
                    .collect(),
                err: None,
            })
        })
    }

    /// The indexer refusing mid-stream, which it follows with a terminator.
    pub(crate) fn stream_error(message: &str) -> Bytes {
        payload(|p| {
            p.stream = Some(wire::ResponseStream {
                index_entries: Vec::new(),
                err: Some(wire::Error {
                    error: message.to_owned(),
                }),
            })
        })
    }

    pub(crate) fn stream_end(read_units: Option<u64>) -> Bytes {
        payload(|p| {
            p.stream_end = Some(wire::StreamEndResponse {
                err: None,
                read_units,
            })
        })
    }

    /// What the client just sent, so a fake server can branch on it.
    pub(crate) fn request_kind(payload: &[u8]) -> &'static str {
        let p = wire::QueryPayload::decode(payload).expect("a well-formed request");
        if p.auth_request.is_some() {
            "AuthRequest"
        } else if p.helo_request.is_some() {
            "HeloRequest"
        } else if p.scan_request.is_some() {
            "ScanRequest"
        } else if p.count_request.is_some() {
            "CountRequest"
        } else if p.end_stream.is_some() {
            "EndStreamRequest"
        } else {
            "unknown"
        }
    }

    /// The decoded `ScanRequest`, for asserting what actually went on the wire.
    pub(crate) fn scan_request_fields(payload: &[u8]) -> ScanRequestFields {
        let p = wire::QueryPayload::decode(payload).expect("a well-formed request");
        let s = p.scan_request.expect("a scan request");
        ScanRequestFields {
            defn_id: s.defn_id,
            cons: s.cons,
            limit: s.limit,
            offset: s.offset.unwrap_or(0),
            distinct: s.distinct,
            data_enc_fmt: s.data_enc_fmt,
            reverse: s.reverse,
            partition_ids: s.partition_ids,
            request_id: s.request_id,
            projected_entry_keys: s.indexprojection.as_ref().map(|p| p.entry_keys.clone()),
            projected_primary_key: s.indexprojection.as_ref().and_then(|p| p.primary_key),
            scan_filters: s
                .scans
                .iter()
                .map(|scan| {
                    scan.filters
                        .iter()
                        .map(|f| (f.low.clone(), f.high.clone(), f.inclusion))
                        .collect()
                })
                .collect(),
            scan_equals: s.scans.iter().map(|scan| scan.equals.clone()).collect(),
        }
    }

    #[allow(clippy::type_complexity)]
    pub(crate) struct ScanRequestFields {
        pub defn_id: u64,
        pub cons: u32,
        pub limit: i64,
        pub offset: i64,
        pub distinct: bool,
        pub data_enc_fmt: Option<u32>,
        pub reverse: Option<bool>,
        pub partition_ids: Vec<u64>,
        pub request_id: Option<String>,
        pub projected_entry_keys: Option<Vec<i64>>,
        pub projected_primary_key: Option<bool>,
        pub scan_filters: Vec<Vec<(Option<Vec<u8>>, Option<Vec<u8>>, u32)>>,
        pub scan_equals: Vec<Vec<Vec<u8>>>,
    }
}
