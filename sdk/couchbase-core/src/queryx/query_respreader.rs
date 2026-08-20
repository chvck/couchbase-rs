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

use std::pin::{pin, Pin};
use std::ptr::read;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use bytes::Bytes;
use futures::future::err;
use futures::{FutureExt, Stream, StreamExt, TryStreamExt};
use http::StatusCode;
use regex::Regex;
use tokio::sync::Mutex;
use tracing::debug;

use crate::helpers::durations::parse_duration_from_golang_string;
use crate::httpx;
use crate::httpx::decoder::Decoder;
use crate::httpx::raw_json_row_streamer::{RawJsonRowItem, RawJsonRowStreamer};
use crate::httpx::response::Response;
use crate::memdx::magic::Magic::Res;
use crate::queryx::error;
use crate::queryx::error::{
    Error, ErrorDesc, ErrorKind, ResourceError, ServerError, ServerErrorKind,
};
use crate::queryx::query_json::{
    QueryEarlyMetaData, QueryError, QueryErrorResponse, QueryMetaData, QueryMetrics, QueryWarning,
};
use crate::queryx::query_result::{EarlyMetaData, MetaData, Metrics, Warning};

pub struct QueryRespReader {
    endpoint: String,
    statement: String,
    client_context_id: String,
    status_code: StatusCode,

    streamer: Pin<Box<dyn Stream<Item = httpx::error::Result<RawJsonRowItem>> + Send>>,
    early_meta_data: EarlyMetaData,
    meta_data: Option<MetaData>,
    meta_data_error: Option<Error>,
}

impl Stream for QueryRespReader {
    type Item = error::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        match this.streamer.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(RawJsonRowItem::Row(row_data)))) => Poll::Ready(Some(Ok(row_data))),
            Poll::Ready(Some(Ok(RawJsonRowItem::Metadata(metadata)))) => {
                match this.read_final_metadata(metadata) {
                    Ok(meta) => this.meta_data = Some(meta),
                    Err(e) => {
                        this.meta_data_error = Some(e.clone());
                        return Poll::Ready(Some(Err(e)));
                    }
                };
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(Error::new_http_error(
                e,
                this.endpoint.clone(),
                Some(this.statement.clone()),
                this.client_context_id.clone(),
            )))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl QueryRespReader {
    pub async fn new(
        resp: Response,
        endpoint: impl Into<String>,
        statement: impl Into<String>,
        client_context_id: impl Into<String>,
    ) -> error::Result<Self> {
        let status_code = resp.status();
        let endpoint = endpoint.into();
        let statement = statement.into();
        let client_context_id = client_context_id.into();
        if status_code != 200 {
            let body = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    debug!("Failed to read response body on error {}", &e);
                    return Err(Error::new_http_error(
                        e,
                        endpoint,
                        statement,
                        client_context_id,
                    ));
                }
            };

            let errors: QueryErrorResponse = match serde_json::from_slice(&body) {
                Ok(e) => e,
                Err(e) => {
                    return Err(Error::new_message_error(
                        format!(
                        "non-200 status code received {status_code} but parsing error response body failed {e}"
                    ),
                        None,
                        None,
                        None,
                    ));
                }
            };

            if errors.errors.is_empty() {
                return Err(Error::new_message_error(
                    format!(
                        "Non-200 status code received {status_code} but response body contained no errors",
                    ),
                    None,
                    None,
                    None,
                ));
            }

            return Err(Self::parse_errors(
                &errors.errors,
                endpoint,
                statement,
                client_context_id,
                status_code,
            ));
        }

        let stream = resp.bytes_stream();
        let mut streamer = RawJsonRowStreamer::new(Decoder::new(stream), "results");

        let early_meta_data =
            Self::read_early_metadata(&mut streamer, &endpoint, &statement, &client_context_id)
                .await?;

        let has_more_rows = streamer.has_more_rows().await;
        let mut epilog = None;
        if !has_more_rows {
            epilog = match streamer.epilog() {
                Ok(epilog) => Some(epilog),
                Err(e) => {
                    return Err(Error::new_http_error(
                        e,
                        endpoint,
                        statement,
                        client_context_id,
                    ));
                }
            };
        }

        let mut reader = Self {
            endpoint,
            statement,
            client_context_id,
            status_code,
            streamer: Box::pin(streamer.into_stream()),
            early_meta_data,
            meta_data: None,
            meta_data_error: None,
        };

        if let Some(epilog) = epilog {
            let meta = reader.read_final_metadata(epilog)?;

            reader.meta_data = Some(meta);
        }

        Ok(reader)
    }

    pub fn early_metadata(&self) -> &EarlyMetaData {
        &self.early_meta_data
    }

    pub fn metadata(&self) -> error::Result<&MetaData> {
        if let Some(e) = &self.meta_data_error {
            return Err(e.clone());
        }

        if let Some(meta) = &self.meta_data {
            return Ok(meta);
        }

        Err(Error::new_message_error(
            "cannot read meta-data until after all rows are read",
            None,
            None,
            None,
        ))
    }

    async fn read_early_metadata(
        streamer: &mut RawJsonRowStreamer,
        endpoint: &str,
        statement: &str,
        client_context_id: &str,
    ) -> error::Result<EarlyMetaData> {
        let prelude = streamer.read_prelude().await.map_err(|e| {
            Error::new_http_error(
                e,
                endpoint,
                statement.to_string(),
                client_context_id.to_string(),
            )
        })?;

        let early_metadata: QueryEarlyMetaData = serde_json::from_slice(&prelude).map_err(|e| {
            Error::new_message_error(
                format!("failed to parse metadata from response: {e}"),
                endpoint.to_string(),
                statement.to_string(),
                client_context_id.to_string(),
            )
        })?;

        Ok(EarlyMetaData {
            prepared: early_metadata.prepared,
        })
    }

    fn read_final_metadata(&mut self, epilog: Vec<u8>) -> error::Result<MetaData> {
        let metadata: QueryMetaData = match serde_json::from_slice(&epilog) {
            Ok(m) => m,
            Err(e) => {
                return Err(Error::new_message_error(
                    format!("failed to parse query metadata from epilog: {e}"),
                    self.endpoint.clone(),
                    self.statement.clone(),
                    self.client_context_id.clone(),
                ));
            }
        };

        self.parse_metadata(metadata)
    }

    fn parse_metadata(&self, metadata: QueryMetaData) -> error::Result<MetaData> {
        if !metadata.errors.is_empty() {
            return Err(Self::parse_errors(
                &metadata.errors,
                &self.endpoint,
                &self.statement,
                &self.client_context_id,
                self.status_code,
            ));
        }

        let metrics = self.parse_metrics(metadata.metrics);
        let warnings = self.parse_warnings(metadata.warnings);

        Ok(MetaData {
            prepared: metadata.early_meta_data.prepared,
            request_id: metadata.request_id.unwrap_or_default(),
            client_context_id: metadata.client_context_id.unwrap_or_default(),
            status: metadata.status,
            metrics,
            signature: metadata.signature,
            warnings,
            profile: metadata.profile,
        })
    }

    fn parse_metrics(&self, metrics: Option<QueryMetrics>) -> Option<Metrics> {
        metrics.map(|m| {
            let elapsed_time = if let Some(elapsed) = m.elapsed_time {
                parse_duration_from_golang_string(&elapsed).unwrap_or_default()
            } else {
                Duration::default()
            };

            let execution_time = if let Some(execution) = m.execution_time {
                parse_duration_from_golang_string(&execution).unwrap_or_default()
            } else {
                Duration::default()
            };

            Metrics {
                elapsed_time,
                execution_time,
                result_count: m.result_count.unwrap_or_default(),
                result_size: m.result_size.unwrap_or_default(),
                mutation_count: m.mutation_count.unwrap_or_default(),
                sort_count: m.sort_count.unwrap_or_default(),
                error_count: m.error_count.unwrap_or_default(),
                warning_count: m.warning_count.unwrap_or_default(),
            }
        })
    }

    fn parse_warnings(&self, warnings: Vec<QueryWarning>) -> Vec<Warning> {
        let mut converted = vec![];
        for w in warnings {
            converted.push(Warning {
                code: w.code.unwrap_or_default(),
                message: w.msg.unwrap_or_default(),
            });
        }

        converted
    }

    fn parse_errors(
        errors: &[QueryError],
        endpoint: impl Into<String>,
        statement: impl Into<String>,
        client_context_id: impl Into<String>,
        status_code: StatusCode,
    ) -> Error {
        let error_descs: Vec<ErrorDesc> = errors
            .iter()
            .map(|error| {
                ErrorDesc::new(
                    Self::parse_error_kind(error),
                    error.code,
                    error.msg.clone(),
                    error.retry.unwrap_or_default(),
                    error.reason.clone(),
                )
            })
            .collect();

        let chosen_desc = error_descs
            .iter()
            .find(|desc| !desc.retry())
            .unwrap_or(&error_descs[0]);

        let mut server_error = ServerError::new(
            chosen_desc.kind().clone(),
            endpoint,
            status_code,
            chosen_desc.code(),
            chosen_desc.retry(),
            chosen_desc.message(),
        )
        .with_client_context_id(client_context_id)
        .with_statement(statement);

        if error_descs.len() > 1 {
            server_error = server_error.with_error_descs(error_descs);
        }

        match server_error.kind() {
            ServerErrorKind::ScopeNotFound => {
                Error::new_resource_error(ResourceError::new(server_error))
            }
            ServerErrorKind::CollectionNotFound => {
                Error::new_resource_error(ResourceError::new(server_error))
            }
            ServerErrorKind::IndexNotFound => {
                Error::new_resource_error(ResourceError::new(server_error))
            }
            ServerErrorKind::IndexExists => {
                Error::new_resource_error(ResourceError::new(server_error))
            }
            ServerErrorKind::AuthenticationFailure => {
                if server_error.code() == 13014 {
                    Error::new_resource_error(ResourceError::new(server_error))
                } else {
                    Error::new_server_error(server_error)
                }
            }
            _ => Error::new_server_error(server_error),
        }
    }

    fn parse_error_kind(error: &QueryError) -> ServerErrorKind {
        let err_code = error.code;
        let err_code_group = err_code / 1000;

        if err_code_group == 4 {
            if err_code == 4040
                || err_code == 4050
                || err_code == 4060
                || err_code == 4070
                || err_code == 4080
                || err_code == 4090
            {
                ServerErrorKind::PreparedStatementFailure
            } else if err_code == 4300 {
                ServerErrorKind::IndexExists
            } else {
                ServerErrorKind::PlanningFailure
            }
        } else if err_code_group == 5 {
            let msg = error.msg.to_lowercase();
            if msg.contains("not enough") && msg.contains("replica") {
                ServerErrorKind::InvalidArgument {
                    argument: "num_replicas".to_string(),
                    reason: "not enough indexer nodes to create index with replica count"
                        .to_string(),
                }
            } else if msg.contains("build already in progress") {
                ServerErrorKind::BuildAlreadyInProgress
            } else if msg.contains("build index fails")
                && msg.contains("index will be retried building")
            {
                ServerErrorKind::BuildFails
            // **Order matters, and the general "index" tests have to stay
            // below these.** A keyspace failure raised by an index create
            // arrives wrapped — `Index creation for index X, bucket Y ...
            // cannot start. Reason: <the real one>` — and a concurrent create
            // says `another concurrent create index request` outright. Both
            // carry the word "index", so a general test placed above them
            // would answer first and neither of these could be reached.
            //
            // What is classified is the reason the wrapper carries, which is
            // why these read the whole message: a wrapper around a transient
            // cause is transient, and one around a fatal cause stays
            // `Internal` rather than being retried on the strength of its
            // envelope.
            } else if msg.contains("collection not found") || msg.contains("keyspace not found") {
                ServerErrorKind::CollectionNotFound
            } else if msg.contains("concurrent create index") {
                ServerErrorKind::ConcurrentOperation
            } else if Regex::new(".*?ndex .*? already exist.*")
                .unwrap()
                .is_match(&error.msg)
            {
                ServerErrorKind::IndexExists
            } else if Regex::new(".*?ndex .*? not exist.*")
                .unwrap()
                .is_match(&error.msg)
            {
                ServerErrorKind::IndexNotFound
            } else {
                ServerErrorKind::Internal
            }
        } else if err_code_group == 12 {
            if err_code == 12003 {
                ServerErrorKind::CollectionNotFound
            } else if err_code == 12004 {
                ServerErrorKind::IndexNotFound
            } else if err_code == 12009 {
                if !error.reason.is_empty() {
                    if let Some(code) = error.reason.get("code") {
                        if code == 12033 {
                            ServerErrorKind::CasMismatch
                        } else if code == 17014 {
                            ServerErrorKind::DocNotFound
                        } else if code == 17012 {
                            ServerErrorKind::DocExists
                        } else {
                            ServerErrorKind::DMLFailure
                        }
                    } else {
                        ServerErrorKind::DMLFailure
                    }
                } else if error.msg.to_lowercase().contains("cas mismatch") {
                    ServerErrorKind::CasMismatch
                } else {
                    ServerErrorKind::DMLFailure
                }
            } else if err_code == 12016 {
                ServerErrorKind::IndexNotFound
            } else if err_code == 12021 {
                ServerErrorKind::ScopeNotFound
            } else {
                ServerErrorKind::IndexFailure
            }
        } else if err_code_group == 14 {
            ServerErrorKind::IndexFailure
        } else if err_code_group == 10 {
            ServerErrorKind::AuthenticationFailure
        } else if err_code == 1000 {
            ServerErrorKind::WriteInReadOnlyMode
        } else if err_code == 1080 {
            ServerErrorKind::Timeout
        } else if err_code == 3000 {
            ServerErrorKind::ParsingFailure
        } else if err_code == 13014 {
            ServerErrorKind::AuthenticationFailure
        } else if err_code == 2120 {
            // "Error authorizing against cluster cause: Failure to authenticate
            // user" — a refusal, and one a caller has to be able to report as
            // one. Group 2 otherwise falls through to `Unknown`, which reads as
            // "something went wrong" rather than "you may not".
            ServerErrorKind::AuthenticationFailure
        } else {
            ServerErrorKind::Unknown
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queryx::query_result::Status;
    use futures::StreamExt;

    /// One error entry, built from the wire shape the query service actually
    /// sends rather than from the struct, so the `serde` attributes are part of
    /// what is pinned.
    fn wire_error(json: &str) -> QueryError {
        serde_json::from_str(json).expect("error entry did not parse")
    }

    /// Classify one error entry the way `parse_errors` does, and hand back the
    /// resulting error.
    fn classify(json: &str) -> Error {
        QueryRespReader::parse_errors(
            &[wire_error(json)],
            "localhost:8093",
            "SELECT 1",
            "ctx",
            StatusCode::OK,
        )
    }

    fn server_kind(err: &Error) -> ServerErrorKind {
        match err.kind() {
            ErrorKind::Server(e) => e.kind().clone(),
            ErrorKind::Resource(e) => e.cause().kind().clone(),
            other => panic!("expected a server or resource error, got {other:?}"),
        }
    }

    fn resource(err: &Error) -> &ResourceError {
        match err.kind() {
            ErrorKind::Resource(e) => e,
            other => panic!("expected a resource error, got {other:?}"),
        }
    }

    /// **The four conditions a 5xxx message carries that the code does not.**
    ///
    /// Ported from cbcore-rs, where each of these was reached by a message test
    /// and each one names a recovery: a build the indexer says it will retry, a
    /// concurrent create to wait out, and a keyspace failure that arrives wrapped
    /// in an index-creation envelope. Folded into `Internal`, all three read as
    /// "something went wrong", and a caller waits for none of them.
    #[test]
    fn a_5xxx_message_is_classified_by_what_it_says() {
        for (msg, want) in [
            (
                "build already in progress",
                ServerErrorKind::BuildAlreadyInProgress,
            ),
            (
                "build index fails. index will be retried building",
                ServerErrorKind::BuildFails,
            ),
            (
                "another concurrent create index request is in progress",
                ServerErrorKind::ConcurrentOperation,
            ),
            (
                "Index creation for index ix_1, bucket b, scope s, collection c \
                 cannot start. Reason: Keyspace not found in CB datastore",
                ServerErrorKind::CollectionNotFound,
            ),
            ("something nobody has classified", ServerErrorKind::Internal),
        ] {
            let json = format!(
                r#"{{"code":5000,"msg":{}}}"#,
                serde_json::to_string(msg).unwrap()
            );
            let got = classify(&json);
            assert_eq!(server_kind(&got), want, "for {msg:?}");
        }
    }

    /// **The general "index" tests have to stay below the specific ones.**
    ///
    /// Both messages above carry the word "index", so a test for "an index that
    /// already exists" placed first would answer for them too — which is how the
    /// previous client's ladder was ordered, and why its comment says so. This
    /// holds the order rather than the comment.
    #[test]
    fn a_specific_5xxx_message_is_not_answered_by_the_general_one() {
        let concurrent = classify(
            r#"{"code":5000,"msg":"another concurrent create index request already exists"}"#,
        );
        assert_eq!(
            server_kind(&concurrent),
            ServerErrorKind::ConcurrentOperation,
            "a message that mentions both must take the specific arm"
        );
    }

    /// 2120 is a refusal, and group 2 otherwise falls through to `Unknown`.
    ///
    /// The distinction is what lets a caller answer "you may not" rather than
    /// "something went wrong" — measured against 8.0.3, an unauthenticated
    /// request to the query service is answered `2120 Error authorizing against
    /// cluster cause: Failure to authenticate user`.
    #[test]
    fn a_2120_is_an_authentication_failure_and_not_an_unknown() {
        let refused = classify(
            r#"{"code":2120,"msg":"Error authorizing against cluster cause: Failure to authenticate user"}"#,
        );
        assert_eq!(
            server_kind(&refused),
            ServerErrorKind::AuthenticationFailure
        );
    }

    /// Ported from cbcore-rs `test_parse_scope_not_found`.
    #[test]
    fn a_scope_not_found_message_names_its_bucket_and_scope() {
        let err = classify(
            r#"{"code":12021,"msg":"Scope not found in CB datastore default:default.test (near line 1, column 15)"}"#,
        );

        assert_eq!(ServerErrorKind::ScopeNotFound, server_kind(&err));
        let r = resource(&err);
        assert_eq!(Some("default"), r.bucket_name());
        assert_eq!(Some("test"), r.scope_name());
        assert_eq!(None, r.collection_name());
    }

    /// Ported from cbcore-rs `test_parse_collection_not_found`.
    ///
    /// The path splits right to left, because a bucket name is the only one of
    /// the three that may itself contain a dot.
    #[test]
    fn a_collection_not_found_message_names_all_three() {
        let err = classify(
            r#"{"code":12003,"msg":"Keyspace not found in CB datastore: default:default._default.test - cause: No such keyspace"}"#,
        );

        assert_eq!(ServerErrorKind::CollectionNotFound, server_kind(&err));
        let r = resource(&err);
        assert_eq!(Some("default"), r.bucket_name());
        assert_eq!(Some("_default"), r.scope_name());
        assert_eq!(Some("test"), r.collection_name());
    }

    /// Ported from cbcore-rs `test_parse_index_exists_msg`.
    #[test]
    fn an_index_exists_message_names_the_index() {
        let err = classify(r#"{"code":4300,"msg":"The index NewIndex already exists."}"#);

        assert_eq!(ServerErrorKind::IndexExists, server_kind(&err));
        assert_eq!(Some("NewIndex"), resource(&err).index_name());
    }

    /// Ported from cbcore-rs `test_parse_index_not_found_msg`.
    ///
    /// The anchor is the lower-case word `index`, so the capitalised
    /// `Index Not Found` at the front of the message is stepped over rather
    /// than yielding `Not` as the index name.
    #[test]
    fn an_index_not_found_message_names_the_index() {
        let err = classify(
            r#"{"code":12004,"msg":"Index Not Found - cause: GSI index testingIndex not found."}"#,
        );

        assert_eq!(ServerErrorKind::IndexNotFound, server_kind(&err));
        assert_eq!(Some("testingIndex"), resource(&err).index_name());
    }

    /// A message whose last word is `index` must not take the parser with it.
    ///
    /// cbcore-rs guards this with a bounds check
    /// (src/services/query.rs::parse_index_not_found_or_exists_msg); the
    /// equivalent here reached past the end of the iterator.
    #[test]
    fn a_message_ending_in_the_word_index_does_not_panic() {
        let err = classify(r#"{"code":4300,"msg":"could not create index"}"#);

        assert_eq!(ServerErrorKind::IndexExists, server_kind(&err));
        assert_eq!(None, resource(&err).index_name());
    }

    /// Ported from cbcore-rs `test_parse_dml_cas_mismatch`: a DML failure with
    /// no structured reason falls back to reading its message.
    #[test]
    fn a_dml_failure_with_no_reason_falls_back_to_its_message() {
        let err = classify(r#"{"code":12009,"msg":"DML error - cause: CAS mismatch"}"#);
        assert_eq!(ServerErrorKind::CasMismatch, server_kind(&err));
    }

    /// Ported from cbcore-rs `test_parse_dml_document_not_found`: when a
    /// structured reason is present it decides, and the message is not read.
    #[test]
    fn a_dml_failure_reads_its_structured_reason_code() {
        let err = classify(
            r#"{"code":12009,"msg":"DML error","reason":{"code":17014,"message":"Key not found"}}"#,
        );
        assert_eq!(ServerErrorKind::DocNotFound, server_kind(&err));

        let err = classify(
            r#"{"code":12009,"msg":"DML error - cause: CAS mismatch","reason":{"code":17012}}"#,
        );
        assert_eq!(
            ServerErrorKind::DocExists,
            server_kind(&err),
            "the structured reason must win over the message text"
        );
    }

    /// Ported from cbcore-rs `an_index_that_already_exists_still_classifies`.
    ///
    /// A 5000 is a free-text bucket, so this arm is decided by matching the
    /// message. That is the arm most easily broken by an over-greedy pattern
    /// added beside it.
    #[test]
    fn an_index_that_already_exists_still_classifies() {
        let err = classify(
            r#"{"code":5000,"msg":"GSI CreateIndex() - cause: Index ix already exists."}"#,
        );
        assert_eq!(ServerErrorKind::IndexExists, server_kind(&err));

        let err =
            classify(r#"{"code":5000,"msg":"GSI DropIndex() - cause: Index ix does not exist."}"#);
        assert_eq!(ServerErrorKind::IndexNotFound, server_kind(&err));

        let err = classify(
            r#"{"code":5000,"msg":"Build Already In Progress. Multiple concurrent index builds are not supported."}"#,
        );
        assert_eq!(ServerErrorKind::BuildAlreadyInProgress, server_kind(&err));
    }

    /// Ported from cbcore-rs
    /// `a_wrapper_around_a_fatal_reason_is_not_made_transient_by_its_envelope`.
    ///
    /// The envelope of a 5000 mentions an index by name whatever went wrong
    /// underneath. Only the carried reason may decide the classification, so a
    /// wrapper carrying an unrecognised reason must stay `Internal` rather than
    /// being read as an index that exists or is missing.
    #[test]
    fn a_wrapper_around_a_fatal_reason_is_not_made_transient_by_its_envelope() {
        let err = classify(
            r#"{"code":5000,"msg":"Index creation for index ix, bucket b, scope s, collection c cannot start. Reason: some other failure"}"#,
        );
        assert_eq!(ServerErrorKind::Internal, server_kind(&err));
    }

    async fn respreader(body: &'static str) -> error::Result<QueryRespReader> {
        let http_resp = http::Response::builder()
            .status(200)
            .body(body.to_string())
            .unwrap();
        let resp = Response::from(reqwest::Response::from(http_resp));

        QueryRespReader::new(resp, "localhost:8093", "SELECT 1", "ctx").await
    }

    /// Read a whole response into the rows it yielded and the error, if any,
    /// that ended it.
    ///
    /// **Both ends are collected because the failure can arrive at either.**
    /// A response with no rows has its epilog read during construction, so its
    /// error comes back from `new`; a response with rows hands those over first
    /// and fails from the stream. Either way it is an error, which is the point.
    async fn drain(body: &'static str) -> (Vec<String>, Option<Error>) {
        let mut reader = match respreader(body).await {
            Ok(r) => r,
            Err(e) => return (vec![], Some(e)),
        };

        let mut rows = vec![];
        let mut error = None;

        while let Some(item) = reader.next().await {
            match item {
                Ok(row) => rows.push(String::from_utf8_lossy(&row).to_string()),
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }

        (rows, error)
    }

    const FATAL_WITH_NO_ROWS: &str = r#"{"requestID":"a","signature":{"*":"*"},"results":[
    ],
    "errors":[{"code":5010,"msg":"Error evaluating ExpressionScan"}],
    "status":"fatal","metrics":{"elapsedTime":"1.1ms","executionTime":"1ms","resultCount":0,"resultSize":0,"errorCount":1}}"#;

    const FATAL_AFTER_ROWS: &str = r#"{"requestID":"a","signature":{"*":"*"},"results":[
    {"a":1},
    {"a":2}
    ],
    "errors":[{"code":5010,"msg":"Error evaluating ExpressionScan"}],
    "status":"fatal","metrics":{"elapsedTime":"1.1ms","executionTime":"1ms","resultCount":2,"resultSize":16,"errorCount":1}}"#;

    const SUCCESS_WITH_ROWS: &str = r#"{"requestID":"a","signature":{"*":"*"},"results":[
    {"a":1},
    {"a":2}
    ],
    "status":"success","metrics":{"elapsedTime":"1.1ms","executionTime":"1ms","resultCount":2,"resultSize":16,"errorCount":0}}"#;

    /// Ported from cbcore-rs
    /// `a_failure_that_arrives_with_a_200_is_an_error_and_not_an_empty_answer`.
    ///
    /// The query service answers 200 and then fails in the body. An empty row
    /// set and a failure look identical to a caller that only counts rows.
    #[tokio::test]
    async fn a_failure_that_arrives_with_a_200_is_an_error_and_not_an_empty_answer() {
        let (rows, error) = drain(FATAL_WITH_NO_ROWS).await;

        assert!(rows.is_empty(), "expected no rows, got {rows:?}");
        let err = error.expect("a fatal response read as an empty answer");
        assert_eq!(ServerErrorKind::Internal, server_kind(&err));
        assert!(
            err.to_string().contains("ExpressionScan"),
            "the server's message was lost: {err}"
        );
    }

    /// Ported from cbcore-rs `rows_already_sent_survive_the_error_that_follows_them`.
    ///
    /// Rows are handed over as they arrive, so a failure in the epilog must be
    /// raised *after* them rather than in place of them.
    #[tokio::test]
    async fn rows_already_sent_survive_the_error_that_follows_them() {
        let (rows, error) = drain(FATAL_AFTER_ROWS).await;

        assert_eq!(vec![r#"{"a":1}"#, r#"{"a":2}"#], rows);
        assert!(
            error.is_some(),
            "the error following the rows never reached the caller"
        );
    }

    /// The control for the two above: an ordinary response yields its rows and
    /// no error. Without it, a reader that always errored would pass them both.
    #[tokio::test]
    async fn rows_still_arrive_when_the_response_carries_no_error() {
        let mut reader = respreader(SUCCESS_WITH_ROWS)
            .await
            .expect("the reader refused a clean 200");
        let mut rows = vec![];
        while let Some(item) = reader.next().await {
            rows.push(String::from_utf8_lossy(&item.expect("unexpected error")).to_string());
        }

        assert_eq!(vec![r#"{"a":1}"#, r#"{"a":2}"#], rows);
        let meta = reader.metadata().expect("no metadata");
        assert_eq!(Status::Success, meta.status);
    }

    /// Ported from cbcore-rs `a_bracket_inside_an_error_message_does_not_end_the_array`.
    ///
    /// cbcore-rs hand-balances the brackets of the `errors` array and has to
    /// track quoting to do it. Here the epilog boundary comes from the real
    /// tokenizer, so this is correct by construction — but the regression it
    /// guards against, a truncated epilog silently dropping the error, is worth
    /// a case anyway.
    #[tokio::test]
    async fn a_bracket_inside_an_error_message_does_not_end_the_array() {
        const BRACKETED: &str = r#"{"requestID":"a","results":[
    ],
    "errors":[{"code":13014,"msg":"User does not have credentials to run queries [see docs] on b] — check permissions"}],
    "status":"fatal","metrics":{"elapsedTime":"1ms","executionTime":"1ms","errorCount":1}}"#;

        let (rows, error) = drain(BRACKETED).await;

        assert!(rows.is_empty());
        let err = error.expect("the error was lost with the bracket");
        assert!(
            err.to_string().contains("see docs"),
            "the message was truncated at the bracket: {err}"
        );
    }
}
