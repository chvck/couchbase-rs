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

//! `indexerx`'s errors, layered the way `memdx`'s are.
//!
//! ```text
//! ProtocolError  framing        bad encoding, checksum, oversize   (packet_codec)
//! CodecError     framing + io                                      (packet_codec)
//! ParseError     payload        undecodable, unknown, wrong shape  (proto)
//! ServerError    the indexer said no, in its own words, typed      (here)
//! Error          the package's error; all of the above lift into it
//! ```
//!
//! Every layer resolves what it alone can name and lifts the rest. The rule
//! that matters is the one about `ServerError`: the indexing service reports
//! failures as **strings** inside an otherwise successful response, and the
//! translation from those strings to something a caller can match on happens
//! exactly once, here. Nothing above `indexerx` string-matches an indexer
//! error, so a message the indexer rewords breaks one test in this file rather
//! than silently disabling a retry three layers up.
//!
//! [`Error`] is the crate's usual opaque wrapper over a [`ErrorKind`] reached
//! through [`Error::kind`], as in `memdx` and `queryx`, and it lifts into
//! [`crate::error::Error`] as `ErrorKind::Indexer`.

use std::error::Error as StdError;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use crate::httpx;
use crate::tracingcomponent::MetricsName;

use super::packet_codec::{CodecError, ProtocolError};
use super::proto::ParseError;

pub type Result<T> = std::result::Result<T, Error>;

/// A source error kept for [`StdError::source`] only.
///
/// `Arc` rather than `Box` because [`Error`] is `Clone` — a scan hands the same
/// failure to the stream that produced it and to the caller draining it.
type Source = Arc<dyn StdError + Send + Sync>;

#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    inner: ErrorImpl,
}

impl Error {
    /// The socket could not be opened, or could not be upgraded to TLS.
    ///
    /// Flattened from `memdx`'s error rather than nested inside it: the shared
    /// transport is an implementation detail of [`super::client::Client`], and
    /// a caller matching on `indexerx` errors should not have to know that
    /// `memdx`'s taxonomy exists. The original is kept as the
    /// [`source`](StdError::source), so nothing is lost.
    pub(crate) fn new_connect_error(
        msg: impl Into<String>,
        source: crate::memdx::error::Error,
    ) -> Self {
        Self {
            inner: ErrorImpl {
                kind: Box::new(ErrorKind::Connect { msg: msg.into() }),
                source: Some(Arc::new(source)),
            },
        }
    }

    pub(crate) fn new_io_error(source: std::io::Error) -> Self {
        Self {
            inner: ErrorImpl {
                kind: Box::new(ErrorKind::Io {
                    kind: source.kind(),
                }),
                source: Some(Arc::new(source)),
            },
        }
    }

    pub(crate) fn new_protocol_error(e: ProtocolError) -> Self {
        Self::new(ErrorKind::Protocol(e))
    }

    pub(crate) fn new_parse_error(e: ParseError) -> Self {
        Self::new(ErrorKind::Parse(e))
    }

    pub(crate) fn new_server_error(e: ServerError) -> Self {
        Self::new(ErrorKind::Server(e))
    }

    pub(crate) fn new_authentication_error(code: AuthCode) -> Self {
        Self::new(ErrorKind::Authentication(code))
    }

    pub(crate) fn new_busy_error() -> Self {
        Self::new(ErrorKind::Busy)
    }

    pub(crate) fn new_unexpected_eof_error() -> Self {
        Self::new(ErrorKind::UnexpectedEof)
    }

    pub(crate) fn new_decoding_error(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Decoding { msg: msg.into() })
    }

    fn new(kind: ErrorKind) -> Self {
        Self {
            inner: ErrorImpl {
                kind: Box::new(kind),
                source: None,
            },
        }
    }

    pub fn kind(&self) -> &ErrorKind {
        &self.inner.kind
    }
}

#[derive(Debug, Clone)]
struct ErrorImpl {
    kind: Box<ErrorKind>,
    source: Option<Source>,
}

/// Two errors are the same error when they are the same kind. The `source` is
/// diagnostic detail hanging off the kind, and `std::io::Error` cannot be
/// compared anyway.
impl PartialEq for ErrorImpl {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}

/// Everything that can go wrong between here and an indexer.
///
/// `Io` carries only [`std::io::ErrorKind`], which is the part a caller
/// branches on; the `std::io::Error` itself is the [`Error`]'s
/// [`source`](StdError::source). Keeping it out of the kind is what lets this
/// enum be compared, which [`crate::error::ErrorKind`] requires of everything
/// it wraps.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    Io {
        kind: std::io::ErrorKind,
    },
    Protocol(ProtocolError),
    Parse(ParseError),
    Server(ServerError),

    /// The socket could not be opened, or the TLS handshake failed. Distinct
    /// from `Io`, which is a live connection going wrong.
    ///
    /// `msg` names which step failed and nothing more; the detail is the
    /// error's [`source`](StdError::source), the same split `Io` makes. Putting
    /// the source's own text in here would print it twice.
    #[non_exhaustive]
    Connect {
        msg: String,
    },

    /// The server rejected our credentials, or wanted credentials on a
    /// connection that offered none.
    ///
    /// The second case should be unreachable — every connection authenticates
    /// before anything else — so it is an error rather than the reconnect the
    /// Go client performs. If it ever fires, the bug is that we skipped auth.
    Authentication(AuthCode),

    /// The connection is spoken for: queryport does not multiplex, so a second
    /// request cannot be issued while a scan is streaming.
    Busy,

    /// The stream ended without the terminator that ends a stream. The
    /// connection is desynchronised and must not be reused.
    UnexpectedEof,

    /// The indexing service's HTTP surface — [`status`](super::status) — could
    /// not be reached. The scan protocol does not use HTTP at all.
    Http(httpx::error::Error),

    /// A response arrived but could not be read: JSON the endpoint's schema
    /// does not describe. Distinct from `Parse`, which is about protobuf.
    #[non_exhaustive]
    Decoding {
        msg: String,
    },
}

/// What the indexer said, classified.
///
/// The raw text is kept on every variant including the classified ones,
/// because the classification is a lossy read of someone else's log line and
/// the original is what makes a surprise diagnosable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerError {
    /// The partition moved. The caller should refresh topology and retry —
    /// this is the variant the whole taxonomy exists for.
    NotMyPartition(String),
    /// The index was dropped, or never existed under this definition id.
    IndexNotFound(String),
    /// The index exists but is still building.
    IndexNotReady(String),
    /// Something the indexer says that we have not classified. Reportable
    /// rather than lost, which is the point of keeping the string.
    Other(String),
}

impl ServerError {
    /// Classify an error string from `ResponseStream.err` or
    /// `StreamEndResponse.err`.
    ///
    /// **Matched case-insensitively and on a substring**, because the same
    /// condition arrives wrapped: the indexer's own `ErrIndexNotFound` is bare
    /// text, but the same condition reaching us through a scan coordinator
    /// arrives with a request id and a prefix around it. An exact-equality
    /// check would pass the unit test written from `client/error.go` and then
    /// fail against a cluster.
    pub fn classify(raw: impl Into<String>) -> ServerError {
        let raw = raw.into();
        let lower = raw.to_lowercase();

        if lower.contains("not my partition") {
            ServerError::NotMyPartition(raw)
        } else if lower.contains("index not found") {
            ServerError::IndexNotFound(raw)
        } else if lower.contains("index not ready") {
            ServerError::IndexNotReady(raw)
        } else {
            ServerError::Other(raw)
        }
    }

    /// The text the indexer sent, whatever the classification.
    pub fn raw(&self) -> &str {
        match self {
            ServerError::NotMyPartition(s)
            | ServerError::IndexNotFound(s)
            | ServerError::IndexNotReady(s)
            | ServerError::Other(s) => s,
        }
    }

    /// Whether a caller that refreshes its topology could reasonably expect a
    /// retry to succeed.
    ///
    /// It is a method here rather than a `match` at the call site so that
    /// adding a variant forces this decision to be taken once, in the place
    /// that knows the service, instead of being missed at one of several call
    /// sites that do not.
    pub fn is_retryable_after_refresh(&self) -> bool {
        match self {
            ServerError::NotMyPartition(_) => true,
            // A dropped index will not come back, and a building one will not
            // finish inside a retry loop. Both are the caller's problem.
            ServerError::IndexNotFound(_) | ServerError::IndexNotReady(_) => false,
            ServerError::Other(_) => false,
        }
    }
}

/// `AuthResponse.code`, from `transport`'s `AUTH_*` constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthCode {
    Success,
    Failure,
    /// The server wants credentials on a connection that sent none.
    Missing,
    Unknown(u32),
}

impl From<u32> for AuthCode {
    fn from(code: u32) -> Self {
        match code {
            1 => AuthCode::Success,
            2 => AuthCode::Failure,
            3 => AuthCode::Missing,
            other => AuthCode::Unknown(other),
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.inner.kind)?;
        if let Some(source) = &self.inner.source {
            write!(f, ": {source}")?;
        }
        Ok(())
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.inner
            .source
            .as_ref()
            .map(|s| s.as_ref() as &(dyn StdError + 'static))
    }
}

impl Display for ErrorKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrorKind::Io { kind } => write!(f, "indexerx IO error: {kind}"),
            ErrorKind::Connect { msg } => write!(f, "indexerx connect error: {msg}"),
            ErrorKind::Protocol(e) => write!(f, "indexerx protocol error: {e}"),
            ErrorKind::Parse(e) => write!(f, "indexerx parse error: {e}"),
            ErrorKind::Server(e) => write!(f, "indexerx server error: {e}"),
            ErrorKind::Authentication(c) => write!(f, "indexerx authentication failed: {c}"),
            ErrorKind::Busy => write!(f, "indexerx connection is already serving a request"),
            ErrorKind::UnexpectedEof => {
                write!(f, "indexerx connection closed before the response ended")
            }
            ErrorKind::Http(e) => write!(f, "indexerx http error: {e}"),
            ErrorKind::Decoding { msg } => write!(f, "indexerx decoding error: {msg}"),
        }
    }
}

impl MetricsName for Error {
    fn metrics_name(&self) -> &'static str {
        match self.kind() {
            ErrorKind::Io { .. } => "indexerx.Io",
            ErrorKind::Connect { .. } => "indexerx.Connect",
            ErrorKind::Protocol(_) => "indexerx.Protocol",
            ErrorKind::Parse(_) => "indexerx.Parse",
            ErrorKind::Server(e) => e.metrics_name(),
            ErrorKind::Authentication(_) => "indexerx.Authentication",
            ErrorKind::Busy => "indexerx.Busy",
            ErrorKind::UnexpectedEof => "indexerx.UnexpectedEof",
            ErrorKind::Http(e) => e.metrics_name(),
            ErrorKind::Decoding { .. } => "indexerx.Decoding",
        }
    }
}

impl Display for ServerError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerError::NotMyPartition(s) => write!(f, "not my partition: {s}"),
            ServerError::IndexNotFound(s) => write!(f, "index not found: {s}"),
            ServerError::IndexNotReady(s) => write!(f, "index not ready: {s}"),
            ServerError::Other(s) => write!(f, "{s}"),
        }
    }
}

impl StdError for ServerError {}

impl MetricsName for ServerError {
    fn metrics_name(&self) -> &'static str {
        match self {
            ServerError::NotMyPartition(_) => "indexerx.NotMyPartition",
            ServerError::IndexNotFound(_) => "indexerx.IndexNotFound",
            ServerError::IndexNotReady(_) => "indexerx.IndexNotReady",
            ServerError::Other(_) => "indexerx._OTHER",
        }
    }
}

impl Display for AuthCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthCode::Success => write!(f, "success"),
            AuthCode::Failure => write!(f, "invalid credentials"),
            AuthCode::Missing => write!(f, "credentials missing"),
            AuthCode::Unknown(c) => write!(f, "unknown auth code {c}"),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::new_io_error(e)
    }
}

impl From<ProtocolError> for Error {
    fn from(e: ProtocolError) -> Self {
        Error::new_protocol_error(e)
    }
}

impl From<ParseError> for Error {
    fn from(e: ParseError) -> Self {
        Error::new_parse_error(e)
    }
}

impl From<ServerError> for Error {
    fn from(e: ServerError) -> Self {
        Error::new_server_error(e)
    }
}

impl From<httpx::error::Error> for Error {
    fn from(e: httpx::error::Error) -> Self {
        Error::new(ErrorKind::Http(e))
    }
}

impl From<CodecError> for Error {
    fn from(e: CodecError) -> Self {
        match e {
            CodecError::Io(e) => Error::new_io_error(e),
            CodecError::Protocol(e) => Error::new_protocol_error(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_classified_conditions_are_recognised() {
        // The exact strings from indexing/secondary/queryport/client/error.go.
        assert!(matches!(
            ServerError::classify("Not my partition"),
            ServerError::NotMyPartition(_)
        ));
        assert!(matches!(
            ServerError::classify("Index not found"),
            ServerError::IndexNotFound(_)
        ));
        assert!(matches!(
            ServerError::classify("Index not ready for serving queries"),
            ServerError::IndexNotReady(_)
        ));
    }

    #[test]
    fn classification_survives_being_wrapped() {
        // What a scan coordinator actually sends, rather than what the
        // constant says. An exact-equality check passes the test above and
        // fails this one, which is the failure mode this method exists to
        // avoid.
        let wrapped = "Scan request 4f2a failed: Index not found (defnId 8891)";
        assert!(matches!(
            ServerError::classify(wrapped),
            ServerError::IndexNotFound(_)
        ));
        assert!(matches!(
            ServerError::classify("NOT MY PARTITION"),
            ServerError::NotMyPartition(_)
        ));
    }

    #[test]
    fn an_unclassified_error_keeps_its_text() {
        let err = ServerError::classify("something we have never seen");
        assert_eq!(
            err,
            ServerError::Other("something we have never seen".into())
        );
        assert_eq!(err.raw(), "something we have never seen");
        assert!(!err.is_retryable_after_refresh());
    }

    #[test]
    fn classified_errors_keep_their_text_too() {
        let err = ServerError::classify("Index not found");
        assert_eq!(err.raw(), "Index not found");
    }

    #[test]
    fn only_a_moved_partition_is_worth_retrying() {
        assert!(ServerError::classify("Not my partition").is_retryable_after_refresh());
        assert!(!ServerError::classify("Index not found").is_retryable_after_refresh());
        assert!(!ServerError::classify("Index not ready").is_retryable_after_refresh());
    }

    #[test]
    fn auth_codes_match_the_transport_constants() {
        assert_eq!(AuthCode::from(1), AuthCode::Success);
        assert_eq!(AuthCode::from(2), AuthCode::Failure);
        assert_eq!(AuthCode::from(3), AuthCode::Missing);
        assert_eq!(AuthCode::from(9), AuthCode::Unknown(9));
    }

    #[test]
    fn an_io_error_keeps_both_its_classification_and_its_text() {
        // The classification is on the kind so a caller can branch on it; the
        // `std::io::Error` is the source so the message survives. Splitting
        // them is what lets `ErrorKind` be compared.
        let err = Error::from(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "peer went away",
        ));
        assert_eq!(
            err.kind(),
            &ErrorKind::Io {
                kind: std::io::ErrorKind::ConnectionReset
            }
        );
        assert!(err.to_string().contains("peer went away"));
        assert!(err.source().is_some());
    }
}
