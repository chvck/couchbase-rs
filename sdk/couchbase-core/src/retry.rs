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

use std::collections::HashSet;
use std::fmt::{Debug, Display};
use std::future::Future;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use crate::errmapcomponent::ErrMapComponent;
use crate::error::{Error, ErrorKind};
use crate::memdx::error::ErrorKind::{Cancelled, Dispatch, Resource, Server};
use crate::memdx::error::{CancellationErrorKind, ServerError, ServerErrorKind};
use crate::retryfailfast::FailFastRetryStrategy;
use crate::tracingcomponent::SPAN_ATTRIB_RETRIES;
use crate::{analyticsx, error, httpx, mgmtx, queryx, searchx};
use async_trait::async_trait;
use tokio::time::sleep;
use tracing::{debug, info};

pub(crate) static DEFAULT_RETRY_STRATEGY: LazyLock<Arc<dyn RetryStrategy>> =
    LazyLock::new(|| Arc::new(FailFastRetryStrategy::default()));

pub(crate) static DEFAULT_RETRY_MANAGER: LazyLock<Arc<dyn RetryManager>> =
    LazyLock::new(|| Arc::new(DefaultRetryManager::default()));

/// The reason an operation is being retried.
///
/// Each variant identifies a specific transient failure condition that triggered
/// a retry. The SDK passes this to [`RetryStrategy::retry_after`] so the strategy
/// can decide whether (and how long) to wait before retrying.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RetryReason {
    /// The server indicated the vBucket is not owned by this node.
    KvNotMyVbucket,
    /// The vBucket map is invalid and must be refreshed.
    KvInvalidVbucketMap,
    /// A temporary failure occurred on the KV engine.
    KvTemporaryFailure,
    /// The collection ID is outdated and must be re-resolved.
    KvCollectionOutdated,
    /// The server error map indicated the operation should be retried.
    KvErrorMapRetryIndicated,
    /// The document is locked by another operation.
    KvLocked,
    /// A sync-write (durable operation) is already in progress on this key.
    KvSyncWriteInProgress,
    /// A sync-write recommit is in progress on this key.
    KvSyncWriteRecommitInProgress,
    /// The required service is temporarily unavailable.
    ServiceNotAvailable,
    /// The connection was closed while the request was in flight.
    SocketClosedWhileInFlight,
    /// No connection is currently available.
    SocketNotAvailable,
    /// A prepared statement for the query was invalidated.
    QueryPreparedStatementFailure,
    /// The query index was not found (may still be building).
    QueryIndexNotFound,
    /// The operation is retryable as indicated by the query engine.
    QueryErrorRetryable,
    /// The search service is rejecting requests due to rate limiting.
    SearchTooManyRequests,
    /// An HTTP request failed to send.
    HttpSendRequestFailed,
    /// An HTTP connection failed to be established.
    HttpConnectFailed,
    /// The SDK is not yet ready to perform the operation.
    NotReady,
}

impl RetryReason {
    /// Returns `true` if this reason allows retrying non-idempotent operations.
    ///
    /// Most retry reasons are safe for non-idempotent retries because the
    /// server never processed the original request.
    pub fn allows_non_idempotent_retry(&self) -> bool {
        matches!(
            self,
            RetryReason::KvInvalidVbucketMap
                | RetryReason::KvNotMyVbucket
                | RetryReason::KvTemporaryFailure
                | RetryReason::KvCollectionOutdated
                | RetryReason::KvErrorMapRetryIndicated
                | RetryReason::KvLocked
                | RetryReason::ServiceNotAvailable
                | RetryReason::SocketNotAvailable
                | RetryReason::KvSyncWriteInProgress
                | RetryReason::KvSyncWriteRecommitInProgress
                | RetryReason::QueryPreparedStatementFailure
                | RetryReason::QueryIndexNotFound
                | RetryReason::QueryErrorRetryable
                | RetryReason::SearchTooManyRequests
                | RetryReason::HttpSendRequestFailed
                | RetryReason::HttpConnectFailed
                | RetryReason::NotReady
        )
    }

    /// Returns `true` if the SDK should always retry for this reason,
    /// regardless of the retry strategy's decision.
    pub fn always_retry(&self) -> bool {
        matches!(
            self,
            RetryReason::KvInvalidVbucketMap
                | RetryReason::KvNotMyVbucket
                | RetryReason::KvCollectionOutdated
        )
    }
}

impl Display for RetryReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RetryReason::KvNotMyVbucket => write!(f, "KV_NOT_MY_VBUCKET"),
            RetryReason::KvInvalidVbucketMap => write!(f, "KV_INVALID_VBUCKET_MAP"),
            RetryReason::KvTemporaryFailure => write!(f, "KV_TEMPORARY_FAILURE"),
            RetryReason::KvCollectionOutdated => write!(f, "KV_COLLECTION_OUTDATED"),
            RetryReason::KvErrorMapRetryIndicated => write!(f, "KV_ERROR_MAP_RETRY_INDICATED"),
            RetryReason::KvLocked => write!(f, "KV_LOCKED"),
            RetryReason::ServiceNotAvailable => write!(f, "SERVICE_NOT_AVAILABLE"),
            RetryReason::SocketClosedWhileInFlight => write!(f, "SOCKET_CLOSED_WHILE_IN_FLIGHT"),
            RetryReason::SocketNotAvailable => write!(f, "SOCKET_NOT_AVAILABLE"),
            RetryReason::KvSyncWriteInProgress => write!(f, "KV_SYNC_WRITE_IN_PROGRESS"),
            RetryReason::KvSyncWriteRecommitInProgress => {
                write!(f, "KV_SYNC_WRITE_RECOMMIT_IN_PROGRESS")
            }
            RetryReason::QueryPreparedStatementFailure => {
                write!(f, "QUERY_PREPARED_STATEMENT_FAILURE")
            }
            RetryReason::QueryIndexNotFound => write!(f, "QUERY_INDEX_NOT_FOUND"),
            RetryReason::QueryErrorRetryable => write!(f, "QUERY_ERROR_RETRYABLE"),
            RetryReason::SearchTooManyRequests => write!(f, "SEARCH_TOO_MANY_REQUESTS"),
            RetryReason::NotReady => write!(f, "NOT_READY"),
            RetryReason::HttpSendRequestFailed => write!(f, "HTTP_SEND_REQUEST_FAILED"),
            RetryReason::HttpConnectFailed => write!(f, "HTTP_CONNECT_FAILED"),
        }
    }
}

/// The action a [`RetryStrategy`] returns to indicate when to retry.
///
/// Contains the [`Duration`] to wait before the next retry attempt.
#[derive(Clone, Debug)]
pub struct RetryAction {
    /// How long to wait before retrying.
    pub duration: Duration,
}

impl RetryAction {
    /// Creates a new `RetryAction` with the given backoff duration.
    pub fn new(duration: Duration) -> Self {
        Self { duration }
    }
}

/// A strategy that decides whether and when to retry a failed operation.
///
/// Implement this trait to provide custom retry behavior. The SDK calls
/// [`retry_after`](RetryStrategy::retry_after) each time a retryable failure
/// occurs, passing the request metadata and the reason for the failure.
///
/// Return `Some(RetryAction)` to retry after the specified duration,
/// or `None` to stop retrying and propagate the error.
///
/// # Example
///
/// ```rust
/// use couchbase_core::retry::{RetryStrategy, RetryAction, RetryRequest, RetryReason};
/// use std::fmt::Debug;
/// use std::time::Duration;
///
/// #[derive(Debug)]
/// struct FixedDelayRetry(Duration);
///
/// impl RetryStrategy for FixedDelayRetry {
///     fn retry_after(&self, request: &RetryRequest, reason: &RetryReason) -> Option<RetryAction> {
///         if request.retry_attempts < 3 {
///             Some(RetryAction::new(self.0))
///         } else {
///             None // give up after 3 attempts
///         }
///     }
/// }
/// ```
pub trait RetryStrategy: Debug + Send + Sync {
    /// Decides whether to retry an operation and how long to wait.
    ///
    /// * `request` — Metadata about the in-flight request (attempt count, idempotency, etc.).
    /// * `reason` — Why the operation failed.
    ///
    /// Return `Some(RetryAction)` to retry, or `None` to stop.
    fn retry_after(&self, request: &RetryRequest, reason: &RetryReason) -> Option<RetryAction>;
}

/// Metadata about a request that is being considered for retry.
#[derive(Clone, Debug)]
pub struct RetryRequest {
    pub(crate) operation: &'static str,
    /// Whether the operation is idempotent (safe to retry without side effects).
    pub is_idempotent: bool,
    /// The number of retry attempts that have already been made.
    pub retry_attempts: u32,
    /// The set of reasons this request has been retried so far.
    pub retry_reasons: HashSet<RetryReason>,
    pub(crate) unique_id: Option<String>,
}

impl RetryRequest {
    /// **Public because [`RetryManager`] is.** A trait an embedder can implement
    /// but whose argument it cannot construct is only half published: there is no
    /// way to unit-test the implementation without a live cluster and a provoked
    /// failure. `operation` is the name that appears in retry logs.
    pub fn new(operation: &'static str, is_idempotent: bool) -> Self {
        Self {
            operation,
            is_idempotent,
            retry_attempts: 0,
            retry_reasons: Default::default(),
            unique_id: None,
        }
    }

    /// Record that this request is being retried, and why.
    ///
    /// Public for the same reason as [`Self::new`]: an implementation of
    /// [`RetryManager`] that retries must call this, or the attempt count and the
    /// reasons seen are not attached to the error when the operation finally
    /// fails, and a caller loses the only account of what the client did.
    pub fn add_retry_attempt(&mut self, reason: RetryReason) {
        self.retry_attempts += 1;
        tracing::Span::current().record(SPAN_ATTRIB_RETRIES, self.retry_attempts);
        self.retry_reasons.insert(reason);
    }

    pub fn is_idempotent(&self) -> bool {
        self.is_idempotent
    }

    pub fn retry_attempts(&self) -> u32 {
        self.retry_attempts
    }

    pub fn retry_reasons(&self) -> &HashSet<RetryReason> {
        &self.retry_reasons
    }
}

impl Display for RetryRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{ operation: {}, id: {}, is_idempotent: {}, retry_attempts: {}, retry_reasons: {} }}",
            self.operation,
            self.unique_id.as_ref().unwrap_or(&"".to_string()),
            self.is_idempotent,
            self.retry_attempts,
            self.retry_reasons
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Decides whether a failed operation is retried, and after how long.
///
/// **This is the layer above [`RetryStrategy`], and the distinction is the reason
/// it is pluggable.** A strategy answers "given a reason, retry?" and is consulted
/// per request. A manager owns the policy *around* that question — including which
/// reasons are decided without asking the strategy at all, which
/// [`DefaultRetryManager`] uses for the conditions a client must handle to remain
/// correct.
///
/// **Replace it when the embedder already owns the recovery.** A gateway that
/// provisions a missing keyspace and reissues the write itself does not want the
/// client retrying `KvCollectionOutdated` underneath it: the retry cannot succeed
/// until the keyspace exists, and only the embedder can create it. Supply an
/// implementation through
/// [`AgentOptions::retry_manager`](crate::options::agent::AgentOptions::retry_manager).
///
/// Implementations must be cheap to call and must not block: this runs on the
/// operation's own task between attempts.
pub trait RetryManager: Debug + Send + Sync {
    /// `Some(duration)` to retry after waiting, `None` to fail the operation.
    ///
    /// `request` is the in-flight request's metadata; an implementation that
    /// retries must call [`RetryRequest::add_retry_attempt`] so the attempt count
    /// and the reasons seen are attached to the error if it eventually fails.
    fn maybe_retry(
        &self,
        strategy: &Arc<dyn RetryStrategy>,
        request: &mut RetryRequest,
        reason: RetryReason,
    ) -> Option<Duration>;
}

/// The manager the SDK uses unless an embedder supplies another.
///
/// Consults the request's [`RetryStrategy`], except for the reasons
/// [`RetryReason::always_retry`] names — a stale vbucket map, a vbucket that has
/// moved, a stale collection id — which are retried on a controlled backoff
/// without asking. Those describe a client whose own routing state is behind the
/// cluster's, and a client that gave up on them would report a failure the caller
/// can do nothing about.
#[derive(Debug, Default)]
pub struct DefaultRetryManager {}

impl RetryManager for DefaultRetryManager {
    fn maybe_retry(
        &self,
        strategy: &Arc<dyn RetryStrategy>,
        request: &mut RetryRequest,
        reason: RetryReason,
    ) -> Option<Duration> {
        if reason.always_retry() {
            request.add_retry_attempt(reason);
            let backoff = controlled_backoff(request.retry_attempts);

            return Some(backoff);
        }

        let action = strategy.retry_after(request, &reason);

        if let Some(a) = action {
            request.add_retry_attempt(reason);

            return Some(a.duration);
        }

        None
    }
}

/// What the service components carry: the error map that classifies a failure
/// into a [`RetryReason`], and the manager that decides what to do about one.
///
/// **Two fields rather than one object, because they are two concerns.** The
/// error map answers "does the server call this status retryable" — a property of
/// the wire, and the same answer for every embedder. The manager answers "so do
/// we retry" — a policy, and the embedder's to replace. They travel together only
/// because the retry loop needs both.
#[derive(Debug)]
pub(crate) struct RetryComponent {
    err_map: Arc<ErrMapComponent>,
    manager: Arc<dyn RetryManager>,
}

impl RetryComponent {
    pub(crate) fn new(err_map: Arc<ErrMapComponent>, manager: Arc<dyn RetryManager>) -> Self {
        Self { err_map, manager }
    }

    /// The classifier, for the one caller that runs its own retry loop.
    pub(crate) fn err_map(&self) -> &ErrMapComponent {
        &self.err_map
    }

    /// The decision, for the same caller.
    pub(crate) fn maybe_retry(
        &self,
        strategy: &Arc<dyn RetryStrategy>,
        request: &mut RetryRequest,
        reason: RetryReason,
    ) -> Option<Duration> {
        self.manager.maybe_retry(strategy, request, reason)
    }
}

pub(crate) async fn orchestrate_retries<Fut, Resp>(
    rs: Arc<RetryComponent>,
    strategy: Arc<dyn RetryStrategy>,
    mut retry_info: RetryRequest,
    operation: impl Fn() -> Fut + Send + Sync,
) -> error::Result<Resp>
where
    Fut: Future<Output = error::Result<Resp>> + Send,
    Resp: Send,
{
    loop {
        let mut err = match operation().await {
            Ok(r) => {
                return Ok(r);
            }
            Err(e) => e,
        };

        if let Some(reason) = error_to_retry_reason(&rs.err_map, &mut retry_info, &err) {
            if let Some(duration) = rs.manager.maybe_retry(&strategy, &mut retry_info, reason) {
                debug!(
                    "Retrying {} after {:?} due to {}",
                    &retry_info, duration, reason
                );
                sleep(duration).await;
                continue;
            }
        }

        if retry_info.retry_attempts > 0 {
            // If we aren't retrying then attach any retry info that we have.
            err.set_retry_info(retry_info);
        }

        return Err(err);
    }
}

pub(crate) fn error_to_retry_reason(
    err_map: &ErrMapComponent,
    retry_info: &mut RetryRequest,
    err: &Error,
) -> Option<RetryReason> {
    match err.kind() {
        ErrorKind::Memdx(err) => {
            retry_info.unique_id = err.has_opaque().map(|o| o.to_string());

            match err.kind() {
                Server(e) => return server_error_to_retry_reason(err_map, e),
                Resource(e) => return server_error_to_retry_reason(err_map, e.cause()),
                Cancelled(e) if e == &CancellationErrorKind::ClosedInFlight => {
                    return Some(RetryReason::SocketClosedWhileInFlight);
                }
                Dispatch { .. } => return Some(RetryReason::SocketNotAvailable),
                _ => {}
            }
        }
        ErrorKind::NoVbucketMap => {
            return Some(RetryReason::KvInvalidVbucketMap);
        }
        ErrorKind::ServiceNotAvailable { .. } => {
            return Some(RetryReason::ServiceNotAvailable);
        }
        ErrorKind::Query(e) => match e.kind() {
            queryx::error::ErrorKind::Server(e) => match e.kind() {
                queryx::error::ServerErrorKind::PreparedStatementFailure => {
                    return Some(RetryReason::QueryPreparedStatementFailure);
                }
                queryx::error::ServerErrorKind::IndexNotFound => {
                    return Some(RetryReason::QueryIndexNotFound);
                }
                _ => {
                    if e.retry() {
                        return Some(RetryReason::QueryErrorRetryable);
                    }
                }
            },
            queryx::error::ErrorKind::Http { error, .. } => match error.kind() {
                httpx::error::ErrorKind::SendRequest(_) => {
                    return Some(RetryReason::HttpSendRequestFailed);
                }
                httpx::error::ErrorKind::Connect { .. } => {
                    return Some(RetryReason::HttpConnectFailed);
                }
                _ => {}
            },
            _ => {}
        },
        ErrorKind::Search(e) => match e.kind() {
            searchx::error::ErrorKind::Server(e) if e.status_code() == 429 => {
                return Some(RetryReason::SearchTooManyRequests);
            }
            searchx::error::ErrorKind::Http { error, .. } => match error.kind() {
                httpx::error::ErrorKind::SendRequest(_) => {
                    return Some(RetryReason::HttpSendRequestFailed);
                }
                httpx::error::ErrorKind::Connect { .. } => {
                    return Some(RetryReason::HttpConnectFailed);
                }
                _ => {}
            },
            _ => {}
        },
        ErrorKind::Analytics(e) => {
            if let analyticsx::error::ErrorKind::Http { error, .. } = e.kind() {
                match error.kind() {
                    httpx::error::ErrorKind::SendRequest(_) => {
                        return Some(RetryReason::HttpSendRequestFailed);
                    }
                    httpx::error::ErrorKind::Connect { .. } => {
                        return Some(RetryReason::HttpConnectFailed);
                    }
                    _ => {}
                }
            }
        }
        ErrorKind::Mgmt(e) => {
            if let mgmtx::error::ErrorKind::Http(error) = e.kind() {
                match error.kind() {
                    httpx::error::ErrorKind::SendRequest(_) => {
                        return Some(RetryReason::HttpSendRequestFailed);
                    }
                    httpx::error::ErrorKind::Connect { .. } => {
                        return Some(RetryReason::HttpConnectFailed);
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    None
}

fn server_error_to_retry_reason(err_map: &ErrMapComponent, e: &ServerError) -> Option<RetryReason> {
    match e.kind() {
        ServerErrorKind::NotMyVbucket => {
            return Some(RetryReason::KvNotMyVbucket);
        }
        ServerErrorKind::TmpFail => {
            return Some(RetryReason::KvTemporaryFailure);
        }
        ServerErrorKind::UnknownCollectionID => {
            return Some(RetryReason::KvCollectionOutdated);
        }
        ServerErrorKind::UnknownCollectionName => {
            return Some(RetryReason::KvCollectionOutdated);
        }
        ServerErrorKind::UnknownScopeName => {
            return Some(RetryReason::KvCollectionOutdated);
        }
        ServerErrorKind::Locked => {
            return Some(RetryReason::KvLocked);
        }
        ServerErrorKind::SyncWriteInProgress => {
            return Some(RetryReason::KvSyncWriteInProgress);
        }
        ServerErrorKind::SyncWriteRecommitInProgress => {
            return Some(RetryReason::KvSyncWriteRecommitInProgress);
        }
        ServerErrorKind::UnknownStatus { status } if err_map.should_retry(status) => {
            return Some(RetryReason::KvErrorMapRetryIndicated);
        }
        _ => {}
    }

    None
}

pub(crate) fn controlled_backoff(retry_attempts: u32) -> Duration {
    match retry_attempts {
        0 => Duration::from_millis(1),
        1 => Duration::from_millis(10),
        2 => Duration::from_millis(50),
        3 => Duration::from_millis(100),
        4 => Duration::from_millis(500),
        _ => Duration::from_millis(1000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;
    use crate::queryx;
    use http::StatusCode;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// **A supplied manager overrides the reasons the default retries unasked.**
    ///
    /// This is the whole point of the trait and it needs pinning, because the
    /// difference is invisible from the strategy: `always_retry` reasons are
    /// decided *before* the strategy is consulted, so no `RetryStrategy` can
    /// change them. `DefaultRetryManager` retries `KvCollectionOutdated`
    /// indefinitely on a controlled backoff — correct for a client whose routing
    /// state is merely behind, and a hang for an embedder whose keyspace was
    /// dropped and will not come back until *it* recreates it.
    #[test]
    fn a_supplied_manager_can_decline_what_the_default_always_retries() {
        #[derive(Debug)]
        struct DeclineCollectionOutdated;

        impl RetryManager for DeclineCollectionOutdated {
            fn maybe_retry(
                &self,
                strategy: &Arc<dyn RetryStrategy>,
                request: &mut RetryRequest,
                reason: RetryReason,
            ) -> Option<Duration> {
                if reason == RetryReason::KvCollectionOutdated {
                    return None;
                }
                DefaultRetryManager::default().maybe_retry(strategy, request, reason)
            }
        }

        let strategy: Arc<dyn RetryStrategy> = Arc::new(FailFastRetryStrategy::default());

        // The default retries it without asking the strategy — which is what
        // makes it unreachable from a strategy, and why this trait exists.
        let mut request = RetryRequest::new("upsert", false);
        assert!(
            DefaultRetryManager::default()
                .maybe_retry(&strategy, &mut request, RetryReason::KvCollectionOutdated)
                .is_some(),
            "the default retries a stale collection id"
        );

        let mut request = RetryRequest::new("upsert", false);
        assert!(
            DeclineCollectionOutdated
                .maybe_retry(&strategy, &mut request, RetryReason::KvCollectionOutdated)
                .is_none(),
            "a supplied manager declines it, so the caller sees the error"
        );

        // And only that reason: everything else still reaches the default, so
        // replacing the manager is not a way to lose the retries that matter.
        let mut request = RetryRequest::new("upsert", false);
        assert!(
            DeclineCollectionOutdated
                .maybe_retry(&strategy, &mut request, RetryReason::KvNotMyVbucket)
                .is_some(),
            "a moved vbucket is still retried"
        );
    }

    fn make_retry_manager() -> Arc<RetryComponent> {
        Arc::new(RetryComponent::new(
            Arc::new(ErrMapComponent::default()),
            Arc::new(DefaultRetryManager::default()),
        ))
    }

    fn make_query_server_error(kind: queryx::error::ServerErrorKind, retry: bool) -> Error {
        let server_error = queryx::error::ServerError::new(
            kind,
            "localhost:8093",
            StatusCode::INTERNAL_SERVER_ERROR,
            12345,
            retry,
            "test error",
        );
        queryx::error::Error::new_server_error(server_error).into()
    }

    #[test]
    fn test_query_error_retryable_when_retry_true() {
        let rs = make_retry_manager();
        let mut retry_info = RetryRequest::new("query", false);
        let err = make_query_server_error(queryx::error::ServerErrorKind::Unknown, true);

        let reason = error_to_retry_reason(rs.err_map(), &mut retry_info, &err);
        assert_eq!(reason, Some(RetryReason::QueryErrorRetryable));
    }

    #[test]
    fn test_query_error_not_retryable_when_retry_false() {
        let rs = make_retry_manager();
        let mut retry_info = RetryRequest::new("query", false);
        let err = make_query_server_error(queryx::error::ServerErrorKind::Unknown, false);

        let reason = error_to_retry_reason(rs.err_map(), &mut retry_info, &err);
        assert_eq!(reason, None);
    }

    #[test]
    fn test_query_prepared_statement_failure_ignores_retry_flag() {
        let rs = make_retry_manager();
        let mut retry_info = RetryRequest::new("query", false);
        let err = make_query_server_error(
            queryx::error::ServerErrorKind::PreparedStatementFailure,
            false,
        );

        let reason = error_to_retry_reason(rs.err_map(), &mut retry_info, &err);
        assert_eq!(reason, Some(RetryReason::QueryPreparedStatementFailure));
    }

    #[test]
    fn test_query_index_not_found_ignores_retry_flag() {
        let rs = make_retry_manager();
        let mut retry_info = RetryRequest::new("query", false);
        let err = make_query_server_error(queryx::error::ServerErrorKind::IndexNotFound, false);

        let reason = error_to_retry_reason(rs.err_map(), &mut retry_info, &err);
        assert_eq!(reason, Some(RetryReason::QueryIndexNotFound));
    }

    #[test]
    fn test_query_error_retryable_allows_non_idempotent_retry() {
        assert!(RetryReason::QueryErrorRetryable.allows_non_idempotent_retry());
    }

    #[test]
    fn test_query_error_retryable_does_not_always_retry() {
        assert!(!RetryReason::QueryErrorRetryable.always_retry());
    }
    /// Ported from cbcore-rs `src/retry/manager.rs::test_orchestrate_retries`.
    ///
    /// The six tests above pin which errors are *classified* as retryable. None
    /// of them runs the loop, so nothing pinned that a retryable failure is
    /// actually re-attempted, that the retry stops as soon as the operation
    /// succeeds, or that the strategy is consulted once per failure.
    #[tokio::test]
    async fn a_retryable_failure_is_re_attempted_until_it_succeeds() {
        let strategy = CountingStrategy::always();
        let calls = Arc::new(AtomicU32::new(0));

        let calls_for_op = calls.clone();
        let res: Result<&str> = orchestrate_retries(
            make_retry_manager(),
            strategy.clone() as Arc<dyn RetryStrategy>,
            RetryRequest::new("test", true),
            || {
                let calls = calls_for_op.clone();
                async move {
                    if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                        Err(make_query_server_error(
                            queryx::error::ServerErrorKind::Internal,
                            true,
                        ))
                    } else {
                        Ok("done")
                    }
                }
            },
        )
        .await;

        assert_eq!("done", res.unwrap());
        assert_eq!(3, calls.load(Ordering::SeqCst), "operation call count");
        assert_eq!(2, strategy.consultations(), "strategy consultations");
    }

    /// The other end of the same loop: when the strategy declines, the caller
    /// gets the error — and it carries how many attempts were spent on it.
    #[tokio::test]
    async fn a_strategy_that_declines_ends_the_loop_and_reports_the_attempts() {
        let strategy = CountingStrategy::giving_up_after(2);
        let calls = Arc::new(AtomicU32::new(0));

        let calls_for_op = calls.clone();
        let res: Result<&str> = orchestrate_retries(
            make_retry_manager(),
            strategy.clone() as Arc<dyn RetryStrategy>,
            RetryRequest::new("test", true),
            || {
                let calls = calls_for_op.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err(make_query_server_error(
                        queryx::error::ServerErrorKind::Internal,
                        true,
                    ))
                }
            },
        )
        .await;

        let err = res.expect_err("expected the failure to reach the caller");
        assert_eq!(3, calls.load(Ordering::SeqCst), "operation call count");
        assert_eq!(
            Some(2),
            err.retry_info().map(|r| r.retry_attempts()),
            "the error should carry its retry history, got {err:?}"
        );
    }

    /// A strategy that answers every failure, counting how often it was asked
    /// and optionally giving up after a fixed number of retries.
    #[derive(Debug)]
    struct CountingStrategy {
        consultations: AtomicU32,
        max_retries: Option<u32>,
    }

    impl CountingStrategy {
        fn always() -> Arc<Self> {
            Arc::new(Self {
                consultations: AtomicU32::new(0),
                max_retries: None,
            })
        }

        fn giving_up_after(max_retries: u32) -> Arc<Self> {
            Arc::new(Self {
                consultations: AtomicU32::new(0),
                max_retries: Some(max_retries),
            })
        }

        fn consultations(&self) -> u32 {
            self.consultations.load(Ordering::SeqCst)
        }
    }

    impl RetryStrategy for CountingStrategy {
        fn retry_after(
            &self,
            request: &RetryRequest,
            _reason: &RetryReason,
        ) -> Option<RetryAction> {
            self.consultations.fetch_add(1, Ordering::SeqCst);

            match self.max_retries {
                Some(max) if request.retry_attempts() >= max => None,
                _ => Some(RetryAction::new(Duration::ZERO)),
            }
        }
    }
}
