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

//! Whether `Agent::vbuuid_map` actually attributes every vbucket to a live
//! node's UUID, which nothing in `agent_ops.rs`'s own unit tests can check:
//! `dispatch_to_vbucket` and `resolve_canonical_addr` there are exercised
//! against a fake router, never against a real `stats vbucket-seqno` sweep
//! and a real endpoint-id-to-address mapping. `require_complete` already
//! refuses a short map with an error rather than returning one -- so the
//! risk this guards is `Ok` with a hole an error-only unit test cannot
//! produce, not a length mismatch a healthy cluster would ever let through.
//!
//! One `#[test]`, not several: the caching and invalidation checks below are
//! only meaningful read against the completeness check's own map, and
//! splitting them into separate tests would just make three fns race each
//! other over the one cache an `Agent` holds.
//!
//! # Why this is a plain `#[test]`, not `#[ignore]`
//!
//! Every file under `tests/` is its own binary, and `cargo test -p
//! couchbase-core --lib` never builds or runs any of them -- that is the
//! crate's actual gate against a cluster-less run, not an attribute on the
//! test itself. `#[ignore]` here is reserved for a probe that is not
//! pass/fail (see `rangescan_latency.rs`); this is an ordinary correctness
//! test, so it follows the same plain-`#[test]` convention as every other
//! file in this directory.
//!
//! ```bash
//! RCBCONNSTR=couchbase://192.168.107.128 RCBUSERNAME=Administrator \
//!   RCBPASSWORD=password RCBBUCKET=default \
//!   cargo test -p couchbase-core --test vbuuid_map -- --nocapture
//! ```

use crate::common::test_config::run_test;
use std::sync::Arc;

mod common;

#[test]
fn every_vbucket_gets_a_live_uuid() {
    run_test(async |agent| {
        let n = agent
            .num_vbuckets()
            .await
            .expect("a bucket-bound agent knows its own vbucket count");

        // The call under test. A short or empty result would surface here as
        // `Err` -- `vbucket_vbuuids`'s `require_complete` refuses anything
        // less than every vbucket rather than returning a partial map -- so
        // reaching the asserts below already proves completeness. What it
        // does not prove, and what the asserts below exist to check, is that
        // the values are the real UUIDs a live failover log would produce
        // rather than the sentinel "not reported" value of 0.
        let map = agent
            .vbuuid_map()
            .await
            .expect("a healthy, unpartitioned cluster should never refuse a full map");

        println!("num_vbuckets = {n}, vbuuid_map.len() = {}", map.len());
        for (vb, uuid) in map.iter().take(5) {
            println!("  vb_{vb}:uuid = {uuid}");
        }

        // Belt-and-suspenders on top of `require_complete`: assert the
        // invariant against `num_vbuckets()` itself, not a hardcoded 1024 or
        // 64, because this suite runs against whichever the cluster under
        // test actually has.
        assert_eq!(
            map.len(),
            n,
            "vbuuid_map came back {} long against {n} vbuckets",
            map.len()
        );

        let zero: Vec<u16> = map
            .iter()
            .filter(|(_, uuid)| **uuid == 0)
            .map(|(vb, _)| *vb)
            .collect();
        assert!(
            zero.is_empty(),
            "vbucket(s) {zero:?} reported uuid 0 (\"not reported\") on a bucket this test \
             requires to be membase/couchstore and therefore vbucket-seqno-capable"
        );

        // Caching: nothing between these two calls can move the config
        // revision or trip the 5s TTL, so the second call must be served
        // from the cache rather than re-running the stats sweep.
        let cached = agent
            .vbuuid_map()
            .await
            .expect("the map just proven complete should not become unreadable a moment later");
        assert!(
            Arc::ptr_eq(&map, &cached),
            "a second vbuuid_map() with nothing to invalidate it re-fetched instead of caching"
        );

        // Invalidation: reporting the cached map as stale must drop exactly
        // that map (see `VbUuidCache::invalidate`'s identity check), so the
        // next read is a fresh `Arc` -- but the cluster's UUIDs have not
        // actually changed underneath this test, so the contents must still
        // match the map just invalidated.
        agent.invalidate_vbuuids(&cached);
        let refreshed = agent
            .vbuuid_map()
            .await
            .expect("invalidating a map must not make the next read fail");
        assert!(
            !Arc::ptr_eq(&cached, &refreshed),
            "invalidate_vbuuids() left the stale map being served"
        );
        assert_eq!(
            *cached, *refreshed,
            "a refetch immediately after invalidation should see the same UUIDs, \
             not a topology change this test never caused"
        );
    });
}
