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

//! What every KV operation says about a document that is not there.
//!
//! Ported from cbcore-rs `tests/integration.rs::test_agent_kv_key_not_found_errors`.
//! The crate's KV coverage was all happy paths plus two retry-until-deadline
//! cases; no test asserted a not-found error kind on any operation, and
//! `get_meta` was never called at all. One operation quietly reporting
//! something else — success, a different status, a retry that never ends — is
//! the failure this catches, and it is per-operation, so the test is too.
//!
//! Fail-fast rather than best-effort throughout: a not-found is not a retry
//! reason, so the strategy does not mask it, and a mistake that made it one
//! would show up here as a hang rather than as a pass.

use crate::common::helpers::{generate_key, is_memdx_error};
use crate::common::test_config::run_test;
use couchbase_core::memdx::error::{ErrorKind, ServerErrorKind};
use couchbase_core::memdx::subdoc::{LookupInOp, LookupInOpType, MutateInOp, MutateInOpType};
use couchbase_core::options::crud::{
    AppendOptions, DecrementOptions, DeleteOptions, GetAndLockOptions, GetAndTouchOptions,
    GetMetaOptions, GetOptions, IncrementOptions, LookupInOptions, MutateInOptions, PrependOptions,
    ReplaceOptions, TouchOptions, UnlockOptions,
};
use couchbase_core::retryfailfast::FailFastRetryStrategy;
use std::sync::Arc;

mod common;

/// Assert that one operation refused a missing key with the status named,
/// naming the operation too so a failure says which one.
fn assert_missing_key<T: std::fmt::Debug>(
    op: &str,
    expected: ServerErrorKind,
    res: Result<T, couchbase_core::error::Error>,
) {
    let err = match res {
        Ok(v) => panic!("{op} succeeded against a key that does not exist: {v:?}"),
        Err(e) => e,
    };

    let memdx = is_memdx_error(&err)
        .unwrap_or_else(|| panic!("{op} did not fail with a memdx error: {err:?}"));

    match memdx.kind() {
        ErrorKind::Server(e) => assert_eq!(
            &expected,
            e.kind(),
            "{op} reported the wrong status for a missing key"
        ),
        other => panic!("{op} did not fail with a server error: {other:?}"),
    }
}

/// Every operation but two.
fn assert_key_not_found<T: std::fmt::Debug>(
    op: &str,
    res: Result<T, couchbase_core::error::Error>,
) {
    assert_missing_key(op, ServerErrorKind::KeyNotFound, res)
}

#[test]
fn every_operation_reports_a_missing_key_as_a_missing_key() {
    run_test(async |mut agent| {
        let strat = Arc::new(FailFastRetryStrategy::default());
        let key = generate_key();
        let key = key.as_slice();

        assert_key_not_found(
            "get",
            agent
                .get(GetOptions::new(key, "", "").retry_strategy(strat.clone()))
                .await,
        );

        assert_key_not_found(
            "get_meta",
            agent
                .get_meta(GetMetaOptions::new(key, "", "").retry_strategy(strat.clone()))
                .await,
        );

        assert_key_not_found(
            "get_and_lock",
            agent
                .get_and_lock(GetAndLockOptions::new(key, "", "", 10).retry_strategy(strat.clone()))
                .await,
        );

        assert_key_not_found(
            "get_and_touch",
            agent
                .get_and_touch(
                    GetAndTouchOptions::new(key, "", "", 10).retry_strategy(strat.clone()),
                )
                .await,
        );

        assert_key_not_found(
            "unlock",
            agent
                .unlock(UnlockOptions::new(key, "", "", 1).retry_strategy(strat.clone()))
                .await,
        );

        assert_key_not_found(
            "touch",
            agent
                .touch(TouchOptions::new(key, "", "", 10).retry_strategy(strat.clone()))
                .await,
        );

        assert_key_not_found(
            "delete",
            agent
                .delete(DeleteOptions::new(key, "", "").retry_strategy(strat.clone()))
                .await,
        );

        assert_key_not_found(
            "replace",
            agent
                .replace(ReplaceOptions::new(key, "", "", b"v").retry_strategy(strat.clone()))
                .await,
        );

        // **Append and prepend are the two that differ**, and they are the
        // reason this is written per operation rather than as one loop. KV
        // answers them with NOT_STORED (0x05) rather than KEY_ENOENT (0x01) —
        // measured against 8.0.3, and the same in the protocol spec. A caller
        // matching only on KeyNotFound will miss a missing document on exactly
        // these two.
        assert_missing_key(
            "append",
            ServerErrorKind::NotStored,
            agent
                .append(AppendOptions::new(key, "", "", b"v").retry_strategy(strat.clone()))
                .await,
        );

        assert_missing_key(
            "prepend",
            ServerErrorKind::NotStored,
            agent
                .prepend(PrependOptions::new(key, "", "", b"v").retry_strategy(strat.clone()))
                .await,
        );

        // **Without an initial value.** With one the server creates the
        // counter, which is a different operation and pins nothing here.
        assert_key_not_found(
            "increment",
            agent
                .increment(IncrementOptions::new(key, 1, "", "").retry_strategy(strat.clone()))
                .await,
        );

        assert_key_not_found(
            "decrement",
            agent
                .decrement(DecrementOptions::new(key, 1, "", "").retry_strategy(strat.clone()))
                .await,
        );

        let lookup_ops = [LookupInOp::new(LookupInOpType::Get, b"field")];
        assert_key_not_found(
            "lookup_in",
            agent
                .lookup_in(
                    LookupInOptions::new(key, "", "", &lookup_ops).retry_strategy(strat.clone()),
                )
                .await,
        );

        let mutate_ops = [MutateInOp::new(MutateInOpType::DictSet, b"field", b"1")];
        assert_key_not_found(
            "mutate_in",
            agent
                .mutate_in(
                    MutateInOptions::new(key, "", "", &mutate_ops).retry_strategy(strat.clone()),
                )
                .await,
        );
    });
}
