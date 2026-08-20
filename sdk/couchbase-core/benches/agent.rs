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

// Each bench binary is its own crate, so it needs the raised limit the library carries: the
// KV futures are no longer boxed, and driving an operation through the agent builds a state
// machine deeper than rustc's default layout query depth.
#![recursion_limit = "256"]

//! One operation's latency through the whole agent, against a live cluster.
//!
//! Carried from cbcore-rs — `benches/agent.rs` at commit `dbf1c0b`
//! (2026-08-17), carried on 2026-08-18 — and adapted: criterion rather than
//! divan, and the cluster comes from `tests/common/test_config.rs` rather than
//! from this file's own hardcoded address, so a bench and a test always agree
//! about which cluster they mean.
//!
//! **This needs a reachable cluster.** `RCBCONNSTR`, `RCBUSERNAME`,
//! `RCBPASSWORD` and `RCBBUCKET` all have compiled-in defaults, so an ordinary
//! run against the development cluster needs no environment at all — and an
//! unreachable one fails at agent setup rather than reporting a number.
//!
//! ```text
//! cargo bench -p couchbase-core --bench agent
//! ```
//!
//! # What this is for
//!
//! `benches/query_rows.rs` measures the row path with the network taken out and
//! `tests/allocations.rs` counts allocations; neither says what an operation
//! costs in time end to end, and nothing else here does either.
//!
//! `get` is also the **control** the connection-tuning study wants. That study
//! (`docs/connection-tuning.md`) turns on the latency of one `get` measured
//! against what else is running, and its floor — a `get` with an idle
//! connection — is what this entry measures. It is only useful measured in the
//! same session as whatever it is being compared against, because it moves with
//! the cluster's mood: the study's control read 216 µs on a 3-node 8.0.3
//! cluster and its own tables quote a second control of 0.46 ms from a different
//! session. Do not quote a control from another day.
//!
//! # What it deliberately does not measure
//!
//! Connecting. `Agent::new` bootstraps and authenticates, which is one to two
//! orders of magnitude dearer than any operation below, so the agent is built
//! once outside every timed closure.

use std::time::Duration;

use couchbase_core::options::crud::{GetOptions, UpsertOptions};
use couchbase_core::options::query::QueryOptions;
use couchbase_core::options::waituntilready::WaitUntilReadyOptions;
use couchbase_core::retryfailfast::FailFastRetryStrategy;
use couchbase_core::service_type::ServiceType;
use criterion::{criterion_group, criterion_main, Criterion};
use futures::StreamExt;
use std::sync::Arc;
use tokio::runtime::Runtime;

#[path = "../tests/common/mod.rs"]
mod common;

use common::helpers::{generate_bytes_value, generate_key};
use common::test_agent::TestAgent;
use common::test_config::create_test_agent;

/// A 32-byte body, matching what `tests/allocations.rs` writes, so the two
/// measurements are of the same operation.
const VALUE_BYTES: usize = 32;

/// The agent, connected once.
///
/// Built on the same runtime the benchmarks run on, because a `tokio` handle
/// captured on one runtime cannot be driven by another — the agent spawns a
/// config watcher and a read loop per connection at construction.
fn setup(rt: &Runtime) -> (TestAgent, Vec<u8>, Vec<u8>) {
    rt.block_on(async {
        let agent = create_test_agent().await;
        agent
            .wait_until_ready(&WaitUntilReadyOptions::new().service_types(vec![ServiceType::MEMD]))
            .await
            .unwrap();

        let key = generate_key();
        let value = generate_bytes_value(VALUE_BYTES);

        // Seed the document `get` reads, and warm the collection resolver and
        // the pool so the first timed iteration is not measuring setup.
        agent
            .upsert(
                UpsertOptions::new(key.as_slice(), "", "", value.as_slice())
                    .retry_strategy(Arc::new(FailFastRetryStrategy::default())),
            )
            .await
            .unwrap();

        (agent, key, value)
    })
}

fn kv(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let (agent, key, value) = setup(&rt);

    let get_opts = GetOptions::new(key.as_slice(), "", "")
        .retry_strategy(Arc::new(FailFastRetryStrategy::default()));
    let upsert_opts = UpsertOptions::new(key.as_slice(), "", "", value.as_slice())
        .retry_strategy(Arc::new(FailFastRetryStrategy::default()));

    let mut group = c.benchmark_group("kv");
    group.bench_function("get", |b| {
        b.to_async(&rt)
            .iter(|| async { agent.get(get_opts.clone()).await.unwrap() })
    });
    group.bench_function("upsert", |b| {
        b.to_async(&rt)
            .iter(|| async { agent.upsert(upsert_opts.clone()).await.unwrap() })
    });
    group.finish();
}

fn query(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let agent = rt.block_on(async {
        let agent = create_test_agent().await;
        agent
            .wait_until_ready(&WaitUntilReadyOptions::new().service_types(vec![ServiceType::QUERY]))
            .await
            .unwrap();
        agent
    });

    // The same statement `tests/allocations.rs` pins, so the time and the
    // allocation count describe one operation rather than two.
    let opts = QueryOptions::default()
        .statement("SELECT 1=1".to_string())
        .retry_strategy(Arc::new(FailFastRetryStrategy::default()));

    // Drained to the metadata, not just to the first row: a query that is not
    // read to the end leaves its connection mid-response, and the next
    // iteration would pay for that rather than for the query.
    c.bench_function("query", |b| {
        b.to_async(&rt).iter(|| async {
            let mut res = agent.query(opts.clone()).await.unwrap();
            while let Some(row) = res.next().await {
                row.unwrap();
            }
            res.metadata().unwrap();
        })
    });
}

criterion_group!(
    name = benches;
    // Every sample here is a network round trip against a shared cluster, so
    // the tail is heavy and the median moves between sessions. A hundred
    // samples over ten seconds is enough to see a change of the size this is
    // useful for, and short enough that the control can be re-run beside it.
    config = Criterion::default()
        .sample_size(100)
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(10));
    targets = kv, query
);
criterion_main!(benches);
