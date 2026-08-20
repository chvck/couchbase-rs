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

//! What a range scan fan-out does to a `get` sharing the agent with it.
//!
//! ```bash
//! cargo test -p couchbase-core --test rangescan_latency -- --ignored --nocapture
//! ```
//!
//! `RCBSCANDOCS` sets how many documents to seed (default 1000) and `RCBSCANCHUNK`
//! the chunked row's `max_count` (default 1).
//!
//! **This is a probe, not a pass/fail test**, which is why it is `#[ignore]`d: it
//! prints rows and asserts only that they were measurable. Three rows per round,
//! all from one session, because a treatment without its own control is not a
//! measurement:
//!
//! | row | what it measures |
//! |---|---|
//! | `get_alone` | the control — nothing else running |
//! | `get_with_scan` | the same `get`, while a fan-out drains each vbucket in one continue |
//! | `get_with_chunked_scan` | the same `get`, while a fan-out reads `RCBSCANCHUNK` documents per continue |
//!
//! # Why there are two treatment rows
//!
//! `RangeScanContinue` returns `would_block` server-side and finishes on a
//! background task, and kv_engine's `Connection::executeCommandsCallback` stops
//! executing a connection's queue at the first active command that may not be
//! reordered. So a `get` behind a fan-out waits for scans rather than for itself.
//!
//! But **how much it waits depends on how long the scans stay resident**, and an
//! unbounded continue does not stay resident long: on a 128-vbucket bucket
//! holding a thousand documents each vbucket drains in a single round trip, and
//! the socket is never deeply queued. Bounding the continue is what parks a
//! stream on the connection — and it is an ordinary thing for a caller to do,
//! because it is how you read a large collection without holding it in memory.
//!
//! Both rows are printed because the difference between them is the finding: the
//! first says how little there is to fix when scans are short, the second says
//! how much there is when they are not.
//!
//! cbcore-rs measured 216 µs alone, 26.0 ms behind a fan-out on one connection
//! manager, and 1.1–2.5 ms beside one on two (`docs/connection-tuning.md`). Its
//! treatment p50 moved by more than a factor of two between runs while the
//! fan-out's own median sat still, so this runs several rounds and prints every
//! one.

extern crate core;

use std::env;
use std::ops::Add;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};

use couchbase_core::agent::Agent;
use couchbase_core::features::BucketFeature;
use couchbase_core::memdx::ops_rangescan::{RangeScanCreateRangeScanConfig, RangeScanItemIter};
use couchbase_core::options::crud::{GetOptions, UpsertOptions};
use couchbase_core::options::rangescan::{RangeScanContinueOptions, RangeScanCreateOptions};
use couchbase_core::options::waituntilready::WaitUntilReadyOptions;
use couchbase_core::retrybesteffort::{BestEffortRetryStrategy, ExponentialBackoffCalculator};
use envconfig::Envconfig;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::common::default_agent_options::create_default_options;
use crate::common::test_config::{create_test_config, EnvTestConfig};

mod common;

/// The end of the key space, for a scan bounded only by its prefix.
const KEY_MAX: &[u8] = &[0xff; 16];

const ROUNDS: usize = 3;

/// How many `get`s to time in each row.
///
/// Every row takes the same number, and each row's fan-out is repeated back to
/// back underneath it: one fan-out finishes in tens of milliseconds and a `get`
/// caught behind one can take tens of milliseconds too, so a single fan-out
/// yields a handful of samples and a p90 of nothing.
const GET_SAMPLES: usize = 100;

#[test]
#[ignore = "latency probe: run with --ignored --nocapture and read the rows"]
fn get_latency_alongside_a_range_scan_fan_out() {
    // **Multi-threaded on purpose.** A current-thread runtime serialises the
    // fan-out onto the same worker as the `get`, so it would measure the
    // client's scheduling rather than the server's connection queue.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async move {
        let config = create_test_config(&EnvTestConfig::init_from_env().unwrap()).await;
        let agent = Arc::new(
            Agent::new(create_default_options(config).await)
                .await
                .expect("the test cluster should accept a connection"),
        );
        agent
            .wait_until_ready(&WaitUntilReadyOptions::new())
            .await
            .unwrap();

        if !agent
            .bucket_features()
            .await
            .unwrap()
            .contains(&BucketFeature::RangeScan)
        {
            eprintln!("skipped: the bucket does not support range scan");
            return;
        }

        let docs: usize = env::var("RCBSCANDOCS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        let chunk: u32 = env::var("RCBSCANCHUNK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);

        // The document count is in the prefix, so a run at one size does not
        // read a previous run's documents at another.
        let prefix = Arc::new(format!("rangescan-probe-{docs}-"));
        let keys = seed(&agent, &prefix, docs).await;
        let probe_key = keys[0].clone();
        let num_vbuckets = agent.num_vbuckets().await.unwrap();

        // A scan reads persisted state, so wait until the seed is all readable
        // before timing anything against it.
        let deadline = Instant::now().add(Duration::from_secs(180));
        loop {
            let (_, seen) = fan_out(agent.clone(), prefix.clone(), num_vbuckets, 0).await;
            if seen >= docs {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "only {seen} of {docs} seeded documents became scannable"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        eprintln!(
            "probe: {docs} documents over {num_vbuckets} vbuckets, {GET_SAMPLES} gets per row, \
             chunked row reads {chunk} per continue"
        );

        for round in 1..=ROUNDS {
            let alone = time_gets(&agent, &probe_key, GET_SAMPLES).await;
            let (unbounded, unbounded_fan_outs) =
                gets_during_fan_out(&agent, &probe_key, prefix.clone(), num_vbuckets, 0).await;
            let (chunked, chunked_fan_outs) =
                gets_during_fan_out(&agent, &probe_key, prefix.clone(), num_vbuckets, chunk).await;

            eprintln!("round {round}");
            report("get_alone            ", &alone, None);
            report(
                "get_with_scan        ",
                &unbounded,
                Some(&unbounded_fan_outs),
            );
            report("get_with_chunked_scan", &chunked, Some(&chunked_fan_outs));

            assert!(!alone.is_empty() && !unbounded.is_empty() && !chunked.is_empty());
            assert!(!unbounded_fan_outs.is_empty() && !chunked_fan_outs.is_empty());
        }
    });
}

fn report(label: &str, gets: &[Duration], fan_outs: Option<&[(Duration, usize)]>) {
    let load = match fan_outs {
        Some(runs) => {
            let times: Vec<Duration> = runs.iter().map(|(d, _)| *d).collect();
            format!(
                "   fan-out median {:>10} over {} runs of {} documents",
                fmt(percentile(&times, 50.0)),
                times.len(),
                runs.first().map(|(_, seen)| *seen).unwrap_or(0),
            )
        }
        None => String::new(),
    };

    eprintln!(
        "  {label}  p50 {:>10}  p90 {:>10}  max {:>10}{load}",
        fmt(percentile(gets, 50.0)),
        fmt(percentile(gets, 90.0)),
        fmt(percentile(gets, 100.0)),
    );
}

/// Time `GET_SAMPLES` gets while fan-outs run back to back underneath them.
async fn gets_during_fan_out(
    agent: &Arc<Agent>,
    key: &str,
    prefix: Arc<String>,
    num_vbuckets: usize,
    max_count: u32,
) -> (Vec<Duration>, Vec<(Duration, usize)>) {
    let stop = Arc::new(AtomicBool::new(false));
    let scanning = agent.clone();
    let scanning_stop = stop.clone();
    let handle = tokio::spawn(async move {
        let mut runs = vec![];
        while !scanning_stop.load(Ordering::Relaxed) {
            runs.push(fan_out(scanning.clone(), prefix.clone(), num_vbuckets, max_count).await);
        }
        runs
    });

    // Let the first fan-out get onto the wire before timing anything.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let gets = time_gets(agent, key, GET_SAMPLES).await;
    stop.store(true, Ordering::Relaxed);
    let runs = handle.await.unwrap();

    (gets, runs)
}

/// Seed `docs` documents under the probe's prefix, and return their keys.
///
/// Idempotent: the prefix is fixed for a given size, so a second run overwrites
/// the same documents rather than adding to them.
async fn seed(agent: &Agent, prefix: &str, docs: usize) -> Vec<String> {
    let strategy = Arc::new(BestEffortRetryStrategy::new(
        ExponentialBackoffCalculator::default(),
    ));

    let mut keys = Vec::with_capacity(docs);
    for i in 0..docs {
        let key = format!("{prefix}{i:08}");
        agent
            .upsert(
                UpsertOptions::new(
                    key.as_bytes(),
                    "",
                    "",
                    br#"{"a":1,"pad":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}"#,
                )
                .retry_strategy(strategy.clone()),
            )
            .await
            .unwrap();
        keys.push(key);
    }

    keys
}

/// One whole-collection scan: every vbucket at once, each drained to completion.
///
/// Concurrent rather than sequential because that is how a caller reads a
/// collection, and because a sequential fan-out never puts more than one scan on
/// a connection — which is the entire thing being measured.
async fn fan_out(
    agent: Arc<Agent>,
    prefix: Arc<String>,
    num_vbuckets: usize,
    max_count: u32,
) -> (Duration, usize) {
    let started = StdInstant::now();
    let mut end_bytes = prefix.as_bytes().to_vec();
    end_bytes.extend_from_slice(KEY_MAX);
    let end = Arc::new(end_bytes);

    let mut set = JoinSet::new();
    for vb in 0..num_vbuckets as u16 {
        let agent = agent.clone();
        let end = end.clone();
        let prefix = prefix.clone();
        set.spawn(async move {
            let Ok(scan) = agent
                .range_scan_create(RangeScanCreateOptions::new("", "", vb).range(
                    RangeScanCreateRangeScanConfig {
                        start: Some(prefix.as_bytes()),
                        end: Some(&end),
                        exclusive_start: None,
                        exclusive_end: None,
                    },
                ))
                .await
            else {
                // Nothing in range on this vbucket: not a scan, and not a
                // failure.
                return 0usize;
            };

            let opts = RangeScanContinueOptions::new().max_count(max_count);
            let mut seen = 0usize;
            loop {
                let Ok(res) = scan
                    .continue_scan(&opts, |resp| {
                        seen += match resp.items {
                            RangeScanItemIter::Full(it) => it.count(),
                            RangeScanItemIter::KeyOnly(it) => it.count(),
                        };
                    })
                    .await
                else {
                    break;
                };
                if res.complete {
                    break;
                }
            }
            seen
        });
    }

    let mut total = 0;
    while let Some(res) = set.join_next().await {
        total += res.unwrap_or(0);
    }

    (started.elapsed(), total)
}

/// Time `samples` gets, one at a time.
async fn time_gets(agent: &Agent, key: &str, samples: usize) -> Vec<Duration> {
    let strategy = Arc::new(BestEffortRetryStrategy::new(
        ExponentialBackoffCalculator::default(),
    ));
    let mut out = Vec::with_capacity(samples);

    for _ in 0..samples {
        let started = StdInstant::now();
        agent
            .get(GetOptions::new(key.as_bytes(), "", "").retry_strategy(strategy.clone()))
            .await
            .expect("the probe document should be readable");
        out.push(started.elapsed());
    }

    out
}

fn percentile(samples: &[Duration], p: f64) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx]
}

fn fmt(d: Duration) -> String {
    if d < Duration::from_millis(1) {
        format!("{} us", d.as_micros())
    } else {
        format!("{:.2} ms", d.as_secs_f64() * 1000.0)
    }
}
