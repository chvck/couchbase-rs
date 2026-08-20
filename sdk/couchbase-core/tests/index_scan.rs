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

//! Index scans against a real cluster.
//!
//! The unit tests in `indexrouter` pin the routing rules against captured
//! responses. These pin the two claims a capture cannot: that
//! `/getIndexStatus` really does name nodes by their mgmt port, so the join
//! against the cluster config resolves; and that a partitioned index really
//! does come back as one row per host, so a scattered scan reads the whole
//! index and each entry exactly once.

use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::time::Duration;

use futures::StreamExt;
use serial_test::serial;

use couchbase_core::error::ErrorKind;
use couchbase_core::indexrouter::RouteError;
use couchbase_core::mutationtoken::MutationToken;
use couchbase_core::options::crud::UpsertOptions;
use couchbase_core::options::index::{IndexScanConsistency, IndexScanOptions};
use couchbase_core::options::query::QueryOptions;
use couchbase_core::results::index_scan::IndexScanResults;

use couchbase_core::agent::Agent;

use crate::common::default_agent_options::create_options_without_bucket;
use crate::common::test_agent::TestAgent;
use crate::common::test_config::{run_test, setup_test};

mod common;

/// Enough documents that four partitions are all very likely to hold some,
/// while staying quick to write. Nothing asserts that they are evenly spread —
/// that is the server's business, and asserting it would be asserting a hash.
const DOCS: usize = 100;

const PARTITIONS: u32 = 4;

/// A name nothing else on this shared cluster will collide with. The cluster
/// carries leftovers from other work, so every index made here is made under a
/// fresh name and dropped again.
fn unique(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

/// Write `DOCS` documents tagged with `tag`, and return their keys and the
/// mutation tokens the writes produced.
async fn seed(agent: &TestAgent, tag: &str) -> (HashSet<String>, Vec<MutationToken>) {
    let mut keys = HashSet::new();
    let mut tokens = Vec::new();

    for i in 0..DOCS {
        let key = format!("{tag}_{i}");
        let value = format!("{{\"e5_tag\":\"{tag}\",\"n\":{i}}}");

        let result = agent
            .upsert(UpsertOptions::new(key.as_bytes(), "", "", value.as_bytes()))
            .await
            .expect("the document is written");

        tokens.push(
            result
                .mutation_token
                .expect("mutation tokens are on by default"),
        );
        keys.insert(key);
    }

    (keys, tokens)
}

/// Run a statement and drain it, so that a DDL statement has finished when this
/// returns.
///
/// Goes to the agent rather than to `TestAgent::query`, whose ten-second
/// deadline is a sensible ceiling for a query and not for building an index.
async fn statement(agent: &TestAgent, statement: String) {
    let mut result = agent
        .deref()
        .query(QueryOptions::default().statement(statement.clone()))
        .await
        .unwrap_or_else(|e| panic!("{statement} failed: {e}"));

    while let Some(row) = result.next().await {
        row.unwrap_or_else(|e| panic!("{statement} failed mid-stream: {e}"));
    }
}

/// Every primary key the scan yields, read through the aggregate stream.
async fn drain(results: &mut IndexScanResults) -> Vec<String> {
    let mut keys = Vec::new();
    while let Some(entry) = results.next().await {
        let entry = entry.expect("an entry rather than a failure");
        keys.push(String::from_utf8(entry.primary_key.to_vec()).expect("keys are utf-8"));
    }
    keys
}

#[serial]
#[test]
fn test_index_scan_reads_an_unpartitioned_index_in_one_stream() {
    run_test(async |agent| {
        let tag = unique("e5_plain");
        let index = unique("ix_e5_plain");
        let (keys, _tokens) = seed(&agent, &tag).await;
        let bucket = agent.test_setup_config.bucket.clone();

        statement(
            &agent,
            format!("CREATE INDEX `{index}` ON `{bucket}`(`n`) WHERE `e5_tag` = \"{tag}\""),
        )
        .await;

        let mut results = agent
            .index_scan(
                &IndexScanOptions::new("_default", "_default", &index).timeout(
                    // Well inside the indexer's own 120-second budget, so a hang
                    // fails the test rather than the suite's patience.
                    Duration::from_secs(30),
                ),
            )
            .await
            .expect("the index routes and scans");

        assert_eq!(results.stream_count(), 1, "one host holds the whole index");
        assert!(
            results.is_index_ordered(),
            "one stream is by definition in index order"
        );
        assert_ne!(results.defn_id(), 0, "a scan addresses a definition id");

        let found = drain(&mut results).await;

        assert_eq!(
            found.iter().cloned().collect::<HashSet<_>>(),
            keys,
            "the scan reads exactly the documents the index covers"
        );
        assert_eq!(found.len(), DOCS, "and each of them once");

        statement(&agent, format!("DROP INDEX `{index}` ON `{bucket}`")).await;
    });
}

#[serial]
#[test]
fn test_index_scan_of_a_partitioned_index_reads_every_partition_once() {
    run_test(async |agent| {
        let tag = unique("e5_part");
        let index = unique("ix_e5_part");
        let (keys, tokens) = seed(&agent, &tag).await;
        let bucket = agent.test_setup_config.bucket.clone();

        statement(
            &agent,
            format!(
                "CREATE INDEX `{index}` ON `{bucket}`(`n`) \
                 PARTITION BY hash((meta().`id`)) \
                 WHERE `e5_tag` = \"{tag}\" \
                 WITH {{\"num_partition\": {PARTITIONS}}}"
            ),
        )
        .await;

        // at_plus over the tokens the writes returned, which is the scan-vector
        // bridge doing its job against a real indexer: every one of these
        // documents must be visible, and the indexer is the one that waits.
        let results = agent
            .index_scan(
                &IndexScanOptions::new("_default", "_default", &index)
                    .consistency(IndexScanConsistency::AtPlus(tokens))
                    .timeout(Duration::from_secs(30)),
            )
            .await
            .expect("the partitioned index routes and scans");

        let streams = results.stream_count();
        assert!(
            streams > 1,
            "a {PARTITIONS}-partition index on a multi-node cluster should scatter, got {streams} stream(s)"
        );
        assert!(
            !results.is_index_ordered(),
            "a scattered scan is not in index order and must not claim to be"
        );

        // The routing claim itself: one stream per host, each holding its own
        // partitions, together covering the index exactly once.
        let mut by_partition: HashMap<u64, String> = HashMap::new();
        let mut found = Vec::new();
        for mut stream in results.into_streams() {
            let address = stream.address().to_string();
            for partition in stream.partitions() {
                assert!(
                    by_partition.insert(*partition, address.clone()).is_none(),
                    "partition {partition} is claimed by two hosts"
                );
            }

            while let Some(entry) = stream.next().await {
                let entry = entry.expect("an entry rather than a failure");
                found.push(String::from_utf8(entry.primary_key.to_vec()).expect("keys are utf-8"));
            }
        }

        println!("index {index} scattered over {streams} host(s): {by_partition:?}");

        let mut covered: Vec<u64> = by_partition.keys().copied().collect();
        covered.sort_unstable();
        assert_eq!(
            covered,
            (1..=u64::from(PARTITIONS)).collect::<Vec<_>>(),
            "every partition is read, and they are numbered from 1"
        );
        assert!(
            by_partition.values().collect::<HashSet<_>>().len() > 1,
            "the partitions are on more than one host: {by_partition:?}"
        );

        assert_eq!(
            found.iter().cloned().collect::<HashSet<_>>(),
            keys,
            "the scattered scan reads the whole index"
        );
        assert_eq!(
            found.len(),
            DOCS,
            "and reads each document once, not once per host"
        );

        statement(&agent, format!("DROP INDEX `{index}` ON `{bucket}`")).await;
    });
}

#[serial]
#[test]
fn test_index_scan_of_a_missing_index_is_a_typed_routing_error() {
    run_test(async |agent| {
        // Refused by the router against a freshly read topology, so this also
        // proves the refresh happened: an empty snapshot answers `NoTopology`.
        let err = agent
            .index_scan(&IndexScanOptions::new(
                "_default",
                "_default",
                "ix_e5_definitely_not_here",
            ))
            .await
            .expect_err("there is no such index");

        match err.kind() {
            ErrorKind::IndexRouting(RouteError::IndexNotFound(index)) => {
                assert_eq!(index.name, "ix_e5_definitely_not_here");
                assert_eq!(index.scope, "_default");
            }
            other => panic!("expected a routing error naming the index, got {other:?}"),
        }
    });
}

#[serial]
#[test]
fn test_index_scan_without_a_bucket_says_so() {
    setup_test(async |config| {
        // The keyspace is the agent's bucket plus the scope and collection in
        // the options, so an agent with no bucket cannot name one.
        let agent = Agent::new(create_options_without_bucket(config).await)
            .await
            .unwrap();

        let err = agent
            .index_scan(&IndexScanOptions::new("_default", "_default", "ix_e5_any"))
            .await
            .expect_err("no bucket, no keyspace");

        assert_eq!(&ErrorKind::NoBucket, err.kind());
    });
}
