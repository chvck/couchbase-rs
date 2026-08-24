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

use crate::analyticsx::error::Error as AnalyticsError;
use crate::httpx::error::Error as HttpError;
use crate::indexerx::error::Error as IndexerError;
use crate::indexrouter::RouteError;
use crate::memdx;
use crate::mgmtx::error::Error as MgmtError;
use crate::queryx::error::Error as QueryError;
use crate::retry::RetryRequest;
use crate::searchx::error::Error as SearchError;
use crate::service_type::ServiceType;
use crate::tracingcomponent::MetricsName;
use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt::{Display, Formatter};
use std::ops::Deref;
use std::sync::Arc;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub struct Error {
    kind: Arc<ErrorKind>,
    retry_info: Option<RetryRequest>,
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Some(retry_info) = &self.retry_info {
            return write!(f, "{}, {}", self.kind, retry_info);
        }
        write!(f, "{}", self.kind)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self.kind.as_ref() {
            ErrorKind::Memdx(err) => err.inner.source.source(),
            ErrorKind::Query(err) => err.source(),
            ErrorKind::Search(err) => err.source(),
            ErrorKind::Http(err) => err.source(),
            ErrorKind::Mgmt(err) => err.source(),
            ErrorKind::Indexer(err) => err.source(),
            // The attempt's own error, so wrapping a connect failure does not
            // cost a caller the cause chain it had before.
            ErrorKind::ConnectFailed { source, .. } => Some(source.cause()),
            _ => None,
        }
    }
}

impl Error {
    pub fn new(kind: ErrorKind) -> Self {
        Self {
            kind: Arc::new(kind),
            retry_info: None,
        }
    }

    pub(crate) fn new_contextual_memdx_error(e: MemdxError) -> Self {
        Self::new(ErrorKind::Memdx(e))
    }

    pub(crate) fn new_bootstrap_all_failed_error(errors: HashMap<String, Error>) -> Self {
        Self::new(ErrorKind::BootstrapAllFailed {
            errors: BootstrapFailures::new(errors),
        })
    }

    /// A connect failure against `endpoint`, carrying what the attempt said.
    ///
    /// **Public, unlike its neighbours**, for the reason `a6917a0e` made the
    /// crate's errors constructible: an embedder that classifies on
    /// [`ErrorKind::ConnectFailed`] has to be able to build one to test that it
    /// does, and the variant is `#[non_exhaustive]` so a struct literal will not
    /// do.
    pub fn new_connect_failed_error(endpoint: impl Into<String>, cause: Error) -> Self {
        Self::new(ErrorKind::ConnectFailed {
            endpoint: endpoint.into(),
            source: ConnectAttemptFailure::new(cause),
        })
    }

    pub(crate) fn new_message_error(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Message { msg: msg.into() })
    }

    pub(crate) fn new_invalid_argument_error(
        msg: impl Into<String>,
        arg: impl Into<Option<String>>,
    ) -> Self {
        Self::new(ErrorKind::InvalidArgument {
            msg: msg.into(),
            arg: arg.into(),
        })
    }

    pub(crate) fn new_feature_not_available_error(
        feature: impl Into<String>,
        msg: impl Into<String>,
    ) -> Self {
        Self::new(ErrorKind::FeatureNotAvailable {
            feature: feature.into(),
            msg: msg.into(),
        })
    }

    pub fn kind(&self) -> &ErrorKind {
        &self.kind
    }

    pub(crate) fn is_memdx_error(&self) -> Option<&memdx::error::Error> {
        match self.kind.as_ref() {
            ErrorKind::Memdx(err) => Some(err),
            _ => None,
        }
    }

    pub(crate) fn set_retry_info(&mut self, retry_info: RetryRequest) {
        self.retry_info = Some(retry_info);
    }

    pub fn retry_info(&self) -> Option<&RetryRequest> {
        self.retry_info.as_ref()
    }
}

#[derive(Debug, PartialEq)]
#[non_exhaustive]
pub enum ErrorKind {
    Memdx(MemdxError),
    Analytics(AnalyticsError),
    Query(QueryError),
    Search(SearchError),
    Http(HttpError),
    Mgmt(MgmtError),
    Indexer(IndexerError),
    /// A scan could not be routed to an indexer. Distinct from
    /// [`ErrorKind::Indexer`], which is what an indexer said: this is a
    /// disagreement between the index topology this client holds and the
    /// cluster, and [`RouteError::worth_refreshing`] is how a caller tells the
    /// two apart without reading any strings.
    IndexRouting(RouteError),
    VbucketMapOutdated,
    #[non_exhaustive]
    InvalidArgument {
        msg: String,
        arg: Option<String>,
    },
    #[non_exhaustive]
    EndpointNotKnown {
        endpoint: String,
    },
    InvalidVbucket {
        requested_vb_id: u16,
        num_vbuckets: usize,
    },
    InvalidReplica {
        requested_replica: u32,
        num_servers: usize,
    },
    NoEndpointsAvailable,
    Shutdown,
    NoBucket,
    IllegalState {
        msg: String,
    },
    NoVbucketMap,
    #[non_exhaustive]
    NoServerAssigned {
        requested_vb_id: u16,
    },
    #[non_exhaustive]
    CollectionManifestOutdated {
        manifest_uid: u64,
        server_manifest_uid: u64,
    },
    #[non_exhaustive]
    Message {
        msg: String,
    },
    #[non_exhaustive]
    ServiceNotAvailable {
        service: ServiceType,
    },
    #[non_exhaustive]
    FeatureNotAvailable {
        feature: String,
        msg: String,
    },
    #[non_exhaustive]
    Compression {
        msg: String,
    },
    #[non_exhaustive]
    Internal {
        msg: String,
    },
    /// Every endpoint an agent was given failed to supply a cluster config.
    ///
    /// Carries what each one said, keyed by endpoint, because with a seed list
    /// the interesting part is usually that the answers differ -- one host
    /// refused, another rejected the credentials.
    #[non_exhaustive]
    BootstrapAllFailed {
        errors: BootstrapFailures,
    },

    /// An operation could not get a connection to a KV endpoint, and
    /// [`KvConfig::surface_connect_errors`] asked for the reason rather than a
    /// wait.
    ///
    /// **The provenance is the point.** The pool knows this error came from a
    /// connect attempt because of where it stored it, so it says so here rather
    /// than handing back the attempt's own error and leaving a caller to work it
    /// out. Working it out is not reliably possible: a refused dial is
    /// `memdx::ErrorKind::ConnectionFailed`, a node lost mid-bootstrap is
    /// `Close`, `Io` or `Cancelled`, and a password the server refuses arrives as
    /// `Server(UnknownStatus { status: AuthError })` -- shaped exactly like a
    /// status the server returned to the operation itself.
    ///
    /// That last shape is also why this is not merely tidier. Left raw, the
    /// failure reaches [`crate::retry`] as a server's answer and is classified as
    /// one: `UnknownStatus` is put to the server-supplied error map, so whether a
    /// rotated password reached the caller or was retried under a best-effort
    /// strategy depended on what that map said about `0x20`. A connect failure is
    /// not an answer about the operation, and this kind is not routed through
    /// answer classification.
    ///
    /// Only produced when the option is on; with it off an operation waits as
    /// before and this kind never appears.
    #[non_exhaustive]
    ConnectFailed {
        /// The endpoint that could not be reached.
        endpoint: String,
        /// What the last connect attempt against it said.
        source: ConnectAttemptFailure,
    },
}

/// What each endpoint said when none of them could supply a cluster config.
///
/// Keyed by endpoint, because with a seed list the interesting part is usually
/// that the answers differ: one host refused the connection, another rejected
/// the credentials.
#[derive(Debug, Clone, Default)]
pub struct BootstrapFailures(HashMap<String, Error>);

impl BootstrapFailures {
    pub(crate) fn new(errors: HashMap<String, Error>) -> Self {
        Self(errors)
    }

    /// Why this endpoint failed, if it was one of those tried.
    pub fn get(&self, endpoint: &str) -> Option<&Error> {
        self.0.get(endpoint)
    }

    /// Every endpoint tried, with what it said, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Error)> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Compares which endpoints failed, not what they said: an [`Error`] is not
/// itself comparable, and [`ErrorKind`] needs this only to answer whether two
/// failures are the same kind of failure.
impl PartialEq for BootstrapFailures {
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len() && self.0.keys().all(|e| other.0.contains_key(e))
    }
}

/// What a connect attempt said, carried by [`ErrorKind::ConnectFailed`].
///
/// **A newtype for the reason [`BootstrapFailures`] is one**: `ErrorKind` derives
/// `PartialEq` and [`Error`] does not implement it, so holding an `Error` inside
/// a kind means writing the comparison rather than deriving it.
#[derive(Debug, Clone)]
pub struct ConnectAttemptFailure(Error);

impl ConnectAttemptFailure {
    pub(crate) fn new(cause: Error) -> Self {
        Self(cause)
    }

    /// What the attempt said.
    pub fn cause(&self) -> &Error {
        &self.0
    }
}

impl PartialEq for ConnectAttemptFailure {
    /// **The endpoint decides, not the cause**, which is the rule
    /// [`BootstrapFailures`] uses when it compares its keys and not its values.
    /// The endpoint sits beside this in the kind and is compared by the derive,
    /// so two failures against the same endpoint are the same failure however
    /// differently the last attempt happened to fail.
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Display for ConnectAttemptFailure {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Display for ErrorKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrorKind::VbucketMapOutdated => write!(f, "vbucket map outdated"),
            ErrorKind::InvalidArgument { msg, arg } => {
                if let Some(arg) = arg {
                    write!(f, "invalid argument {arg}: {msg}")
                } else {
                    write!(f, "invalid argument: {msg}")
                }
            }
            ErrorKind::Memdx(err) => write!(f, "{err}"),
            ErrorKind::Analytics(err) => write!(f, "{err}"),
            ErrorKind::Query(err) => write!(f, "{err}"),
            ErrorKind::Search(err) => write!(f, "{err}"),
            ErrorKind::Http(err) => write!(f, "{err}"),
            ErrorKind::Mgmt(err) => write!(f, "{err}"),
            ErrorKind::Indexer(err) => write!(f, "{err}"),
            ErrorKind::IndexRouting(err) => write!(f, "{err}"),
            ErrorKind::EndpointNotKnown { endpoint } => {
                write!(f, "endpoint not known: {endpoint}")
            }
            ErrorKind::NoEndpointsAvailable => write!(f, "no endpoints available"),
            ErrorKind::Shutdown => write!(f, "shutdown"),
            ErrorKind::NoBucket => write!(f, "no bucket selected"),
            ErrorKind::IllegalState { msg } => write!(f, "illegal state: {msg}"),
            ErrorKind::NoVbucketMap => write!(f, "invalid vbucket map"),
            ErrorKind::BootstrapAllFailed { errors } => {
                // Sorted, so that the same set of failures reads the same way
                // twice: a HashMap would otherwise reorder them per process.
                let mut endpoints: Vec<_> = errors.iter().collect();

                endpoints.sort_by(|(a, _), (b, _)| a.cmp(b));

                let detail = endpoints
                    .into_iter()
                    .map(|(endpoint, err)| format!("{endpoint}: {{{err}}}"))
                    .collect::<Vec<_>>()
                    .join(", ");

                write!(f, "all bootstrap hosts failed ({detail})")
            }
            ErrorKind::ConnectFailed { endpoint, source } => {
                write!(f, "could not connect to {endpoint}: {source}")
            }
            ErrorKind::CollectionManifestOutdated {
                manifest_uid,
                server_manifest_uid,
            } => {
                write!(
                    f,
                    "collection manifest outdated: our manifest uid: {manifest_uid}, server manifest uid: {server_manifest_uid}"
                )
            }
            ErrorKind::Message { msg } => write!(f, "{msg}"),
            ErrorKind::ServiceNotAvailable { service } => {
                write!(f, "service not available: {service}")
            }
            ErrorKind::FeatureNotAvailable { feature, msg } => {
                write!(f, "feature not available: {feature}, {msg}")
            }
            ErrorKind::Internal { msg } => write!(f, "internal error: {msg}"),
            ErrorKind::NoServerAssigned { requested_vb_id } => {
                write!(f, "no server assigned for vbucket id: {requested_vb_id}")
            }
            ErrorKind::InvalidVbucket {
                requested_vb_id,
                num_vbuckets,
            } => write!(
                f,
                "invalid vbucket id: {requested_vb_id}, num vbuckets: {num_vbuckets}"
            ),
            ErrorKind::InvalidReplica {
                requested_replica,
                num_servers,
            } => write!(
                f,
                "invalid replica: {requested_replica}, num servers: {num_servers}"
            ),
            ErrorKind::Compression { msg } => write!(f, "compression error: {msg}"),
        }
    }
}

impl MetricsName for Error {
    fn metrics_name(&self) -> &'static str {
        self.kind().metrics_name()
    }
}

impl MetricsName for ErrorKind {
    fn metrics_name(&self) -> &'static str {
        match self {
            ErrorKind::Memdx(err) => err.metrics_name(),
            ErrorKind::Analytics(err) => err.metrics_name(),
            ErrorKind::Query(err) => err.metrics_name(),
            ErrorKind::Search(err) => err.metrics_name(),
            ErrorKind::Http(err) => err.metrics_name(),
            ErrorKind::Mgmt(err) => err.metrics_name(),
            ErrorKind::Indexer(err) => err.metrics_name(),
            ErrorKind::IndexRouting(_) => "IndexRouting",
            ErrorKind::ConnectFailed { .. } => "ConnectFailed",
            ErrorKind::InvalidArgument { .. } => "InvalidArgument",
            ErrorKind::ServiceNotAvailable { .. } => "ServiceNotAvailable",
            ErrorKind::FeatureNotAvailable { .. } => "FeatureNotAvailable",
            ErrorKind::VbucketMapOutdated => "VBucketMapOutdated",
            ErrorKind::EndpointNotKnown { .. } => "EndpointNotKnown",
            ErrorKind::InvalidVbucket { .. } => "InvalidVbucket",
            ErrorKind::InvalidReplica { .. } => "InvalidReplica",
            ErrorKind::NoEndpointsAvailable => "NoEndpointsAvailable",
            ErrorKind::Shutdown => "Shutdown",
            ErrorKind::NoBucket => "NoBucket",
            ErrorKind::IllegalState { .. } => "IllegalState",
            ErrorKind::NoVbucketMap => "NoVbucketMap",
            ErrorKind::BootstrapAllFailed { .. } => "BootstrapAllFailed",
            ErrorKind::NoServerAssigned { .. } => "NoServerAssigned",
            ErrorKind::CollectionManifestOutdated { .. } => "CollectionManifestOutdated",
            ErrorKind::Message { .. } => "_OTHER",
            ErrorKind::Compression { .. } => "Compression",
            ErrorKind::Internal { .. } => "_OTHER",
        }
    }
}

impl MetricsName for MemdxError {
    fn metrics_name(&self) -> &'static str {
        self.inner.source.metrics_name()
    }
}

#[derive(Debug, PartialEq)]
pub struct MemdxError {
    inner: Box<InnerMemdxError>,
}

#[derive(Debug, PartialEq)]
pub struct InnerMemdxError {
    source: memdx::error::Error,
    dispatched_to: Option<String>,
    dispatched_from: Option<String>,
    doc_id: Option<Vec<u8>>,
    bucket_name: Option<String>,
    scope_name: Option<String>,
    collection_name: Option<String>,
}

impl Deref for MemdxError {
    type Target = memdx::error::Error;

    fn deref(&self) -> &Self::Target {
        &self.inner.source
    }
}

impl MemdxError {
    pub(crate) fn new(source: memdx::error::Error) -> Self {
        Self {
            inner: Box::new(InnerMemdxError {
                source,
                dispatched_to: None,
                dispatched_from: None,
                doc_id: None,
                bucket_name: None,
                scope_name: None,
                collection_name: None,
            }),
        }
    }

    pub(crate) fn with_dispatched_to(mut self, dispatched_to: impl Into<String>) -> Self {
        self.inner.dispatched_to = Some(dispatched_to.into());
        self
    }

    pub(crate) fn with_dispatched_from(mut self, dispatched_from: impl Into<String>) -> Self {
        self.inner.dispatched_from = Some(dispatched_from.into());
        self
    }

    pub fn dispatched_to(&self) -> Option<&String> {
        self.inner.dispatched_to.as_ref()
    }

    pub fn dispatched_from(&self) -> Option<&String> {
        self.inner.dispatched_from.as_ref()
    }

    pub fn doc_id(&self) -> Option<&[u8]> {
        self.inner.doc_id.as_deref()
    }

    pub fn bucket_name(&self) -> Option<&String> {
        self.inner.bucket_name.as_ref()
    }

    pub fn scope_name(&self) -> Option<&String> {
        self.inner.scope_name.as_ref()
    }

    pub fn collection_name(&self) -> Option<&String> {
        self.inner.collection_name.as_ref()
    }

    pub(crate) fn set_doc_id(mut self, doc_id: Vec<u8>) -> Self {
        self.inner.doc_id = Some(doc_id);
        self
    }

    pub(crate) fn set_bucket_name(mut self, bucket_name: String) -> Self {
        self.inner.bucket_name = Some(bucket_name);
        self
    }

    pub(crate) fn set_scope_name(mut self, scope_name: String) -> Self {
        self.inner.scope_name = Some(scope_name);
        self
    }

    pub(crate) fn set_collection_name(mut self, collection_name: String) -> Self {
        self.inner.collection_name = Some(collection_name);
        self
    }
}

impl Display for MemdxError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.inner.source)?;
        if let Some(ref dispatched_to) = self.inner.dispatched_to {
            write!(f, ", dispatched to: {dispatched_to}")?;
        }
        if let Some(ref dispatched_from) = self.inner.dispatched_from {
            write!(f, ", dispatched from: {dispatched_from}")?;
        }
        Ok(())
    }
}

impl<E> From<E> for Error
where
    ErrorKind: From<E>,
{
    fn from(err: E) -> Self {
        Self {
            kind: Arc::new(err.into()),
            retry_info: None,
        }
    }
}

impl From<AnalyticsError> for Error {
    fn from(value: AnalyticsError) -> Self {
        Self::new(ErrorKind::Analytics(value))
    }
}

impl From<QueryError> for Error {
    fn from(value: QueryError) -> Self {
        Self::new(ErrorKind::Query(value))
    }
}

impl From<HttpError> for Error {
    fn from(value: HttpError) -> Self {
        Self::new(ErrorKind::Http(value))
    }
}

impl From<SearchError> for Error {
    fn from(value: SearchError) -> Self {
        Self::new(ErrorKind::Search(value))
    }
}

impl From<MgmtError> for Error {
    fn from(value: MgmtError) -> Self {
        Self::new(ErrorKind::Mgmt(value))
    }
}

impl From<IndexerError> for Error {
    fn from(value: IndexerError) -> Self {
        Self::new(ErrorKind::Indexer(value))
    }
}
