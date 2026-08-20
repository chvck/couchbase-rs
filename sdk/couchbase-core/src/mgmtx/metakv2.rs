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

//! metakv2 — ns_server's internal cluster-wide coordination store.
//!
//! A compare-and-swap key/value store backed by chronicle, exposed at
//! `/_metakv2`. It is what a component uses when it needs several nodes to
//! agree on metadata it authored, and it gives three things a bucket document
//! cannot: atomic multi-key commits, cross-key consistent reads, and survival
//! of the bucket being dropped.
//!
//! **It is an internal API with no published contract.** The permission is
//! literally `cluster.admin.internal.metakv2`, and the behaviour has been
//! observed to change between server versions — `depth` is rejected outright by
//! shipping builds, and `create=true` over an existing key answers a plain 409
//! rather than a distinguishable "exists". Everything this module encodes was
//! measured against Couchbase 8.0.0/8.0.3. Treat a server upgrade as a reason to
//! re-measure.
//!
//! The rules that surprise people, all of them measured:
//!
//! - The revision is `<history-uuid>:<seqno>` and the seqno is a **single
//!   cluster-global counter**, not a per-key one. Two revisions are comparable
//!   for equality and nothing else.
//! - A commit stamps only the keys it **actually changes**. A key written to
//!   the value it already holds keeps its old revision.
//! - A commit in which nothing differs returns "Not Changed" **with no
//!   revision at all**, which is why the mutation response carries an
//!   `Option<MetaKv2Revision>`.
//! - A revision precondition is checked **before** values are compared, so a
//!   commit that would write nothing still conflicts on a stale revision.
//! - A create collision and a stale precondition are **the same 409**, naming
//!   the same path and carrying the key's current revision. The caller tells
//!   them apart by knowing whether its own entry carried a revision.
//! - `DELETE` accepts a revision and **ignores it**. There is no conditional
//!   delete.
//! - Path shape is strict: a directory needs its trailing slash, a leaf must
//!   not have one, and the wrong shape is a 404.
//! - A read from a node that did not take the write can be one commit stale,
//!   and a node cut off from quorum serves stale reads with a 200 indefinitely.
//!   Success here is not evidence of freshness.
//!
//! Ported from cbcore-rs `src/services/metakv2.rs`, with the wire handling
//! reconciled against the Go sibling SDK (`gocbcorex/cbmgmtx/metakv2.go`).

use std::collections::{BTreeMap, HashMap};

use bytes::Bytes;
use http::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::httpx::client::Client;
use crate::mgmtx::error;
use crate::mgmtx::mgmt::{parse_response_json, Management};
use crate::mgmtx::options::{
    DeleteMetaKv2DirOptions, GetMetaKv2DirOptions, GetMetaKv2Options, SetMetaKv2MultipleOptions,
    SetMetaKv2Options, SyncMetaKv2QuorumOptions,
};
use crate::mgmtx::responses::{GetMetaKv2DirResponse, MetaKv2MutationResponse};
use crate::tracingcomponent::{BeginDispatchFields, EndDispatchFields};
use crate::util::get_host_port_tuple_from_uri;

/// The prefix every metakv2 request sits behind.
const METAKV2_PREFIX: &str = "_metakv2";

/// A revision as the store issues it: `<history-uuid>:<seqno>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MetaKv2Revision(pub String);

impl MetaKv2Revision {
    pub fn new(revision: impl Into<String>) -> Self {
        Self(revision.into())
    }

    /// The history uuid half. A change of it means the store may have been
    /// rebuilt rather than merely relabelled, so seqnos across it are not
    /// comparable.
    pub fn history(&self) -> &str {
        self.0.split_once(':').map_or(self.0.as_str(), |(h, _)| h)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MetaKv2Revision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One leaf, as a read or a directory listing returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MetaKv2Entry {
    pub value: String,
    pub revision: MetaKv2Revision,
}

impl MetaKv2Entry {
    pub fn new(value: impl Into<String>, revision: MetaKv2Revision) -> Self {
        Self {
            value: value.into(),
            revision,
        }
    }
}

/// One key's write within a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetaKv2Write {
    /// Write `value`, conditional on the key standing at `revision`. `None` is
    /// unconditional — and the field is omitted rather than sent empty, because
    /// an empty revision string is a 400.
    Set {
        value: String,
        revision: Option<MetaKv2Revision>,
    },
    /// Write `value` only if the key is absent.
    Create { value: String },
}

impl MetaKv2Write {
    pub fn set(value: impl Into<String>, revision: impl Into<Option<MetaKv2Revision>>) -> Self {
        Self::Set {
            value: value.into(),
            revision: revision.into(),
        }
    }

    pub fn create(value: impl Into<String>) -> Self {
        Self::Create {
            value: value.into(),
        }
    }
}

/// One entry of a `setMultiple` body. `revision` and `create` are omitted when
/// unset — an empty revision string is a 400, not an absent precondition.
#[derive(Debug, Serialize)]
pub(crate) struct MetaKv2SetEntryJson<'a> {
    pub value: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<&'a str>,
    #[serde(skip_serializing_if = "is_false")]
    pub create: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// A leaf read: `{"value": "...", "revision": "..."}`.
#[derive(Debug, Deserialize)]
struct MetaKv2LeafJson {
    revision: String,
    value: String,
}

/// One node of a directory listing, and the listing's own envelope, which have
/// the same shape. A leaf carries a string `value`; a directory carries an
/// object of its children, or nothing at all when it is empty. The outer `value`
/// is keyed by the requested directory's own absolute path, and the entries
/// beneath it are keyed absolutely too.
#[derive(Debug, Deserialize)]
struct MetaKv2NodeJson {
    revision: String,
    #[serde(default)]
    value: Value,
}

/// Every mutation answers with the same envelope; `revision` is absent when the
/// commit changed nothing.
#[derive(Debug, Deserialize)]
struct MetaKv2MutationJson {
    #[serde(default)]
    revision: Option<String>,
}

/// The error envelope: `{"message": "...", "path": "...", "revision": "..."}`.
#[derive(Debug, Default, Deserialize)]
struct MetaKv2ErrorJson {
    #[serde(default)]
    message: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    revision: Option<String>,
}

/// Give a key its leading slash. Both this client and the Go sibling accept a
/// key written either way and normalise before it reaches the wire.
pub(crate) fn normalize_metakv2_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

/// Walk a directory listing, collecting the leaves into `out` keyed by their
/// absolute paths.
///
/// A recursive listing nests each subdirectory's children inside that
/// subdirectory's own node, so anything that stops at the first level silently
/// drops every key below it. Directory nodes are not collected — only the
/// leaves are, which is the surface a caller of `get_metakv2_dir` wants, and it
/// means an unrelated subtree sharing the namespace cannot break the read.
pub(crate) fn collect_metakv2_leaves(
    nodes: Value,
    out: &mut BTreeMap<String, MetaKv2Entry>,
) -> error::Result<()> {
    if nodes.is_null() {
        return Ok(());
    }

    let nodes: HashMap<String, MetaKv2NodeJson> = serde_json::from_value(nodes)
        .map_err(|e| error::Error::new_message_error(format!("could not parse response: {e}")))?;

    for (path, node) in nodes {
        if path.ends_with('/') {
            collect_metakv2_leaves(node.value, out)?;
        } else {
            let Value::String(value) = node.value else {
                return Err(error::Error::new_message_error(format!(
                    "metakv2 leaf {path} did not carry a string value"
                )));
            };

            out.insert(
                path,
                MetaKv2Entry::new(value, MetaKv2Revision(node.revision)),
            );
        }
    }

    Ok(())
}

impl<C: Client> Management<C> {
    /// Read one leaf.
    ///
    /// `path` must **not** carry a trailing slash — a leaf read with one is a
    /// 404, the mirror of the rule for directories — so the wrong shape is
    /// rejected here rather than sent.
    pub async fn get_metakv2(&self, opts: &GetMetaKv2Options<'_>) -> error::Result<MetaKv2Entry> {
        let key = Self::metakv2_leaf_key(opts.path)?;

        let method = Method::GET;
        let path = format!("{METAKV2_PREFIX}{key}");

        let resp = self
            .execute_metakv2(method.clone(), &path, "", opts.on_behalf_of_info, None)
            .await?;

        if resp.status() != 200 {
            return Err(Self::decode_metakv2_error(method, path, resp).await);
        }

        let leaf: MetaKv2LeafJson = parse_response_json(resp).await?;

        Ok(MetaKv2Entry::new(
            leaf.value,
            MetaKv2Revision(leaf.revision),
        ))
    }

    /// Read every leaf under `dir`, keyed by absolute path, each with its own
    /// revision.
    ///
    /// One read is a consistent snapshot across keys: the store advances as a
    /// single global log and a read reflects one position in it, so a caller
    /// cannot see one key of an atomic commit without the other. It is a
    /// snapshot, not necessarily a fresh one — see the module docs.
    ///
    /// `dir` must carry its trailing slash. A directory that has never been
    /// published answers 404, which is distinguishable from an empty listing
    /// because a directory survives its last child being deleted.
    pub async fn get_metakv2_dir(
        &self,
        opts: &GetMetaKv2DirOptions<'_>,
    ) -> error::Result<GetMetaKv2DirResponse> {
        let key = Self::metakv2_dir_key(opts.path)?;

        let method = Method::GET;
        let path = format!("{METAKV2_PREFIX}{key}?recursive=true");

        let resp = self
            .execute_metakv2(method.clone(), &path, "", opts.on_behalf_of_info, None)
            .await?;

        if resp.status() != 200 {
            return Err(Self::decode_metakv2_error(method, path, resp).await);
        }

        let dir: MetaKv2NodeJson = parse_response_json(resp).await?;

        let mut entries = BTreeMap::new();
        collect_metakv2_leaves(dir.value, &mut entries)?;

        Ok(GetMetaKv2DirResponse {
            revision: MetaKv2Revision(dir.revision),
            entries,
        })
    }

    /// Write one leaf, conditional on `revision` when it is given.
    ///
    /// This is `set_metakv2_multiple` with one entry, and deliberately so: the
    /// single-key `PUT` the server also offers reports "Not Changed" *with* a
    /// revision and answers 201 on create, so routing this through the commit
    /// endpoint keeps one contract for every write.
    pub async fn set_metakv2(
        &self,
        opts: &SetMetaKv2Options<'_>,
    ) -> error::Result<MetaKv2MutationResponse> {
        let mut writes = BTreeMap::new();
        writes.insert(
            opts.path.to_string(),
            MetaKv2Write::Set {
                value: opts.value.to_string(),
                revision: opts.revision.cloned(),
            },
        );

        self.set_metakv2_multiple(&SetMetaKv2MultipleOptions {
            on_behalf_of_info: opts.on_behalf_of_info,
            writes: &writes,
        })
        .await
    }

    /// Commit a set of writes atomically.
    ///
    /// All-or-nothing under a conflict: one stale revision means nothing is
    /// applied, and the body names exactly one conflicting path even when
    /// several entries conflict. Missing parent directories are created, which
    /// every write on a cold start needs.
    ///
    /// An empty set of writes is a commit in which nothing differs, so it
    /// answers the same way the server does for one — `revision: None` — without
    /// a round trip.
    pub async fn set_metakv2_multiple(
        &self,
        opts: &SetMetaKv2MultipleOptions<'_>,
    ) -> error::Result<MetaKv2MutationResponse> {
        if opts.writes.is_empty() {
            return Ok(MetaKv2MutationResponse { revision: None });
        }

        let body = Bytes::from(Self::build_set_multiple_body(opts.writes)?);

        let method = Method::POST;
        let path = format!("{METAKV2_PREFIX}/_controller/setMultiple?recursive=true");

        let resp = self
            .execute_metakv2(
                method.clone(),
                &path,
                "application/json",
                opts.on_behalf_of_info,
                Some(body),
            )
            .await?;

        if resp.status() != 200 && resp.status() != 201 {
            return Err(Self::decode_metakv2_error(method, path, resp).await);
        }

        Self::parse_mutation_response(resp).await
    }

    /// Remove a subtree and everything under it.
    ///
    /// There is no compare-and-swap: a revision parameter is accepted and
    /// ignored. And a 404 is **not proof of absence** — a delete routed to a
    /// node that did not take the write 404s about as often as a read from that
    /// node goes stale, so a caller needing certainty confirms with a second
    /// read.
    pub async fn delete_metakv2_dir(
        &self,
        opts: &DeleteMetaKv2DirOptions<'_>,
    ) -> error::Result<MetaKv2MutationResponse> {
        let key = Self::metakv2_dir_key(opts.path)?;

        let method = Method::DELETE;
        let path = format!("{METAKV2_PREFIX}{key}?recursive=true");

        let resp = self
            .execute_metakv2(method.clone(), &path, "", opts.on_behalf_of_info, None)
            .await?;

        if resp.status() != 200 {
            return Err(Self::decode_metakv2_error(method, path, resp).await);
        }

        Self::parse_mutation_response(resp).await
    }

    /// Ask this node to confirm it still has quorum.
    ///
    /// The only measured way to tell a current node from a stale one: reads
    /// cannot do it, because a minority-partitioned node answers them with a
    /// 200 and old data indefinitely. On a healthy node this costs ~8–20 ms; on
    /// a node without quorum it fails after ~15 s.
    ///
    /// The `timeout` parameter the endpoint accepts is **ignored** — absent,
    /// 1000, 5000 and 30000 all return in the same 12–15 s — so it is not
    /// offered here and any deadline has to be imposed by the caller.
    pub async fn sync_metakv2_quorum(
        &self,
        opts: &SyncMetaKv2QuorumOptions<'_>,
    ) -> error::Result<()> {
        let method = Method::POST;
        let path = format!("{METAKV2_PREFIX}/_controller/syncQuorum");

        let resp = self
            .execute_metakv2(method.clone(), &path, "", opts.on_behalf_of_info, None)
            .await?;

        if resp.status() != 200 {
            return Err(Self::decode_metakv2_error(method, path, resp).await);
        }

        Ok(())
    }

    async fn execute_metakv2(
        &self,
        method: Method,
        path: &str,
        content_type: &str,
        on_behalf_of_info: Option<&crate::httpx::request::OnBehalfOfInfo>,
        body: Option<Bytes>,
    ) -> error::Result<crate::httpx::response::Response> {
        let peer_addr = get_host_port_tuple_from_uri(&self.endpoint).unwrap_or_default();
        let canonical_addr =
            get_host_port_tuple_from_uri(&self.canonical_endpoint).unwrap_or_default();

        self.tracing
            .orchestrate_dispatch_span(
                BeginDispatchFields::new(
                    (&peer_addr.0, &peer_addr.1),
                    (&canonical_addr.0, &canonical_addr.1),
                    None,
                ),
                self.execute(
                    method,
                    path,
                    content_type,
                    on_behalf_of_info.cloned(),
                    None,
                    body,
                ),
                |_| EndDispatchFields::new(None, None),
            )
            .await
    }

    async fn parse_mutation_response(
        resp: crate::httpx::response::Response,
    ) -> error::Result<MetaKv2MutationResponse> {
        let mutation: MetaKv2MutationJson = parse_response_json(resp).await?;

        Ok(MetaKv2MutationResponse {
            revision: mutation.revision.map(MetaKv2Revision),
        })
    }

    pub(crate) fn build_set_multiple_body(
        writes: &BTreeMap<String, MetaKv2Write>,
    ) -> error::Result<String> {
        let mut entries = BTreeMap::new();
        for (path, write) in writes {
            let key = Self::metakv2_leaf_key(path)?;
            let entry = match write {
                MetaKv2Write::Set { value, revision } => MetaKv2SetEntryJson {
                    value,
                    revision: revision.as_ref().map(MetaKv2Revision::as_str),
                    create: false,
                },
                MetaKv2Write::Create { value } => MetaKv2SetEntryJson {
                    value,
                    revision: None,
                    create: true,
                },
            };

            entries.insert(key, entry);
        }

        serde_json::to_string(&entries).map_err(|e| {
            error::Error::new_message_error(format!("could not encode metakv2 writes: {e}"))
        })
    }

    fn metakv2_leaf_key(path: &str) -> error::Result<String> {
        if path.is_empty() || path == "/" {
            return Err(error::Error::new_invalid_argument_error(
                "must specify a path when addressing a metakv2 key",
                "path".to_string(),
            ));
        }

        if path.ends_with('/') {
            return Err(error::Error::new_invalid_argument_error(
                "a metakv2 leaf path must not carry a trailing slash",
                "path".to_string(),
            ));
        }

        Ok(normalize_metakv2_path(path))
    }

    fn metakv2_dir_key(path: &str) -> error::Result<String> {
        if !path.ends_with('/') {
            return Err(error::Error::new_invalid_argument_error(
                "a metakv2 directory path must carry a trailing slash",
                "path".to_string(),
            ));
        }

        let key = normalize_metakv2_path(path);
        if key == "/" {
            return Err(error::Error::new_invalid_argument_error(
                "must specify a path when addressing a metakv2 directory",
                "path".to_string(),
            ));
        }

        Ok(key)
    }

    pub(crate) async fn decode_metakv2_error(
        method: Method,
        path: String,
        response: crate::httpx::response::Response,
    ) -> error::Error {
        let status = response.status();
        let url = response.url().to_string();

        let body = match response.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return error::Error::new_message_error(format!(
                    "could not parse response body: {e}"
                ))
            }
        };

        let body_str = match String::from_utf8(body.to_vec()) {
            Ok(s) => s,
            Err(e) => {
                return error::Error::new_message_error(format!(
                    "could not parse error response: {e}"
                ))
            }
        };

        let kind = classify_metakv2_error(status.as_u16(), &body_str, &path);

        error::ServerError::new(status, url, method, path, body_str, kind).into()
    }
}

/// Classify a metakv2 error body.
///
/// Follows the Go sibling's classifier, with two additions the shipping server
/// forces:
///
/// - Contention is checked **first**. Chronicle answers 503 "Exceeded retries
///   due to conflicting updates", whose message contains "conflict", so a
///   naive conflict test swallows it. It has to stay distinct: the seqno is one
///   cluster-global counter, so every commit in the cluster serializes through
///   the same log and writes to entirely unrelated keys contend. Nothing was
///   applied and the same commit is still valid, so the recovery is to retry
///   with backoff — reading it as a conflict sends the caller off to rebase
///   against a version that never moved.
/// - A 409 keeps the path and current revision from the body. A create
///   collision and a stale precondition are the same 409, so the caller
///   recovers the difference from its own request; `current_revision` is absent
///   when the named key does not exist.
fn classify_metakv2_error(status: u16, body: &str, request_path: &str) -> error::ServerErrorKind {
    let err_resp: MetaKv2ErrorJson = serde_json::from_str(body).unwrap_or_default();
    let message = err_resp.message.to_lowercase();

    if status == 503 && message.contains("conflicting updates") {
        error::ServerErrorKind::MetaKvContended
    } else if message.contains("not found") || status == 404 {
        error::ServerErrorKind::MetaKvEntryNotFound
    } else if message.contains("conflict") || status == 409 {
        error::ServerErrorKind::MetaKvConflict {
            path: err_resp.path,
            current_revision: err_resp.revision,
        }
    } else if message.contains("not empty") {
        error::ServerErrorKind::MetaKvNotEmpty
    } else if message.contains("timeout") || status == 504 {
        error::ServerErrorKind::MetaKvTimeout
    } else if message.contains("wrong type") {
        error::ServerErrorKind::MetaKvWrongType
    } else if message.contains("exists") {
        error::ServerErrorKind::MetaKvExists
    } else if status == 401 || status == 403 {
        // The permission is `cluster.admin.internal.metakv2`, which is Full
        // Admin only: `cluster_admin`, `ro_admin` and `security_admin_local` all
        // land here, so it is a deployment answer rather than a transient one.
        error::ServerErrorKind::AccessDenied
    } else if status == 400 {
        error::ServerErrorKind::ServerInvalidArg {
            arg: request_path.to_string(),
            reason: body.to_string(),
        }
    } else {
        error::ServerErrorKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::httpx::client::ReqwestClient;

    type Mgmt = Management<ReqwestClient>;

    /// Captured verbatim from Couchbase 8.0.3-5864-enterprise for
    /// `GET /_metakv2/rscbcoreprobe/?recursive=true`, over a subtree holding
    /// one leaf and one subdirectory with a leaf of its own.
    const DIR_LISTING: &str = r#"{
      "revision": "c5e1beb4aeaa4a54b47332e880a4cc70:82268",
      "value": {
        "/rscbcoreprobe/": {
          "revision": "c5e1beb4aeaa4a54b47332e880a4cc70:82268",
          "value": {
            "/rscbcoreprobe/a": {
              "revision": "c5e1beb4aeaa4a54b47332e880a4cc70:82265",
              "value": "hello"
            },
            "/rscbcoreprobe/sub/": {
              "revision": "c5e1beb4aeaa4a54b47332e880a4cc70:82268",
              "value": {
                "/rscbcoreprobe/sub/b": {
                  "revision": "c5e1beb4aeaa4a54b47332e880a4cc70:82268",
                  "value": "world"
                }
              }
            }
          }
        }
      }
    }"#;

    fn parse_dir(body: &str) -> BTreeMap<String, MetaKv2Entry> {
        let dir: Value = serde_json::from_str(body).unwrap();
        let mut entries = BTreeMap::new();
        collect_metakv2_leaves(
            dir.get("value").cloned().unwrap_or(Value::Null),
            &mut entries,
        )
        .unwrap();
        entries
    }

    #[test]
    fn dir_listing_reaches_nested_leaves() {
        let entries = parse_dir(DIR_LISTING);

        assert_eq!(
            entries.keys().collect::<Vec<_>>(),
            vec!["/rscbcoreprobe/a", "/rscbcoreprobe/sub/b"],
        );

        assert_eq!(
            entries["/rscbcoreprobe/a"],
            MetaKv2Entry::new(
                "hello",
                MetaKv2Revision::new("c5e1beb4aeaa4a54b47332e880a4cc70:82265")
            )
        );

        // The nested leaf only exists inside the `sub/` node, so anything that
        // stops at the first level of the listing drops it silently.
        assert_eq!(
            entries["/rscbcoreprobe/sub/b"],
            MetaKv2Entry::new(
                "world",
                MetaKv2Revision::new("c5e1beb4aeaa4a54b47332e880a4cc70:82268")
            )
        );
    }

    #[test]
    fn dir_listing_of_an_empty_directory_is_empty_not_an_error() {
        // A directory survives its last child being deleted, and answers 200
        // with a node carrying no value at all.
        let body = r#"{"revision":"h:1","value":{"/d/":{"revision":"h:1"}}}"#;

        assert!(parse_dir(body).is_empty());
    }

    #[test]
    fn dir_listing_of_a_non_string_leaf_is_an_error() {
        let body = r#"{"revision":"h:1","value":{"/d/":{"revision":"h:1","value":{"/d/a":{"revision":"h:1","value":7}}}}}"#;
        let dir: Value = serde_json::from_str(body).unwrap();

        let mut entries = BTreeMap::new();
        assert!(collect_metakv2_leaves(dir["value"].clone(), &mut entries).is_err());
    }

    #[test]
    fn revision_history_is_the_half_before_the_seqno() {
        assert_eq!(MetaKv2Revision::new("abc:82268").history(), "abc");
        // A revision the client did not issue itself still has to be usable.
        assert_eq!(MetaKv2Revision::new("abc").history(), "abc");
    }

    #[test]
    fn set_multiple_omits_an_absent_revision() {
        let mut writes = BTreeMap::new();
        writes.insert("/d/a".to_string(), MetaKv2Write::set("v", None));

        // An empty revision string is a 400, so the field has to be absent
        // rather than present-and-empty.
        assert_eq!(
            Mgmt::build_set_multiple_body(&writes).unwrap(),
            r#"{"/d/a":{"value":"v"}}"#
        );
    }

    #[test]
    fn set_multiple_sends_a_revision_precondition() {
        let mut writes = BTreeMap::new();
        writes.insert(
            "/d/a".to_string(),
            MetaKv2Write::set("v", MetaKv2Revision::new("h:1")),
        );

        assert_eq!(
            Mgmt::build_set_multiple_body(&writes).unwrap(),
            r#"{"/d/a":{"value":"v","revision":"h:1"}}"#
        );
    }

    #[test]
    fn set_multiple_sends_create_without_a_revision() {
        let mut writes = BTreeMap::new();
        writes.insert("/d/a".to_string(), MetaKv2Write::create("v"));

        assert_eq!(
            Mgmt::build_set_multiple_body(&writes).unwrap(),
            r#"{"/d/a":{"value":"v","create":true}}"#
        );
    }

    #[test]
    fn set_multiple_normalizes_a_leading_slash() {
        let mut writes = BTreeMap::new();
        writes.insert("d/a".to_string(), MetaKv2Write::set("v", None));

        assert_eq!(
            Mgmt::build_set_multiple_body(&writes).unwrap(),
            r#"{"/d/a":{"value":"v"}}"#
        );
    }

    #[test]
    fn set_multiple_rejects_a_directory_path() {
        let mut writes = BTreeMap::new();
        writes.insert("/d/".to_string(), MetaKv2Write::set("v", None));

        let err = Mgmt::build_set_multiple_body(&writes).unwrap_err();
        assert!(
            matches!(err.kind(), error::ErrorKind::InvalidArgument { .. }),
            "expected an invalid argument error, got {err}"
        );
    }

    #[test]
    fn contention_is_not_a_conflict() {
        // Chronicle's 503 message contains "conflict", so it has to be tested
        // for before the conflict arm or it is swallowed by it.
        let body = r#"{"message":"Exceeded retries due to conflicting updates"}"#;

        assert_eq!(
            classify_metakv2_error(503, body, "_metakv2/_controller/setMultiple"),
            error::ServerErrorKind::MetaKvContended
        );
    }

    #[test]
    fn a_conflict_keeps_the_path_and_the_current_revision() {
        let body = r#"{"message":"Conflict","revision":"c5e1beb4aeaa4a54b47332e880a4cc70:82270","path":"/rscbcoreprobe/a"}"#;

        assert_eq!(
            classify_metakv2_error(409, body, "_metakv2/_controller/setMultiple"),
            error::ServerErrorKind::MetaKvConflict {
                path: "/rscbcoreprobe/a".to_string(),
                current_revision: Some("c5e1beb4aeaa4a54b47332e880a4cc70:82270".to_string()),
            }
        );
    }

    #[test]
    fn a_conflict_on_an_absent_key_has_no_current_revision() {
        let body = r#"{"message":"Conflict","path":"/rscbcoreprobe/a"}"#;

        assert_eq!(
            classify_metakv2_error(409, body, "_metakv2/rscbcoreprobe/a"),
            error::ServerErrorKind::MetaKvConflict {
                path: "/rscbcoreprobe/a".to_string(),
                current_revision: None,
            }
        );
    }

    #[test]
    fn not_found_is_an_entry_not_found() {
        let body = r#"{"message":"Not Found","path":"/rscbcoreprobe/nope"}"#;

        assert_eq!(
            classify_metakv2_error(404, body, "_metakv2/rscbcoreprobe/nope"),
            error::ServerErrorKind::MetaKvEntryNotFound
        );
    }

    #[test]
    fn a_forbidden_answer_is_access_denied() {
        // The permission is Full Admin only, so anything short of it lands here
        // rather than on a metakv-specific kind.
        assert_eq!(
            classify_metakv2_error(403, "Forbidden", "_metakv2/d/a"),
            error::ServerErrorKind::AccessDenied
        );
        assert_eq!(
            classify_metakv2_error(401, "", "_metakv2/d/a"),
            error::ServerErrorKind::AccessDenied
        );
    }

    #[test]
    fn a_bad_request_carries_the_body_as_its_reason() {
        assert_eq!(
            classify_metakv2_error(400, "bad revision", "_metakv2/d/a"),
            error::ServerErrorKind::ServerInvalidArg {
                arg: "_metakv2/d/a".to_string(),
                reason: "bad revision".to_string(),
            }
        );
    }

    #[test]
    fn a_non_json_body_still_classifies_by_status() {
        // A non-JSON body is ns_server's generic error page, which means the URL
        // did not route at all.
        assert_eq!(
            classify_metakv2_error(500, "<html>oh no</html>", "_metakv2/d/a"),
            error::ServerErrorKind::Unknown
        );
    }

    #[test]
    fn the_remaining_go_reference_mappings_hold() {
        assert_eq!(
            classify_metakv2_error(500, r#"{"message":"Directory not empty"}"#, "_metakv2/d/"),
            error::ServerErrorKind::MetaKvNotEmpty
        );
        assert_eq!(
            classify_metakv2_error(504, "", "_metakv2/d/a"),
            error::ServerErrorKind::MetaKvTimeout
        );
        assert_eq!(
            classify_metakv2_error(500, r#"{"message":"Wrong type"}"#, "_metakv2/d/a"),
            error::ServerErrorKind::MetaKvWrongType
        );
        assert_eq!(
            classify_metakv2_error(500, r#"{"message":"Exists"}"#, "_metakv2/d/a"),
            error::ServerErrorKind::MetaKvExists
        );
    }
}
