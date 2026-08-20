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

//! The indexing service's HTTP surface: what indexes exist, and where.
//!
//! Shaped like `cbmgmtx.Management` — an explicit endpoint, explicit
//! credentials, one node. Which node to ask is orchestration and lives above,
//! the same way `cbmgmtx` knows how to call `/pools` and the agent knows whose
//! `/pools` to call.
//!
//! ### Why this is here rather than in a `services` module
//!
//! `/getIndexStatus` is the indexing service's own API. It belongs beside the
//! indexing service's wire protocol for the same reason `cbqueryx/index.go`
//! belongs beside the query service's — and it replaces the 2,497-line
//! gometa-based metadata provider in the Go client with a `serde` struct and a
//! GET.
//!
//! ### The one thing that will surprise you
//!
//! **`hosts` and the keys of `partition_map` carry the node's *mgmt* port** —
//! `:8091` — not its index HTTP port and not its scan port. Measured against
//! Couchbase 8.0, and contrary to the comment on the Go `IndexStatus` type,
//! which says `host:index_http_port`.
//!
//! So a caller has to translate mgmt → `indexScan` through
//! `/pools/default/nodeServices`, which is at least convenient: nodeServices is
//! keyed by hostname with the mgmt port beside every other service. That
//! translation needs cluster config, which this module deliberately does not
//! have — [`IndexStatus`] reports what the server said and the router above
//! does the mapping.

use std::collections::HashMap;
use std::sync::Arc;

use http::Method;
use serde::Deserialize;

use crate::httpx::client::Client;
use crate::httpx::request::{Auth, OnBehalfOfInfo, Request};

use super::error::{Error, ServerError};

/// One index instance, as `/getIndexStatus` describes it.
///
/// Read an explicit `null` as the field's default.
///
/// **`#[serde(default)]` does not cover this**, and the difference is the whole
/// reason this exists: `default` fills in a field the server *omitted*, and
/// `/getIndexStatus` does not omit — it sends `"partitionMap": null` for an
/// index it has not placed yet. Measured on 8.0 against an index in `"Scheduled
/// for Creation"`, which is a state neither the Go type nor this module knew
/// about, and which also nulls `alternateShardIds`.
///
/// Getting it wrong is expensive out of proportion to the bug: a topology
/// refresh reads *every* index in the cluster, so one index mid-creation makes
/// the whole response unreadable, and every scan that needed a refresh at that
/// moment fails. Applied to every optional field rather than only to the one
/// caught, because "the server nulls what it has not computed" is the rule and
/// `partitionMap` was only the first field to prove it.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Fields the endpoint returns and this client has no use for are omitted
/// rather than carried: progress and completion percentages, the DDL text, the
/// display name with its replica decoration. Adding one back is a line, and
/// carrying them all would make the struct look like a contract we honour.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct IndexStatus {
    /// What a scan addresses. **Not** the name — the wire has no index names.
    #[serde(rename = "defnId")]
    pub defn_id: u64,
    #[serde(rename = "instId", default, deserialize_with = "null_as_default")]
    pub inst_id: u64,

    /// The N1QL name. `name` is the decorated display name and is deliberately
    /// not what this is.
    #[serde(rename = "indexName", default, deserialize_with = "null_as_default")]
    pub index_name: String,

    #[serde(default, deserialize_with = "null_as_default")]
    pub bucket: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub scope: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub collection: String,

    /// `"Ready"`, `"Created"`, `"Building"`, `"Error"`, and others. Only
    /// `Ready` can serve a scan — see [`IndexStatus::is_ready`].
    #[serde(default, deserialize_with = "null_as_default")]
    pub status: String,

    #[serde(rename = "isPrimary", default, deserialize_with = "null_as_default")]
    pub is_primary: bool,
    #[serde(default, deserialize_with = "null_as_default")]
    pub partitioned: bool,

    /// **The partitions on *this row's* host, not the index's total** —
    /// measured on 8.0 against a four-partition index over three nodes, which
    /// comes back as three rows reporting 2, 1 and 1. A non-partitioned index
    /// reports 1, which is the same rule seen from its only case.
    ///
    /// The server's name for it reads as a total, and that is exactly the trap:
    /// a router that believed it would read one host's share and think it had
    /// the whole index. [`IndexStatus::declared_partitions`] is where a real
    /// total can be had.
    #[serde(rename = "numPartition", default, deserialize_with = "null_as_default")]
    pub num_partition: u32,

    /// The DDL the index was created with. Carried for one reason: it is the
    /// only place `/getIndexStatus` states how many partitions the index is
    /// *meant* to have.
    #[serde(default, deserialize_with = "null_as_default")]
    pub definition: String,

    /// `host:mgmt` → the partition ids on that host. **The mgmt port, not the
    /// index port** — see the module docs.
    ///
    /// A non-partitioned index has the single partition 0; a partitioned one
    /// numbers from 1. That is the server's convention and is preserved rather
    /// than normalised, because the numbers go back to it in a `ScanRequest`.
    #[serde(rename = "partitionMap", default, deserialize_with = "null_as_default")]
    pub partition_map: HashMap<String, Vec<u64>>,

    #[serde(default, deserialize_with = "null_as_default")]
    pub hosts: Vec<String>,

    #[serde(rename = "numReplica", default, deserialize_with = "null_as_default")]
    pub num_replica: u32,
    /// 0-based. Instances of one index differ in this and share `defn_id`.
    #[serde(rename = "replicaId", default, deserialize_with = "null_as_default")]
    pub replica_id: u32,

    /// The index's key expressions, in key order. What a span's positions index
    /// into.
    #[serde(rename = "secExprs", default, deserialize_with = "null_as_default")]
    pub sec_exprs: Vec<String>,
    /// The partial-index condition, if it has one.
    #[serde(rename = "where", default, deserialize_with = "null_as_default")]
    pub where_expr: Option<String>,
}

impl IndexStatus {
    /// Whether this instance can serve a scan right now.
    ///
    /// A building index answers a scan with "Index not ready for serving
    /// queries", so checking here turns a round trip into a filter. It is
    /// exact-match on purpose: the states are a closed set the server controls,
    /// and a new one should read as not-ready rather than be guessed at.
    pub fn is_ready(&self) -> bool {
        self.status == "Ready"
    }

    /// How many partitions the index was *declared* with, if its DDL says.
    ///
    /// **The only total the endpoint gives.** `num_partition` is per row, and a
    /// row is a host, so summing the rows only ever says how many partitions
    /// someone is currently serving — which is the number a scan that lost one
    /// would also compute. The `WITH` clause is what it was supposed to be, and
    /// the server echoes the clause back verbatim:
    ///
    /// ```text
    /// CREATE INDEX `x` ON `b`.`s`.`c`(`n`) PARTITION BY hash((meta().`id`))
    ///   WITH {  "nodes":[ … ], "num_partition":4 }
    /// ```
    ///
    /// `None` when the DDL named no count — the server's default applies and
    /// this cannot say what it was — or when the text is not what we think it
    /// is, which is a reason to fall back rather than to guess.
    pub fn declared_partitions(&self) -> Option<u32> {
        let at = self.definition.find("\"num_partition\"")?;
        let rest = self.definition[at..].split_once(':')?.1;
        let digits: String = rest
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok().filter(|n| *n > 0)
    }

    /// The keyspace, as the three names that identify it.
    pub fn keyspace(&self) -> (&str, &str, &str) {
        (&self.bucket, &self.scope, &self.collection)
    }
}

#[derive(Debug, serde::Deserialize)]
struct IndexStatusResponse {
    #[serde(default, deserialize_with = "null_as_default")]
    status: Vec<IndexStatus>,
}

/// The indexing service on **one** node.
///
/// Shaped like [`mgmtx::mgmt::Management`](crate::mgmtx::mgmt::Management): an
/// explicit endpoint, explicit credentials, one node, and the HTTP client handed
/// in rather than owned.
#[derive(Debug)]
pub struct Indexing<C: Client> {
    pub http_client: Arc<C>,
    pub user_agent: String,
    /// The node's index HTTP endpoint, e.g. `http://10.0.0.1:9102`.
    pub endpoint: String,
    pub auth: Auth,
}

impl<C: Client> Indexing<C> {
    /// Every index instance the cluster knows about.
    ///
    /// Asks one node, which answers for the whole cluster — the endpoint
    /// consolidates what its peers report. That is why a caller needs only one
    /// reachable indexer to route a scan, and why a caller that gets an error
    /// should try a different node rather than conclude anything about the
    /// cluster.
    ///
    /// `get_all` includes instances that are not `Ready`. It is on by default
    /// because a caller usually wants to distinguish "building" from "absent",
    /// and those are the same answer if the server filters them out.
    pub async fn get_index_status(
        &self,
        get_all: bool,
        on_behalf_of: Option<OnBehalfOfInfo>,
    ) -> Result<Vec<IndexStatus>, Error> {
        let path = if get_all {
            "getIndexStatus?getAll=true"
        } else {
            "getIndexStatus"
        };

        let request = Request::new(Method::GET, format!("{}/{path}", self.endpoint))
            .auth(self.auth.clone())
            .on_behalf_of(on_behalf_of)
            .user_agent(self.user_agent.clone());

        let response = self.http_client.execute(request).await?;

        let status = response.status();
        let body = response.bytes().await?;

        if !status.is_success() {
            // The endpoint reports failures as a status code and a plain-text
            // body rather than as JSON, so the classification the rest of this
            // package uses applies here too — and "index not found" arriving
            // over HTTP means the same thing it means over the wire.
            return Err(ServerError::classify(format!(
                "getIndexStatus: {status}: {}",
                String::from_utf8_lossy(&body)
            ))
            .into());
        }

        let parsed: IndexStatusResponse = serde_json::from_slice(&body).map_err(|e| {
            Error::new_decoding_error(format!("getIndexStatus returned unreadable JSON: {e}"))
        })?;
        Ok(parsed.status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real 8.0 response. Kept verbatim in shape — the field
    /// names are the contract and paraphrasing them would test the paraphrase.
    // `##` rather than `#`: the sample contains `"#primary"`, whose `"#`
    // closes a single-hash raw string.
    const SAMPLE: &str = r##"{
      "status": [
        {
          "defnId": 8891234567890,
          "instId": 1122334455,
          "name": "ix_a (replica 1)",
          "indexName": "ix_a",
          "bucket": "travel",
          "scope": "inventory",
          "collection": "airline",
          "secExprs": ["`a`.`r`", "`a`.`v`"],
          "where": "(`a`.`z`) != 4",
          "definition": "CREATE INDEX `ix_a` ON `travel`.`inventory`.`airline`((`a`.`r`),(`a`.`v`)) PARTITION BY hash((meta().`id`)) WITH {  \"nodes\":[ \"10.0.0.1:8091\",\"10.0.0.2:8091\" ], \"num_partition\":4 }",
          "status": "Ready",
          "isPrimary": false,
          "partitioned": true,
          "numPartition": 2,
          "partitionMap": { "10.0.0.1:8091": [1, 2] },
          "hosts": ["10.0.0.1:8091"],
          "numReplica": 1,
          "replicaId": 1,
          "completion": 100,
          "progress": 100.0
        },
        {
          "defnId": 8891234567890,
          "instId": 1122334455,
          "name": "ix_a (replica 1)",
          "indexName": "ix_a",
          "bucket": "travel",
          "scope": "inventory",
          "collection": "airline",
          "secExprs": ["`a`.`r`", "`a`.`v`"],
          "where": "(`a`.`z`) != 4",
          "definition": "CREATE INDEX `ix_a` ON `travel`.`inventory`.`airline`((`a`.`r`),(`a`.`v`)) PARTITION BY hash((meta().`id`)) WITH {  \"nodes\":[ \"10.0.0.1:8091\",\"10.0.0.2:8091\" ], \"num_partition\":4 }",
          "status": "Ready",
          "isPrimary": false,
          "partitioned": true,
          "numPartition": 2,
          "partitionMap": { "10.0.0.2:8091": [3, 4] },
          "hosts": ["10.0.0.2:8091"],
          "numReplica": 1,
          "replicaId": 1,
          "completion": 100,
          "progress": 100.0
        },
        {
          "defnId": 42,
          "indexName": "#primary",
          "bucket": "travel",
          "scope": "_default",
          "collection": "_default",
          "status": "Building",
          "isPrimary": true,
          "numPartition": 1,
          "partitionMap": { "10.0.0.1:8091": [0] },
          "hosts": ["10.0.0.1:8091"]
        }
      ]
    }"##;

    fn parse() -> Vec<IndexStatus> {
        serde_json::from_str::<IndexStatusResponse>(SAMPLE)
            .expect("parse")
            .status
    }

    #[test]
    fn a_partitioned_index_is_one_row_per_host_and_not_one_row() {
        // **Measured on 8.0**, and the shape the router is built around: a
        // four-partition index over two hosts is *two* rows sharing a `defnId`
        // and an `instId`, each naming one host and only its own partitions.
        // `numPartition` is that row's share — 2, not 4 — which is why summing
        // the rows that survived a host going away cannot detect that it did.
        let indexes = parse();
        let rows: Vec<&IndexStatus> = indexes.iter().filter(|s| s.index_name == "ix_a").collect();

        assert_eq!(rows.len(), 2, "one row per host");
        assert_eq!(rows[0].defn_id, 8_891_234_567_890);
        assert_eq!(
            rows[0].index_name, "ix_a",
            "the N1QL name, not the display name"
        );
        assert_eq!(rows[0].inst_id, rows[1].inst_id, "one instance, two rows");
        assert_eq!(rows[0].num_partition, 2, "this host's share, not the total");
        assert_eq!(
            rows[0].partition_map["10.0.0.1:8091"],
            vec![1, 2],
            "keyed by the mgmt port, measured against 8.0"
        );
        assert_eq!(rows[1].partition_map["10.0.0.2:8091"], vec![3, 4]);
        assert_eq!(rows[0].keyspace(), ("travel", "inventory", "airline"));
    }

    #[test]
    fn the_declared_partition_count_comes_out_of_the_ddl() {
        // The only total the endpoint gives, and the server echoes the `WITH`
        // clause back verbatim — this is the exact text a 8.0.3 cluster
        // returned for a four-partition index.
        assert_eq!(parse()[0].declared_partitions(), Some(4));
    }

    #[test]
    fn an_index_whose_ddl_named_no_count_declares_nothing() {
        // The server's default applies and the definition cannot say what it
        // was, so this is `None` rather than a guess the router would trust.
        let mut ix = parse()[0].clone();
        ix.definition =
            "CREATE INDEX `ix_a` ON `travel`.`inventory`.`airline`(`a`) PARTITION BY hash((meta().`id`))"
                .to_string();
        assert_eq!(ix.declared_partitions(), None);
        ix.definition = String::new();
        assert_eq!(ix.declared_partitions(), None);
    }

    #[test]
    fn key_expressions_and_a_partial_condition_come_through() {
        // The key expressions are what a span's positions index into, so
        // losing them would make a correct span unwriteable.
        let ix = &parse()[0];
        assert_eq!(ix.sec_exprs, vec!["`a`.`r`", "`a`.`v`"]);
        assert_eq!(ix.where_expr.as_deref(), Some("(`a`.`z`) != 4"));
    }

    #[test]
    fn a_non_partitioned_index_has_the_single_partition_zero() {
        // Partitioned indexes number from 1, unpartitioned ones use 0. The
        // numbers go straight back to the server in a ScanRequest, so they are
        // preserved rather than normalised.
        let ix = &parse()[2];
        assert_eq!(ix.num_partition, 1);
        assert_eq!(ix.partition_map["10.0.0.1:8091"], vec![0]);
    }

    #[test]
    fn only_ready_can_serve_a_scan() {
        let indexes = parse();
        assert!(indexes[0].is_ready());
        assert!(
            !indexes[2].is_ready(),
            "Building would answer a scan with index-not-ready"
        );
    }

    #[test]
    fn replicas_share_a_defn_id_and_differ_in_replica_id() {
        let ix = &parse()[0];
        assert_eq!(ix.replica_id, 1);
        assert_eq!(ix.num_replica, 1);
    }

    #[test]
    fn absent_optional_fields_do_not_fail_the_parse() {
        // The endpoint omits empty fields rather than sending nulls, and it has
        // gained fields every release. A parse that broke on either would make
        // this client version-locked to a cluster for no reason.
        let minimal = r#"{"status":[{"defnId":1,"indexName":"x","status":"Ready"}]}"#;
        let parsed: IndexStatusResponse = serde_json::from_str(minimal).expect("parse");
        assert_eq!(parsed.status[0].defn_id, 1);
        assert!(parsed.status[0].partition_map.is_empty());
        assert_eq!(parsed.status[0].where_expr, None);
    }

    #[test]
    fn an_unknown_field_is_ignored_rather_than_fatal() {
        let future = r#"{"status":[{"defnId":1,"indexName":"x","status":"Ready",
                         "somethingAddedIn82":{"a":1}}]}"#;
        let parsed: IndexStatusResponse = serde_json::from_str(future).expect("parse");
        assert_eq!(parsed.status[0].defn_id, 1);
    }

    #[test]
    fn an_index_that_is_only_scheduled_nulls_its_partition_map() {
        // **Measured on 8.0, and it broke a live gate.** An index in
        // `"Scheduled for Creation"` — a state the Go type does not list —
        // sends `"partitionMap": null` rather than omitting the field, and
        // `#[serde(default)]` covers only the omission.
        //
        // The blast radius is what makes this worth a test: a topology refresh
        // reads every index in the cluster, so one index mid-creation made the
        // whole response unreadable and every scan needing a refresh at that
        // moment failed with "getIndexStatus returned unreadable JSON".
        let scheduled = r#"{"status":[{
            "defnId":4432614232985572278,
            "instId":4432614232985572278,
            "indexName":"qp_np_1",
            "bucket":"default","scope":"_default","collection":"qpgate",
            "status":"Scheduled for Creation",
            "hosts":["172.17.0.3:8091"],
            "partitionMap":null,
            "alternateShardIds":null,
            "secExprs":["`n`","`grp`"],
            "where":"(`grp` = 1)"
        }]}"#;

        let parsed: IndexStatusResponse = serde_json::from_str(scheduled).expect("parse");
        let status = &parsed.status[0];
        assert!(status.partition_map.is_empty());
        assert!(!status.is_ready(), "{}", status.status);
        assert_eq!(status.hosts, ["172.17.0.3:8091"]);
    }

    #[test]
    fn every_optional_field_reads_a_null_as_its_default() {
        // The rule the field above is one instance of: the server nulls what it
        // has not computed, so a null anywhere optional must not cost the whole
        // response. `defnId` is deliberately not in this set — an index status
        // with no id addresses nothing, and defaulting it to zero would invent
        // an index rather than report a broken one.
        let nulled = r#"{"status":[{
            "defnId":7,
            "instId":null,"indexName":null,"bucket":null,"scope":null,
            "collection":null,"status":null,"isPrimary":null,"partitioned":null,
            "numPartition":null,"partitionMap":null,"hosts":null,
            "numReplica":null,"replicaId":null,"secExprs":null,"where":null
        }]}"#;

        let parsed: IndexStatusResponse = serde_json::from_str(nulled).expect("parse");
        let status = &parsed.status[0];

        assert_eq!(status.defn_id, 7, "the one field that is not optional");
        assert_eq!(status.inst_id, 0);
        assert_eq!(status.index_name, "");
        assert_eq!(status.keyspace(), ("", "", ""));
        assert_eq!(status.status, "");
        assert!(!status.is_primary && !status.partitioned);
        assert_eq!(status.num_partition, 0);
        assert!(status.partition_map.is_empty());
        assert!(status.hosts.is_empty());
        assert_eq!((status.num_replica, status.replica_id), (0, 0));
        assert!(status.sec_exprs.is_empty());
        assert_eq!(status.where_expr, None);
    }
}
