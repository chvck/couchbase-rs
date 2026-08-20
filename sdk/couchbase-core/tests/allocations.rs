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
    ensure_manifest, feature_supported, generate_bytes_value, generate_key, generate_string_value,
    try_until,
};
use crate::common::test_agent::TestAgent;
use crate::common::test_config::run_test;
use couchbase_core::features::BucketFeature;
use couchbase_core::memdx::durability_level::DurabilityLevel;
use couchbase_core::memdx::ops_rangescan::{RangeScanCreateRangeScanConfig, RangeScanItemIter};
use couchbase_core::memdx::subdoc::{LookupInOp, LookupInOpType};
use couchbase_core::options::crud::{
    AddOptions, GetOptions, LookupInOptions, ReplaceOptions, UpsertOptions,
};
use couchbase_core::options::management::CreateCollectionOptions;
use couchbase_core::options::query::QueryOptions;
use couchbase_core::options::rangescan::{
    RangeScanCancelOptions, RangeScanContinueOptions, RangeScanCreateOptions,
};
use couchbase_core::options::waituntilready::WaitUntilReadyOptions;
use couchbase_core::retryfailfast::FailFastRetryStrategy;
use couchbase_core::service_type::ServiceType;
use futures::StreamExt;
use serial_test::serial;
use std::future::Future;
use std::ops::Add;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

mod common;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// Silences the test logger before it initialises, for this binary only.
///
/// The shared harness defaults to `TRACE` when `RUST_LOG` is unset, and memdx
/// logs two records per round trip at roughly four allocations each -- so about
/// eight of every count below was `chrono` formatting a timestamp, and the
/// budgets measured the logger more than the client. Worse, they measured
/// whichever of the two the ambient environment picked: the same tree passed
/// with `RUST_LOG` unset and failed every case with it set.
///
/// Every test calls this first. The logger initialises once per process, on
/// whichever test runs first, so setting it here is enough.
#[cfg(feature = "dhat-heap")]
fn measure_the_client_not_the_logger() {
    std::env::set_var("RUST_LOG", "off");
}

#[serial]
#[cfg(feature = "dhat-heap")]
#[test]
fn upsert() {
    measure_the_client_not_the_logger();
    run_test(async |mut agent| {
        let key = generate_key();
        let value = generate_bytes_value(32);
        let key_clone = key.clone();
        let value_clone = value.clone();

        let upsert_opts = UpsertOptions::new(key_clone.as_slice(), "", "", value_clone.as_slice())
            .retry_strategy(Arc::new(FailFastRetryStrategy::default()));
        let expected_allocs: u64 = if agent.test_setup_config.use_ssl {
            3
        } else {
            1
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
    measure_the_client_not_the_logger();
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
            3
        } else {
            1
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
    measure_the_client_not_the_logger();
    run_test(async |mut agent| {
        let keys: Vec<Vec<u8>> = (0..WARMUP_RUNS + MEASURED_RUNS)
            .map(|_| generate_key())
            .collect();
        let value = generate_bytes_value(32);
        let strategy: Arc<dyn couchbase_core::retry::RetryStrategy> =
            Arc::new(FailFastRetryStrategy::default());

        let expected_allocs: u64 = if agent.test_setup_config.use_ssl {
            3
        } else {
            1
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
    measure_the_client_not_the_logger();
    run_test(async |mut agent| {
        let key = generate_key();
        let value = generate_bytes_value(32);
        let key_clone = key.clone();
        let value_clone = value.clone();

        let opts = ReplaceOptions::new(key_clone.as_slice(), "", "", value_clone.as_slice())
            .retry_strategy(Arc::new(FailFastRetryStrategy::default()));
        let expected_allocs: u64 = if agent.test_setup_config.use_ssl {
            3
        } else {
            1
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
    measure_the_client_not_the_logger();
    run_test(async |mut agent| {
        let key = generate_key();
        let value = generate_bytes_value(32);
        let key_clone = key.clone();

        let opts = GetOptions::new(key_clone.as_slice(), "", "")
            .retry_strategy(Arc::new(FailFastRetryStrategy::default()));

        let expected_allocs: u64 = if agent.test_setup_config.use_ssl {
            4
        } else {
            2
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
    measure_the_client_not_the_logger();
    run_test(async |mut agent| {
        let opts = QueryOptions::default()
            .statement("SELECT 1=1".to_string())
            .retry_strategy(Arc::new(FailFastRetryStrategy::default()));

        // The query path is HTTP, not KV, so it does not share the KV budget:
        // this pins the request encode plus the row and metadata decode of a
        // single-row response.
        let expected_allocs: u64 = if agent.test_setup_config.use_ssl {
            197
        } else {
            194
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

/// **A durable mutation's durability frame does not touch the heap.**
///
/// The frame is one byte for the level and two more for a timeout. It used to be
/// built with `vec![level]` -- capacity exactly one -- and then pushed twice, so
/// three bytes cost an allocation plus up to two reallocations, on every
/// mutation that asked for durability. Nothing pinned that, which is why it
/// survived: the plain `upsert` case above never sets a durability level.
#[serial]
#[cfg(feature = "dhat-heap")]
#[test]
fn durable_upsert() {
    measure_the_client_not_the_logger();
    run_test(async |mut agent| {
        let key = generate_key();
        let value = generate_bytes_value(32);
        let key_clone = key.clone();
        let value_clone = value.clone();

        let opts = UpsertOptions::new(key_clone.as_slice(), "", "", value_clone.as_slice())
            .durability_level(DurabilityLevel::MAJORITY)
            .retry_strategy(Arc::new(FailFastRetryStrategy::default()));

        // The same budget as a plain upsert: asking for durability adds a frame
        // extra, and a frame extra is not a reason to allocate.
        //
        // This covers the level-only frame, which is the only one reachable:
        // `crudcomponent` passes `durability_level_timeout: None` at all five of
        // its mutation sites and no option sets it, so the three-byte
        // level-and-timeout frame cannot be built through the Agent at all.
        let expected_allocs: u64 = if agent.test_setup_config.use_ssl {
            3
        } else {
            1
        };

        ensure_agent_ready(&agent).await;

        create_doc(key, value, "", &agent).await;

        run_allocation_test(agent, expected_allocs, async |agent: &TestAgent, _run| {
            agent.upsert(opts.clone()).await.unwrap();
        })
        .await
    });
}

/// **A subdoc lookup costs nothing per path, on the way out or on the way back.**
///
/// `lookup_in` used to build a `Vec<Vec<u8>>` of `path.to_vec()` and then copy
/// each one into the request body four lines later, so an N-path lookup made N
/// allocations to hold bytes it already borrowed, plus one more for a
/// single-byte extras block. Decode then did the mirror image of that: a
/// `vec![0; len]` and a `read_exact` per result, copying values out of a frame
/// that is already a refcounted `Bytes`. Both sides are views now, so the count
/// below is flat in the number of paths. Three paths are used here rather than
/// one so that a per-path cost cannot hide in a fixed one; two of them return a
/// value and the third is an `Exists`, which returns none.
#[serial]
#[cfg(feature = "dhat-heap")]
#[test]
fn lookup_in_three_paths() {
    measure_the_client_not_the_logger();
    run_test(async |mut agent| {
        let key = generate_key();
        let key_clone = key.clone();

        let ops = [
            LookupInOp::new(LookupInOpType::Get, b"a"),
            LookupInOp::new(LookupInOpType::Get, b"b"),
            LookupInOp::new(LookupInOpType::Exists, b"c"),
        ];

        let opts = LookupInOptions::new(key_clone.as_slice(), "", "", &ops)
            .retry_strategy(Arc::new(FailFastRetryStrategy::default()));

        // Nothing per path, in either direction. What remains is the request
        // body, which is genuinely sized at run time, and the `Vec` of results.
        let expected_allocs: u64 = if agent.test_setup_config.use_ssl {
            6
        } else {
            4
        };

        ensure_agent_ready(&agent).await;

        create_doc(key, br#"{"a":1,"b":2,"c":3}"#.to_vec(), "", &agent).await;

        run_allocation_test(agent, expected_allocs, async |agent: &TestAgent, _run| {
            agent.lookup_in(opts.clone()).await.unwrap();
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

// ---------------------------------------------------------------------------
// Range scan
// ---------------------------------------------------------------------------

/// How many documents to put into the one vbucket these tests scan.
///
/// Enough that a chunk of [`SCAN_LARGE_CHUNK`] leaves more behind, so both
/// measured shapes are the same three round trips and differ only in how many
/// documents came back.
#[cfg(feature = "dhat-heap")]
const SCAN_DOCS_IN_VBUCKET: usize = 16;

/// The two chunk sizes the per-document claim is made with.
#[cfg(feature = "dhat-heap")]
const SCAN_SMALL_CHUNK: u32 = 1;
#[cfg(feature = "dhat-heap")]
const SCAN_LARGE_CHUNK: u32 = 8;

/// The end of the key space, for a scan with no upper bound within its prefix.
#[cfg(feature = "dhat-heap")]
const SCAN_KEY_MAX: &[u8] = &[0xff; 16];

/// The router's key hash, mirrored so the seed can choose a vbucket.
#[cfg(feature = "dhat-heap")]
fn vbucket_for(key: &[u8], num_vbuckets: usize) -> u16 {
    let mid_bits = (crc32fast::hash(key) >> 16) as u16 & 0x7fff;
    mid_bits % num_vbuckets as u16
}

/// **What a range scan costs, and what a document within one costs.**
///
/// A whole-collection scan is one scan per vbucket — placement is by hash of the
/// key, so a key range prunes nothing — which makes its cost a fixed term per
/// vbucket plus a term per document. This pins both, and separates them the only
/// way an exactly-comparable measurement can: by running the *same three round
/// trips* over a different number of documents.
///
/// One measured run is a create, one continue read to the end of its stream, and
/// a cancel. It is bounded by `max_count` rather than drained to completion
/// because how many continues a full drain takes is the server's chunking
/// decision, not this client's — and a count that moves with the server's
/// discretion cannot be asserted exactly.
///
/// | | cbcore-rs, as measured there | here |
/// |---|---|---|
/// | per vbucket | 7.0, after two changes took it from 12.0 | 8, in the client |
/// | per document | 0.02–0.03 | exactly 0, asserted as an equality |
///
/// The absolute figures are not comparable between the two crates — a `get` is 2
/// allocations there and 11 here, because the memdx layers differ — but the two
/// changes measured there are both in this port: the create's JSON body is built
/// without owned `String`s, and the continue's 28-byte extras block lives on the
/// stack. Undo either and this test moves.
///
/// `SCAN_ALLOCS` is 6, against cbcore-rs's measured 7.0. The create and the
/// cancel answer once each and correlate through a one-shot that does not
/// allocate; the continue answers with a stream and still pays for its channel.
/// The other two are the create's JSON body and the bucket-feature check. See [`measure_the_client_not_the_logger`] for
/// why it used to read 32.
#[serial]
#[cfg(feature = "dhat-heap")]
#[test]
fn range_scan() {
    measure_the_client_not_the_logger();
    run_test(async |agent| {
        if !feature_supported(&agent, BucketFeature::RangeScan).await {
            eprintln!("  skipped: the bucket does not support range scan");
            return;
        }

        ensure_agent_ready(&agent).await;

        let num_vbuckets = agent.num_vbuckets().await.unwrap();

        // Seeded into a single vbucket, so the number of documents in range is
        // exactly known rather than however the key hash happened to spread them.
        let prefix = format!("allocscan-{}-", generate_string_value(8));
        let mut keys: Vec<String> = Vec::with_capacity(SCAN_DOCS_IN_VBUCKET);
        let mut chosen = None;
        let mut candidate = 0usize;
        while keys.len() < SCAN_DOCS_IN_VBUCKET {
            let key = format!("{prefix}{candidate:06}");
            candidate += 1;
            let key_vb = vbucket_for(key.as_bytes(), num_vbuckets);
            match chosen {
                None => {
                    chosen = Some(key_vb);
                    keys.push(key);
                }
                Some(vb) if vb == key_vb => keys.push(key),
                _ => {}
            }
        }
        let vbucket_id = chosen.unwrap();

        let strategy: Arc<dyn couchbase_core::retry::RetryStrategy> =
            Arc::new(FailFastRetryStrategy::default());
        for key in &keys {
            agent
                .upsert(
                    UpsertOptions::new(key.as_bytes(), "", "", br#"{"a":1}"#)
                        .retry_strategy(strategy.clone()),
                )
                .await
                .unwrap();
        }

        let mut end = prefix.as_bytes().to_vec();
        end.extend_from_slice(SCAN_KEY_MAX);

        let create_opts = RangeScanCreateOptions::new("", "", vbucket_id)
            .range(RangeScanCreateRangeScanConfig {
                start: Some(prefix.as_bytes()),
                end: Some(&end),
                exclusive_start: None,
                exclusive_end: None,
            })
            .retry_strategy(strategy.clone());
        let unbounded = RangeScanContinueOptions::new();
        let small = RangeScanContinueOptions::new().max_count(SCAN_SMALL_CHUNK);
        let large = RangeScanContinueOptions::new().max_count(SCAN_LARGE_CHUNK);
        let cancel_opts = RangeScanCancelOptions::new();

        // A range scan reads the vbucket's persisted state, so a document is not
        // in range the instant its write is acknowledged. Nothing is measured
        // until every seeded document is readable, or the two shapes would not
        // be reading the same data.
        try_until(
            Instant::now().add(Duration::from_secs(30)),
            Duration::from_millis(250),
            "the seeded documents did not become scannable in time",
            || async {
                let seen =
                    scan_one_chunk(&agent, &create_opts, &unbounded, &cancel_opts, false).await;
                Ok(if seen == SCAN_DOCS_IN_VBUCKET {
                    Some(())
                } else {
                    None
                })
            },
        )
        .await;

        let small_op = async |agent: &TestAgent, _run: usize| {
            let seen = scan_one_chunk(agent, &create_opts, &small, &cancel_opts, true).await;
            assert_eq!(seen, SCAN_SMALL_CHUNK as usize);
        };
        let large_op = async |agent: &TestAgent, _run: usize| {
            let seen = scan_one_chunk(agent, &create_opts, &large, &cancel_opts, true).await;
            assert_eq!(seen, SCAN_LARGE_CHUNK as usize);
        };

        for run in 0..WARMUP_RUNS {
            small_op(&agent, run).await;
            large_op(&agent, run).await;
        }

        let profiler = dhat::Profiler::builder().testing().build();

        let (small_min, small_worst) = measure_allocations(&agent, WARMUP_RUNS, small_op).await;
        let (large_min, large_worst) = measure_allocations(&agent, WARMUP_RUNS, large_op).await;

        let per_document =
            (large_min as f64 - small_min as f64) / (SCAN_LARGE_CHUNK - SCAN_SMALL_CHUNK) as f64;
        eprintln!(
            "  range scan on vbucket {vbucket_id} of {num_vbuckets}, \
             {SCAN_DOCS_IN_VBUCKET} documents in range"
        );
        eprintln!(
            "    create + continue({SCAN_SMALL_CHUNK}) + cancel: {small_min} \
             allocations (worst {small_worst})"
        );
        eprintln!(
            "    create + continue({SCAN_LARGE_CHUNK}) + cancel: {large_min} \
             allocations (worst {large_worst})"
        );
        eprintln!(
            "    => {per_document:.2} per document, over {} more documents",
            SCAN_LARGE_CHUNK - SCAN_SMALL_CHUNK
        );

        // The per-vbucket term: everything a scan costs that is not a document.
        let expected_allocs: u64 = if agent.test_setup_config.use_ssl {
            SCAN_ALLOCS + 2 * SCAN_ROUND_TRIPS
        } else {
            SCAN_ALLOCS
        };
        dhat::assert_eq!(
            small_min,
            expected_allocs,
            "a create, a continue and a cancel allocated {} times, not {}. If \
             this is a deliberate improvement, lower SCAN_ALLOCS; if it is a \
             regression, look first at the create's JSON body and the \
             continue's extras block, which are the two terms deliberately \
             kept off the heap. If it is 32, the logger is inside the \
             measurement and this binary failed to silence it.",
            small_min,
            expected_allocs
        );

        // The per-document term, which is the claim that reading a scan does not
        // allocate per item: the same three round trips over eight times the
        // documents must cost the same.
        dhat::assert_eq!(
            large_min,
            small_min,
            "reading {} documents cost {} allocations against {} for reading \
             {} — something on the item decode path is now allocating per \
             document, which is the term that scales with the data.",
            SCAN_LARGE_CHUNK,
            large_min,
            small_min,
            SCAN_SMALL_CHUNK
        );

        drop(profiler);
    });
}

/// The measured operation: open a scan, read one chunk of it, release it.
///
/// Returns how many documents the chunk carried. The options are borrowed rather
/// than built here — building them allocates an `Arc` for the retry strategy,
/// which would be counted against the scan.
#[cfg(feature = "dhat-heap")]
async fn scan_one_chunk(
    agent: &TestAgent,
    create_opts: &RangeScanCreateOptions<'_>,
    continue_opts: &RangeScanContinueOptions,
    cancel_opts: &RangeScanCancelOptions,
    expect_more: bool,
) -> usize {
    let scan = agent
        .range_scan_create(create_opts.clone())
        .await
        .expect("the seeded vbucket should open a scan");

    let mut items = 0usize;
    let res = scan
        .continue_scan(continue_opts, |resp| {
            // Counted, not collected: an item is a slice of the packet it
            // arrived in, and keeping one is what costs.
            items += match resp.items {
                RangeScanItemIter::Full(iter) => iter.count(),
                RangeScanItemIter::KeyOnly(iter) => iter.count(),
            };
        })
        .await
        .expect("continuing a scan just created should succeed");

    if expect_more {
        assert!(
            res.more,
            "a bounded chunk of a {SCAN_DOCS_IN_VBUCKET} document vbucket \
             should leave more behind, or the two measured shapes are not the \
             same three round trips"
        );
        scan.cancel(cancel_opts)
            .await
            .expect("cancelling an open scan should succeed");
    } else {
        // The unbounded read used to wait for persistence: drain it rather than
        // leaving a scan open behind us.
        while !res.complete
            && !scan
                .continue_scan(continue_opts, |_| {})
                .await
                .expect("draining should succeed")
                .complete
        {}
    }

    items
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

/// What a create, one bounded continue and a cancel allocate, measured.
///
/// 8 in the client and 24 in the logger, which this harness measures at its
/// the logger silenced, along with every other budget here.
///
/// **Lowering this is a change to the assertion, not a failure.** See
/// [`range_scan`] for what the number is made of and which parts of it are this
/// crate's to spend.
#[cfg(feature = "dhat-heap")]
const SCAN_ALLOCS: u64 = 6;

/// Round trips in one measured scan run: the create, the continue, the cancel.
///
/// Only used to infer the TLS arm. The cluster this was measured against is not
/// TLS, and a `get` costs 11 without it and 13 with, so the inference is two
/// allocations per round trip. It is an inference, not a measurement.
#[cfg(feature = "dhat-heap")]
const SCAN_ROUND_TRIPS: u64 = 3;

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

    let (min, worst) = measure_allocations(&agent, WARMUP_RUNS, op).await;

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

/// Run `op` `MEASURED_RUNS` times and return the cleanest and worst counts.
///
/// A profiler must already be running; warm-up is the caller's business, because
/// a test measuring two shapes of the same operation warms both before it
/// measures either.
#[cfg(feature = "dhat-heap")]
async fn measure_allocations<Op>(agent: &TestAgent, first_run: usize, op: Op) -> (u64, u64)
where
    Op: AsyncFn(&TestAgent, usize),
{
    let mut min = u64::MAX;
    let mut worst = 0u64;
    for run in 0..MEASURED_RUNS {
        let before = dhat::HeapStats::get().total_blocks;
        op(agent, first_run + run).await;
        let delta = dhat::HeapStats::get().total_blocks - before;
        min = min.min(delta);
        worst = worst.max(delta);
    }
    (min, worst)
}
