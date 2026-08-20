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
use crate::common::helpers::{
    ensure_manifest, generate_bytes_value, generate_key, generate_string_value,
};
use crate::common::test_agent::TestAgent;
use crate::common::test_config::run_test;
use couchbase_core::options::crud::{AddOptions, GetOptions, ReplaceOptions, UpsertOptions};
use couchbase_core::options::management::CreateCollectionOptions;
use couchbase_core::options::query::QueryOptions;
use couchbase_core::options::waituntilready::WaitUntilReadyOptions;
use couchbase_core::retryfailfast::FailFastRetryStrategy;
use couchbase_core::service_type::ServiceType;
use futures::StreamExt;
use serial_test::serial;
use std::future::Future;
use std::sync::Arc;

mod common;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[serial]
#[cfg(feature = "dhat-heap")]
#[test]
fn upsert() {
    run_test(async |mut agent| {
        let key = generate_key();
        let value = generate_bytes_value(32);
        let key_clone = key.clone();
        let value_clone = value.clone();

        let upsert_opts = UpsertOptions::new(key_clone.as_slice(), "", "", value_clone.as_slice())
            .retry_strategy(Arc::new(FailFastRetryStrategy::default()));
        let expected_allocs: u64 = if agent.test_setup_config.use_ssl {
            12
        } else {
            10
        };

        ensure_agent_ready(&agent).await;

        create_doc(key, value, "", &agent).await;

        run_allocation_test(agent, expected_allocs, async |agent: &TestAgent, _run| {
            agent.upsert(upsert_opts.clone()).await.unwrap();
        })
        .await
    });
}

#[serial]
#[cfg(feature = "dhat-heap")]
#[test]
fn upsert_against_new_collection() {
    run_test(async |mut agent| {
        let key = generate_key();
        let value = generate_bytes_value(32);
        let key_clone = key.clone();
        let value_clone = value.clone();

        let collection_name = generate_string_value(10);

        let resp = agent
            .create_collection(&CreateCollectionOptions::new(
                &agent.test_setup_config.bucket,
                &agent.test_setup_config.scope,
                &collection_name,
            ))
            .await
            .unwrap();

        ensure_manifest(&agent, &agent.test_setup_config.bucket, resp.manifest_uid).await;

        let upsert_opts = UpsertOptions::new(
            key_clone.as_slice(),
            "",
            &collection_name,
            value_clone.as_slice(),
        )
        .retry_strategy(Arc::new(FailFastRetryStrategy::default()));

        // Same budget as `upsert` against the default collection: a warm
        // fast-cache hit for a named collection now costs nothing over the
        // shortcut that skips the resolver entirely.
        let expected_allocs: u64 = if agent.test_setup_config.use_ssl {
            12
        } else {
            10
        };

        ensure_agent_ready(&agent).await;

        create_doc(key, value, &collection_name, &agent).await;

        run_allocation_test(agent, expected_allocs, async |agent: &TestAgent, _run| {
            agent.upsert(upsert_opts.clone()).await.unwrap();
        })
        .await
    });
}

#[serial]
#[cfg(feature = "dhat-heap")]
#[test]
fn add() {
    run_test(async |mut agent| {
        let keys: Vec<Vec<u8>> = (0..WARMUP_RUNS + MEASURED_RUNS)
            .map(|_| generate_key())
            .collect();
        let value = generate_bytes_value(32);
        let strategy: Arc<dyn couchbase_core::retry::RetryStrategy> =
            Arc::new(FailFastRetryStrategy::default());

        let expected_allocs: u64 = if agent.test_setup_config.use_ssl {
            12
        } else {
            10
        };

        ensure_agent_ready(&agent).await;

        run_allocation_test(agent, expected_allocs, async |agent: &TestAgent, run| {
            let opts = AddOptions::new(keys[run].as_slice(), "", "", value.as_slice())
                .retry_strategy(strategy.clone());
            agent.add(opts).await.unwrap();
        })
        .await
    });
}

#[serial]
#[cfg(feature = "dhat-heap")]
#[test]
fn replace() {
    run_test(async |mut agent| {
        let key = generate_key();
        let value = generate_bytes_value(32);
        let key_clone = key.clone();
        let value_clone = value.clone();

        let opts = ReplaceOptions::new(key_clone.as_slice(), "", "", value_clone.as_slice())
            .retry_strategy(Arc::new(FailFastRetryStrategy::default()));
        let expected_allocs: u64 = if agent.test_setup_config.use_ssl {
            12
        } else {
            10
        };

        ensure_agent_ready(&agent).await;

        create_doc(key, value, "", &agent).await;

        run_allocation_test(agent, expected_allocs, async |agent: &TestAgent, _run| {
            agent.replace(opts.clone()).await.unwrap();
        })
        .await
    });
}

#[serial]
#[cfg(feature = "dhat-heap")]
#[test]
fn get() {
    run_test(async |mut agent| {
        let key = generate_key();
        let value = generate_bytes_value(32);
        let key_clone = key.clone();

        let opts = GetOptions::new(key_clone.as_slice(), "", "")
            .retry_strategy(Arc::new(FailFastRetryStrategy::default()));

        let expected_allocs: u64 = if agent.test_setup_config.use_ssl {
            13
        } else {
            11
        };

        ensure_agent_ready(&agent).await;

        create_doc(key, value, "", &agent).await;

        run_allocation_test(agent, expected_allocs, async |agent: &TestAgent, _run| {
            agent.get(opts.clone()).await.unwrap();
        })
        .await
    });
}

#[serial]
#[cfg(feature = "dhat-heap")]
#[test]
fn query() {
    run_test(async |mut agent| {
        let opts = QueryOptions::default()
            .statement("SELECT 1=1".to_string())
            .retry_strategy(Arc::new(FailFastRetryStrategy::default()));

        // The query path is HTTP, not KV, so it does not share the KV budget:
        // this pins the request encode plus the row and metadata decode of a
        // single-row response.
        let expected_allocs: u64 = if agent.test_setup_config.use_ssl {
            225
        } else {
            222
        };

        ensure_query_ready(&agent).await;

        run_allocation_test(agent, expected_allocs, async |agent: &TestAgent, _run| {
            let mut res = agent.query(opts.clone()).await.unwrap();
            while let Some(row) = res.next().await {
                row.unwrap();
            }
            res.metadata().unwrap();
        })
        .await
    });
}

async fn ensure_query_ready(agent: &TestAgent) {
    agent
        .wait_until_ready(&WaitUntilReadyOptions::new().service_types(vec![ServiceType::QUERY]))
        .await
        .unwrap();
}

async fn ensure_agent_ready(agent: &TestAgent) {
    agent
        .wait_until_ready(&WaitUntilReadyOptions::new().service_types(vec![ServiceType::MEMD]))
        .await
        .unwrap();
}

async fn create_doc(key: Vec<u8>, value: Vec<u8>, collection: &str, agent: &TestAgent) {
    let strat = Arc::new(FailFastRetryStrategy::default());

    let upsert_opts = UpsertOptions::new(key.as_slice(), "", collection, value.as_slice())
        .retry_strategy(strat.clone());

    // make sure that all the underlying resources are setup.
    agent.upsert(upsert_opts.clone()).await.unwrap();
}

/// Warm-up runs, excluded from the measurement.
///
/// The first operation against a collection resolves it against the manifest
/// and caches the result; the first against a node grows the pool. Counting
/// either would measure setup, not the per-operation cost this pins.
#[cfg(feature = "dhat-heap")]
const WARMUP_RUNS: usize = 100;

/// Measured runs. The budget is compared against the *minimum* of these — the
/// cleanest run is the one where no unrelated background work landed inside
/// the window.
#[cfg(feature = "dhat-heap")]
const MEASURED_RUNS: usize = 200;

/// Runs `op` against a warmed agent and pins its allocation count.
///
/// **The comparison is exact, not a ceiling.** Lowering `expected_allocs` is a
/// change to the assertion, not a failure: an improvement reports here rather
/// than passing silently and letting the budget rot upward later.
#[cfg(feature = "dhat-heap")]
async fn run_allocation_test<Op>(agent: TestAgent, expected_allocs: u64, op: Op)
where
    Op: AsyncFn(&TestAgent, usize),
{
    for run in 0..WARMUP_RUNS {
        op(&agent, run).await;
    }

    let profiler = dhat::Profiler::builder().testing().build();

    let mut min = u64::MAX;
    let mut worst = 0u64;
    for run in 0..MEASURED_RUNS {
        let before = dhat::HeapStats::get().total_blocks;
        op(&agent, WARMUP_RUNS + run).await;
        let delta = dhat::HeapStats::get().total_blocks - before;
        min = min.min(delta);
        worst = worst.max(delta);
    }

    eprintln!("  {min} allocations on the cleanest of {MEASURED_RUNS} runs (worst {worst})");

    dhat::assert_eq!(
        min,
        expected_allocs,
        "the operation allocated {} times, not {}. If this is a deliberate \
         improvement, lower the expected count; if it is a regression, \
         something on the request or response path started allocating per \
         operation.",
        min,
        expected_allocs
    );

    drop(profiler);
}
