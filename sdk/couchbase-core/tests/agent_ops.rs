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

extern crate core;

use crate::common::default_agent_options::{create_default_options, create_options_without_bucket};
use crate::common::helpers::try_until;
use crate::common::helpers::{
    create_collection_and_wait_for_kv, delete_collection_and_wait_for_kv, feature_supported,
    generate_bytes_value, generate_key, generate_string_key, is_memdx_error,
};
use crate::common::test_agent::TestAgent;
use crate::common::test_config::{run_test, setup_test};
use couchbase_core::agent::Agent;
use couchbase_core::features::BucketFeature;
use couchbase_core::memdx::durability_level::DurabilityLevel;
use couchbase_core::memdx::error::{ErrorKind, ServerErrorKind, SubdocErrorKind};
use couchbase_core::memdx::ops_rangescan::{RangeScanCreateRangeScanConfig, RangeScanItemIter};
use couchbase_core::memdx::subdoc::{LookupInOp, LookupInOpType, MutateInOp, MutateInOpType};
use couchbase_core::options::agent::KvConfig;
use couchbase_core::options::crud::{
    AddOptions, AppendOptions, DecrementOptions, DeleteOptions, GetAndLockOptions,
    GetAndTouchOptions, GetOptions, IncrementOptions, LookupInOptions, MutateInOptions,
    PrependOptions, ReplaceOptions, TouchOptions, UnlockOptions, UpsertOptions,
};
use couchbase_core::options::rangescan::{
    RangeScanCancelOptions, RangeScanContinueOptions, RangeScanCreateOptions,
};
use couchbase_core::options::stats::{StatsByVbucketOptions, StatsOptions};
use couchbase_core::options::waituntilready::WaitUntilReadyOptions;
use couchbase_core::retrybesteffort::{BestEffortRetryStrategy, ExponentialBackoffCalculator};
use couchbase_core::retryfailfast::FailFastRetryStrategy;
use couchbase_core::service_type::ServiceType;
use rand::distr::Alphanumeric;
use rand::{rng, Rng, RngExt};
use serde::Serialize;
use std::ops::{Add, Deref};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{timeout_at, Instant};

mod common;

#[test]
fn test_upsert_and_get() {
    run_test(async |mut agent| {
        let strat = Arc::new(BestEffortRetryStrategy::new(
            ExponentialBackoffCalculator::default(),
        ));

        let key = generate_key();
        let value = generate_bytes_value(32);

        let upsert_opts = UpsertOptions::new(key.as_slice(), "", "", value.as_slice())
            .retry_strategy(strat.clone());

        let upsert_result = agent.upsert(upsert_opts).await.unwrap();

        assert_ne!(0, upsert_result.cas);
        assert!(upsert_result.mutation_token.is_some());

        let get_result = agent
            .get(GetOptions::new(&key, "", "").retry_strategy(strat))
            .await
            .unwrap();

        assert_eq!(value, get_result.value);
        assert_eq!(upsert_result.cas, get_result.cas);
    });
}

#[test]
fn test_upsert_durability_level_majority() {
    run_test(async |mut agent| {
        let strat = Arc::new(BestEffortRetryStrategy::new(
            ExponentialBackoffCalculator::default(),
        ));

        let key = generate_key();
        let value = generate_bytes_value(32);

        let upsert_opts = UpsertOptions::new(key.as_slice(), "", "", value.as_slice())
            .durability_level(DurabilityLevel::MAJORITY)
            .retry_strategy(strat.clone());

        let upsert_result = agent.upsert(upsert_opts).await.unwrap();

        assert_ne!(0, upsert_result.cas);
        assert!(upsert_result.mutation_token.is_some());
    });
}

#[test]
fn test_upsert_retry_locked_until_deadline() {
    run_test(async |mut agent| {
        let strat = Arc::new(BestEffortRetryStrategy::new(
            ExponentialBackoffCalculator::default(),
        ));

        let key = generate_key();
        let value = generate_bytes_value(32);

        let upsert_opts = UpsertOptions::new(key.as_slice(), "", "", value.as_slice())
            .retry_strategy(strat.clone());

        let upsert_result = agent.upsert(upsert_opts.clone()).await.unwrap();

        assert_ne!(0, upsert_result.cas);
        assert!(upsert_result.mutation_token.is_some());

        let _ = agent
            .get_and_lock(
                GetAndLockOptions::new(key.as_slice(), "", "", 10).retry_strategy(strat.clone()),
            )
            .await
            .unwrap();

        let res = timeout_at(
            Instant::now().add(Duration::from_secs(1)),
            agent.deref().upsert(upsert_opts.clone()),
        )
        .await;

        assert!(res.is_err(), "Expected timeout error, got {res:?}");
    });
}

#[test]
fn test_add_and_delete() {
    run_test(async |mut agent| {
        let strat = Arc::new(BestEffortRetryStrategy::new(
            ExponentialBackoffCalculator::default(),
        ));

        let key = generate_key();
        let value = generate_bytes_value(32);

        let add_opts =
            AddOptions::new(key.as_slice(), "", "", value.as_slice()).retry_strategy(strat.clone());

        let add_result = agent.add(add_opts.clone()).await.unwrap();

        assert_ne!(0, add_result.cas);
        assert!(add_result.mutation_token.is_some());

        let add_result = agent.add(add_opts.clone()).await;

        assert!(is_memdx_error(&add_result.err().unwrap())
            .unwrap()
            .is_server_error_kind(ServerErrorKind::KeyExists));

        let delete_result = agent
            .delete(DeleteOptions::new(&key, "", "").retry_strategy(strat))
            .await
            .unwrap();

        assert_ne!(0, delete_result.cas);
        assert!(delete_result.mutation_token.is_some());

        let add_result = agent.add(add_opts).await.unwrap();

        assert_ne!(0, add_result.cas);
        assert!(add_result.mutation_token.is_some());
    });
}

#[test]
fn test_replace() {
    run_test(async |mut agent| {
        let strat = Arc::new(BestEffortRetryStrategy::new(
            ExponentialBackoffCalculator::default(),
        ));

        let key = generate_key();
        let value = generate_bytes_value(32);

        let upsert_opts = UpsertOptions::new(key.as_slice(), "", "", value.as_slice())
            .retry_strategy(strat.clone());

        let upsert_result = agent.upsert(upsert_opts).await.unwrap();

        assert_ne!(0, upsert_result.cas);
        assert!(upsert_result.mutation_token.is_some());

        let new_value = generate_bytes_value(32);

        let replace_result = agent
            .replace(
                ReplaceOptions::new(&key, "", "", new_value.as_slice())
                    .retry_strategy(strat.clone()),
            )
            .await
            .unwrap();

        assert_ne!(0, replace_result.cas);
        assert!(replace_result.mutation_token.is_some());

        let get_result = agent
            .get(GetOptions::new(&key, "", "").retry_strategy(strat.clone()))
            .await
            .unwrap();

        assert_eq!(new_value, get_result.value);
        assert_eq!(replace_result.cas, get_result.cas);
    });
}

#[test]
fn test_lock_unlock() {
    run_test(async |mut agent| {
        let strat = Arc::new(BestEffortRetryStrategy::new(
            ExponentialBackoffCalculator::default(),
        ));

        let key = generate_key();
        let value = generate_bytes_value(32);

        let upsert_opts = UpsertOptions::new(key.as_slice(), "", "", value.as_slice())
            .retry_strategy(strat.clone());

        let upsert_result = agent.upsert(upsert_opts).await.unwrap();

        assert_ne!(0, upsert_result.cas);
        assert!(upsert_result.mutation_token.is_some());

        let get_result = agent
            .get(GetOptions::new(&key, "", "").retry_strategy(strat.clone()))
            .await
            .unwrap();

        let cas = get_result.cas;

        let unlock_result = agent
            .unlock(
                UnlockOptions::new(key.as_slice(), "", "", cas)
                    .retry_strategy(Arc::new(FailFastRetryStrategy::default())),
            )
            .await
            .err()
            .unwrap();

        let memdx_err = is_memdx_error(&unlock_result).unwrap();
        assert!(
            memdx_err.is_server_error_kind(ServerErrorKind::NotLocked)
                || memdx_err.is_server_error_kind(ServerErrorKind::TmpFail)
        );

        let get_and_lock_result = agent
            .get_and_lock(
                GetAndLockOptions::new(key.as_slice(), "", "", 10).retry_strategy(strat.clone()),
            )
            .await
            .unwrap();

        let cas = get_and_lock_result.cas;
        assert_eq!(value, get_and_lock_result.value);

        let unlock_result = agent
            .unlock(UnlockOptions::new(key.as_slice(), "", "", cas).retry_strategy(strat.clone()))
            .await;

        assert!(unlock_result.is_ok());
    });
}

#[test]
fn test_touch_operations() {
    // TODO RSCBC-27 we can't fetch & check the expiry without subdoc
    run_test(async |mut agent| {
        let strat = Arc::new(BestEffortRetryStrategy::new(
            ExponentialBackoffCalculator::default(),
        ));

        let key = generate_key();
        let value = generate_bytes_value(32);

        let upsert_opts = UpsertOptions::new(key.as_slice(), "", "", value.as_slice())
            .retry_strategy(strat.clone())
            .expiry(10);

        let upsert_result = agent.upsert(upsert_opts).await.unwrap();

        assert_ne!(0, upsert_result.cas);
        assert!(upsert_result.mutation_token.is_some());

        let touch_result = agent
            .touch(TouchOptions::new(key.as_slice(), "", "", 12).retry_strategy(strat.clone()))
            .await
            .unwrap();

        assert_ne!(0, touch_result.cas);

        let get_and_touch_result = agent
            .get_and_touch(
                GetAndTouchOptions::new(key.as_slice(), "", "", 15).retry_strategy(strat.clone()),
            )
            .await
            .unwrap();

        assert_eq!(value, get_and_touch_result.value);
        assert_ne!(0, get_and_touch_result.cas);
    });
}

#[test]
fn test_append_and_prepend() {
    run_test(async |mut agent| {
        let strat = Arc::new(BestEffortRetryStrategy::new(
            ExponentialBackoffCalculator::default(),
        ));

        let key = generate_key();
        let value = "answer is".as_bytes().to_vec();

        let upsert_result = agent
            .upsert(
                UpsertOptions::new(key.as_slice(), "", "", value.as_slice())
                    .retry_strategy(strat.clone()),
            )
            .await
            .unwrap();

        assert_ne!(0, upsert_result.cas);
        assert!(upsert_result.mutation_token.is_some());

        let value = "the ".as_bytes();

        let prepend_result = agent
            .prepend(PrependOptions::new(&key, "", "", value).retry_strategy(strat.clone()))
            .await
            .unwrap();

        assert_ne!(0, prepend_result.cas);
        assert!(prepend_result.mutation_token.is_some());

        let value = " 42".as_bytes();

        let append_result = agent
            .append(AppendOptions::new(&key, "", "", value).retry_strategy(strat.clone()))
            .await
            .unwrap();

        assert_ne!(0, append_result.cas);
        assert!(append_result.mutation_token.is_some());

        let get_result = agent
            .get(GetOptions::new(&key, "", "").retry_strategy(strat.clone()))
            .await
            .unwrap();

        assert_eq!("the answer is 42".as_bytes(), get_result.value.as_slice());
        assert_eq!(append_result.cas, get_result.cas);
    })
}

#[test]
fn test_append_and_prepend_cas_mismatch() {
    run_test(async |mut agent| {
        let strat = Arc::new(BestEffortRetryStrategy::new(
            ExponentialBackoffCalculator::default(),
        ));

        let key = generate_key();
        let value = "answer is".as_bytes().to_vec();

        let upsert_result = agent
            .upsert(
                UpsertOptions::new(key.as_slice(), "", "", value.as_slice())
                    .retry_strategy(strat.clone()),
            )
            .await
            .unwrap();

        assert_ne!(0, upsert_result.cas);
        assert!(upsert_result.mutation_token.is_some());

        let value = "the ".as_bytes();

        let prepend_result = agent
            .prepend(
                PrependOptions::new(&key, "", "", value)
                    .retry_strategy(strat.clone())
                    .cas(1234),
            )
            .await;

        assert!(prepend_result.is_err());
        let e = prepend_result.err().unwrap();
        let e = is_memdx_error(&e).unwrap();
        assert!(e.is_server_error_kind(ServerErrorKind::CasMismatch));

        let value = " 42".as_bytes();

        let append_result = agent
            .append(
                AppendOptions::new(&key, "", "", value)
                    .retry_strategy(strat.clone())
                    .cas(1234),
            )
            .await;

        assert!(append_result.is_err());
        let e = append_result.err().unwrap();
        let e = is_memdx_error(&e).unwrap();
        assert!(e.is_server_error_kind(ServerErrorKind::CasMismatch));
    });
}

#[test]
fn test_increment_and_decrement() {
    run_test(async |mut agent| {
        let strat = Arc::new(BestEffortRetryStrategy::new(
            ExponentialBackoffCalculator::default(),
        ));

        let key = generate_key();

        let increment_result = agent
            .increment(
                IncrementOptions::new(key.as_slice(), 1, "", "")
                    .retry_strategy(strat.clone())
                    .initial(42),
            )
            .await
            .unwrap();

        assert_ne!(0, increment_result.cas);
        assert_eq!(increment_result.value, 42);
        assert!(increment_result.mutation_token.is_some());

        let decrement_result = agent
            .decrement(
                DecrementOptions::new(key.as_slice(), 2, "", "").retry_strategy(strat.clone()),
            )
            .await
            .unwrap();

        assert_ne!(0, decrement_result.cas);
        assert_eq!(decrement_result.value, 40);
        assert!(decrement_result.mutation_token.is_some());
    });
}

#[derive(Serialize)]
struct SubdocObject {
    foo: u32,
    bar: u32,
    baz: String,
    arr: Vec<u32>,
}

#[test]
fn test_lookup_in() {
    run_test(async |mut agent| {
        let strat = Arc::new(BestEffortRetryStrategy::new(
            ExponentialBackoffCalculator::default(),
        ));

        let key = generate_key();

        let obj = SubdocObject {
            foo: 14,
            bar: 2,
            baz: "hello".to_string(),
            arr: vec![1, 2, 3],
        };

        let value = serde_json::to_vec(&obj).unwrap();

        let upsert_opts = UpsertOptions::new(key.as_slice(), "", "", value.as_slice())
            .retry_strategy(strat.clone());

        let upsert_result = agent.upsert(upsert_opts).await.unwrap();

        assert_ne!(0, upsert_result.cas);
        assert!(upsert_result.mutation_token.is_some());

        let ops = [
            LookupInOp::new(LookupInOpType::Get, "baz".as_bytes()),
            LookupInOp::new(LookupInOpType::Exists, "not-exists".as_bytes()),
            LookupInOp::new(LookupInOpType::GetCount, "arr".as_bytes()),
            LookupInOp::new(LookupInOpType::GetDoc, "".as_bytes()),
        ];

        let lookup_in_opts =
            LookupInOptions::new(key.as_slice(), "", "", &ops).retry_strategy(strat.clone());

        let lookup_in_result = agent.lookup_in(lookup_in_opts).await.unwrap();

        assert_eq!(4, lookup_in_result.value.len());
        assert_ne!(0, lookup_in_result.cas);
        assert!(!lookup_in_result.doc_is_deleted);
        assert!(lookup_in_result.value[0].err.is_none());
        assert_eq!(
            std::str::from_utf8(lookup_in_result.value[0].value.as_ref().unwrap())
                .unwrap()
                .trim_matches('"'),
            "hello"
        );
        assert!(lookup_in_result.value[0].err.is_none());

        let kind = lookup_in_result.value[1].err.as_ref().unwrap().kind();
        match kind {
            ErrorKind::Server(e) => match e.kind() {
                ServerErrorKind::Subdoc { error, .. } => {
                    assert!(error.is_error_kind(SubdocErrorKind::PathNotFound));
                    assert_eq!(1, error.op_index().unwrap());
                }
                _ => panic!("Expected subdoc error, got {:?}", e.kind()),
            },
            _ => panic!("Expected server error, got {kind:?}"),
        }

        assert!(lookup_in_result.value[2].err.is_none());
        assert_eq!(
            std::str::from_utf8(lookup_in_result.value[2].value.as_ref().unwrap())
                .unwrap()
                .trim_matches('"'),
            "3"
        );
        assert!(lookup_in_result.value[3].err.is_none());
        assert_eq!(
            lookup_in_result.value[3].value.as_ref().unwrap(),
            &serde_json::to_vec(&obj).unwrap()
        );
    });
}

#[test]
fn test_mutate_in() {
    run_test(async |mut agent| {
        let strat = Arc::new(BestEffortRetryStrategy::new(
            ExponentialBackoffCalculator::default(),
        ));

        let key = generate_key();

        let obj = SubdocObject {
            foo: 14,
            bar: 2,
            baz: "hello".to_string(),
            arr: vec![1, 2, 3],
        };

        let value = serde_json::to_vec(&obj).unwrap();

        let upsert_opts = UpsertOptions::new(key.as_slice(), "", "", value.as_slice())
            .retry_strategy(strat.clone());

        let upsert_result = agent.upsert(upsert_opts).await.unwrap();

        assert_ne!(0, upsert_result.cas);
        assert!(upsert_result.mutation_token.is_some());

        let ops = [
            MutateInOp::new(MutateInOpType::Counter, "bar".as_bytes(), "3".as_bytes()),
            MutateInOp::new(
                MutateInOpType::DictSet,
                "baz".as_bytes(),
                "\"world\"".as_bytes(),
            ),
            MutateInOp::new(
                MutateInOpType::ArrayPushLast,
                "arr".as_bytes(),
                "4".as_bytes(),
            ),
        ];

        let mutate_in_options = MutateInOptions::new(key.as_slice(), "", "", &ops)
            .retry_strategy(strat.clone())
            .expiry(10);

        let mutate_in_result = agent.mutate_in(mutate_in_options).await.unwrap();

        assert_eq!(mutate_in_result.value.len(), 3);
        assert!(mutate_in_result.value[0].err.is_none());
        assert!(mutate_in_result.value[0]
            .value
            .as_ref()
            .is_some_and(|val| String::from_utf8(val.clone()).unwrap() == "5"));
        assert!(mutate_in_result.value[1].err.is_none());
        assert!(mutate_in_result.value[1].value.is_none());
        assert!(mutate_in_result.value[2].err.is_none());
        assert!(mutate_in_result.value[2].value.is_none());
    });
}

#[test]
fn test_kv_without_a_bucket() {
    setup_test(async |config| {
        let agent_opts = create_options_without_bucket(config).await;

        let agent = Agent::new(agent_opts).await.unwrap();

        let strat = Arc::new(BestEffortRetryStrategy::new(
            ExponentialBackoffCalculator::default(),
        ));

        let key = generate_key();
        let value = generate_bytes_value(32);

        let upsert_result = agent
            .upsert(
                UpsertOptions::new(key.as_slice(), "", "", value.as_slice())
                    .retry_strategy(strat.clone()),
            )
            .await;

        assert!(upsert_result.is_err());
        let err = upsert_result.err().unwrap();
        assert_eq!(&couchbase_core::error::ErrorKind::NoBucket, err.kind());
    });
}

#[test]
fn test_unknown_collection_id() {
    setup_test(async |config| {
        let agent_opts = create_default_options(config.clone()).await;
        let bucket = config.bucket;
        let scope_name = config.scope;
        let collection_name = rng()
            .sample_iter(&Alphanumeric)
            .take(10)
            .map(char::from)
            .collect::<String>();

        let mut agent = Agent::new(agent_opts).await.unwrap();

        let strat = Arc::new(FailFastRetryStrategy::default());

        let key = generate_key();
        let value = generate_bytes_value(32);

        create_collection_and_wait_for_kv(
            &agent,
            &bucket,
            &scope_name,
            &collection_name,
            Instant::now().add(Duration::from_secs(10)),
        )
        .await;

        // Do an upsert to prep the cid cache.
        let upsert_opts = UpsertOptions::new(
            key.as_slice(),
            &scope_name,
            &collection_name,
            value.as_slice(),
        )
        .retry_strategy(strat.clone());

        let upsert_result = agent.upsert(upsert_opts).await.unwrap();

        assert_ne!(0, upsert_result.cas);
        assert!(upsert_result.mutation_token.is_some());

        delete_collection_and_wait_for_kv(
            &agent,
            &bucket,
            &scope_name,
            &collection_name,
            Instant::now().add(Duration::from_secs(10)),
        )
        .await;

        let upsert_opts = UpsertOptions::new(
            key.as_slice(),
            &scope_name,
            &collection_name,
            value.as_slice(),
        )
        .retry_strategy(Arc::new(BestEffortRetryStrategy::new(
            ExponentialBackoffCalculator::default(),
        )));

        // Even though we wait for the delete collection to be acknowledged, it still may persist in kv
        // on some nodes.
        try_until(
            Instant::now().add(Duration::from_secs(30)),
            Duration::from_millis(100),
            "upsert didn't fail with timeout in allowed time",
            || async {
                let upsert_result = timeout_at(
                    Instant::now().add(Duration::from_millis(2500)),
                    agent.upsert(upsert_opts.clone()),
                )
                .await;

                match upsert_result {
                    Ok(_) => Ok(None),
                    Err(_e) => Ok(Some(())),
                }
            },
        )
        .await;
    });
}

#[test]
fn test_changed_collection_id() {
    setup_test(async |config| {
        let agent_opts = create_default_options(config.clone()).await;
        let bucket = config.bucket;
        let scope_name = config.scope;
        let collection_name = rng()
            .sample_iter(&Alphanumeric)
            .take(10)
            .map(char::from)
            .collect::<String>();

        let mut agent = Agent::new(agent_opts).await.unwrap();

        let strat = Arc::new(FailFastRetryStrategy::default());

        let key = generate_key();
        let value = generate_bytes_value(32);

        create_collection_and_wait_for_kv(
            &agent,
            &bucket,
            &scope_name,
            &collection_name,
            Instant::now().add(Duration::from_secs(10)),
        )
        .await;

        // Do an upsert to prep the cid cache.
        let upsert_opts = UpsertOptions::new(
            key.as_slice(),
            &scope_name,
            &collection_name,
            value.as_slice(),
        )
        .retry_strategy(strat.clone());

        let upsert_result = agent.upsert(upsert_opts).await.unwrap();

        assert_ne!(0, upsert_result.cas);
        assert!(upsert_result.mutation_token.is_some());

        delete_collection_and_wait_for_kv(
            &agent,
            &bucket,
            &scope_name,
            &collection_name,
            Instant::now().add(Duration::from_secs(5)),
        )
        .await;

        create_collection_and_wait_for_kv(
            &agent,
            &bucket,
            &scope_name,
            &collection_name,
            Instant::now().add(Duration::from_secs(5)),
        )
        .await;

        let upsert_opts = UpsertOptions::new(
            key.as_slice(),
            &scope_name,
            &collection_name,
            value.as_slice(),
        )
        .retry_strategy(Arc::new(BestEffortRetryStrategy::new(
            ExponentialBackoffCalculator::default(),
        )));

        // This call should now get a cid unknown error and fetch the new one.
        let upsert_result = agent.upsert(upsert_opts).await.unwrap();

        assert_ne!(0, upsert_result.cas);
        assert!(upsert_result.mutation_token.is_some());
    });
}

#[test]
fn test_unknown_scope() {
    setup_test(async |config| {
        let agent_opts = create_default_options(config.clone()).await;
        let scope_name = generate_string_key();
        let collection_name = generate_string_key();

        let mut agent = Agent::new(agent_opts).await.unwrap();

        let strat = Arc::new(BestEffortRetryStrategy::default());

        let key = generate_key();
        let value = generate_bytes_value(32);

        let upsert_opts = UpsertOptions::new(
            key.as_slice(),
            &scope_name,
            &collection_name,
            value.as_slice(),
        )
        .retry_strategy(strat);

        try_until(
            Instant::now().add(Duration::from_secs(30)),
            Duration::from_millis(100),
            "upsert didn't fail with timeout in allowed time",
            || async {
                let upsert_result = timeout_at(
                    Instant::now().add(Duration::from_millis(2500)),
                    agent.upsert(upsert_opts.clone()),
                )
                .await;

                match upsert_result {
                    Ok(_) => Ok(None),
                    Err(_e) => Ok(Some(())),
                }
            },
        )
        .await;
    });
}

// ---------------------------------------------------------------------------
// Range scan
// ---------------------------------------------------------------------------

/// The end of the key space, for a scan with no upper bound within its prefix.
const RANGE_SCAN_KEY_MAX: &[u8] = &[0xff; 16];

/// Seed `count` documents under a shared prefix and return the prefix.
///
/// The default collection is used with a unique prefix rather than a fresh
/// collection: the range bounds are on whole keys, so a prefix range isolates
/// this run's documents from every other test's without paying for a collection
/// create and a manifest propagation wait.
async fn seed_scan_documents(agent: &TestAgent, count: usize) -> (String, Vec<String>) {
    let prefix = format!("rangescan-{}-", generate_string_key());
    let strat = Arc::new(BestEffortRetryStrategy::new(
        ExponentialBackoffCalculator::default(),
    ));

    let mut keys = Vec::with_capacity(count);
    for i in 0..count {
        let key = format!("{prefix}{i:04}");
        agent
            .upsert(
                UpsertOptions::new(key.as_bytes(), "", "", br#"{"scanned":true}"#)
                    .retry_strategy(strat.clone()),
            )
            .await
            .unwrap();
        keys.push(key);
    }

    (prefix, keys)
}

/// Scan every vbucket for `prefix`, draining each scan to completion.
///
/// Returns the keys found, and how many vbuckets actually opened a scan. A
/// vbucket with nothing in range answers `KeyNotFound` at create rather than
/// opening an empty scan, which is an empty result and not a failure.
async fn scan_all_vbuckets(
    agent: &TestAgent,
    prefix: &str,
    keys_only: bool,
) -> (Vec<String>, usize, usize) {
    let mut end = prefix.as_bytes().to_vec();
    end.extend_from_slice(RANGE_SCAN_KEY_MAX);

    let num_vbuckets = agent.num_vbuckets().await.unwrap();
    let mut found = vec![];
    let mut scanned_vbuckets = 0;
    let mut values_seen = 0;

    for vb in 0..num_vbuckets as u16 {
        let created = agent
            .range_scan_create(
                RangeScanCreateOptions::new("", "", vb)
                    .keys_only(keys_only)
                    .range(RangeScanCreateRangeScanConfig {
                        start: Some(prefix.as_bytes()),
                        end: Some(&end),
                        exclusive_start: None,
                        exclusive_end: None,
                    })
                    .retry_strategy(Arc::new(FailFastRetryStrategy::default())),
            )
            .await;

        let scan = match created {
            Ok(scan) => scan,
            Err(e) => {
                assert!(
                    is_memdx_error(&e)
                        .map(|me| matches!(
                            me.kind(),
                            ErrorKind::Server(se) if se.kind() == &ServerErrorKind::KeyNotFound
                        ))
                        .unwrap_or(false),
                    "range scan create on vbucket {vb} failed with something other than an \
                     empty vbucket: {e}"
                );
                continue;
            }
        };
        scanned_vbuckets += 1;

        loop {
            let res = scan
                .continue_scan(&RangeScanContinueOptions::new(), |resp| match resp.items {
                    RangeScanItemIter::Full(items) => {
                        for item in items {
                            let item = item.unwrap();
                            if !item.value.is_empty() {
                                values_seen += 1;
                            }
                            found.push(String::from_utf8(item.key.to_vec()).unwrap());
                        }
                    }
                    RangeScanItemIter::KeyOnly(items) => {
                        for item in items {
                            found.push(String::from_utf8(item.unwrap().key.to_vec()).unwrap());
                        }
                    }
                })
                .await
                .unwrap();

            // Drain until `complete`, not until `more` goes false: a continue
            // that is still streaming reports neither.
            if res.complete {
                break;
            }
        }
    }

    (found, scanned_vbuckets, values_seen)
}

#[test]
fn test_range_scan_reads_every_seeded_document() {
    run_test(async |agent| {
        if !feature_supported(&agent, BucketFeature::RangeScan).await {
            return;
        }

        let (prefix, mut keys) = seed_scan_documents(&agent, 50).await;
        keys.sort();

        // A range scan reads the vbucket's persisted state, so a document is
        // not necessarily in range the instant its write is acknowledged. Retry
        // the whole fan-out rather than sleeping a guess.
        let found = try_until(
            Instant::now().add(Duration::from_secs(30)),
            Duration::from_millis(250),
            "range scan did not see every seeded document in time",
            || async {
                let (mut found, scanned, values) = scan_all_vbuckets(&agent, &prefix, false).await;
                found.sort();
                if found.len() == keys.len() {
                    assert_eq!(
                        values,
                        keys.len(),
                        "a full scan should carry a value for every document"
                    );
                    assert!(scanned > 0, "no vbucket opened a scan");
                    return Ok(Some(found));
                }
                Ok(None)
            },
        )
        .await;

        assert_eq!(found, keys);
    });
}

#[test]
fn test_range_scan_keys_only_omits_the_values() {
    run_test(async |agent| {
        if !feature_supported(&agent, BucketFeature::RangeScan).await {
            return;
        }

        let (prefix, mut keys) = seed_scan_documents(&agent, 10).await;
        keys.sort();

        let found = try_until(
            Instant::now().add(Duration::from_secs(30)),
            Duration::from_millis(250),
            "keys-only range scan did not see every seeded document in time",
            || async {
                let (mut found, _, values) = scan_all_vbuckets(&agent, &prefix, true).await;
                found.sort();
                if found.len() == keys.len() {
                    assert_eq!(values, 0, "a keys-only scan must not carry values");
                    return Ok(Some(found));
                }
                Ok(None)
            },
        )
        .await;

        assert_eq!(found, keys);
    });
}

/// Cancelling releases the scan server-side, which the next continue reports.
#[test]
fn test_range_scan_cancel_releases_the_scan() {
    run_test(async |agent| {
        if !feature_supported(&agent, BucketFeature::RangeScan).await {
            return;
        }

        let (prefix, keys) = seed_scan_documents(&agent, 20).await;
        let mut end = prefix.as_bytes().to_vec();
        end.extend_from_slice(RANGE_SCAN_KEY_MAX);

        // Wait until the documents are scannable, then find a vbucket that has
        // some -- an empty one never opens a scan and so has nothing to cancel.
        let vb = try_until(
            Instant::now().add(Duration::from_secs(30)),
            Duration::from_millis(250),
            "no vbucket opened a scan in time",
            || async {
                let (found, _, _) = scan_all_vbuckets(&agent, &prefix, true).await;
                if found.len() != keys.len() {
                    return Ok(None);
                }
                let num_vbuckets = agent.num_vbuckets().await.unwrap();
                for vb in 0..num_vbuckets as u16 {
                    let created = agent
                        .range_scan_create(RangeScanCreateOptions::new("", "", vb).range(
                            RangeScanCreateRangeScanConfig {
                                start: Some(prefix.as_bytes()),
                                end: Some(&end),
                                exclusive_start: None,
                                exclusive_end: None,
                            },
                        ))
                        .await;
                    if let Ok(scan) = created {
                        scan.cancel(&RangeScanCancelOptions::new()).await.unwrap();
                        return Ok(Some(vb));
                    }
                }
                Ok(None)
            },
        )
        .await;

        // And a cancelled scan is gone: a fresh scan on the same vbucket, then
        // cancel, then continue.
        let scan = agent
            .range_scan_create(RangeScanCreateOptions::new("", "", vb).range(
                RangeScanCreateRangeScanConfig {
                    start: Some(prefix.as_bytes()),
                    end: Some(&end),
                    exclusive_start: None,
                    exclusive_end: None,
                },
            ))
            .await
            .unwrap();

        scan.cancel(&RangeScanCancelOptions::new()).await.unwrap();

        let err = scan
            .continue_scan(&RangeScanContinueOptions::new(), |_| {})
            .await
            .expect_err("continuing a cancelled scan should fail");
        let memdx_err = is_memdx_error(&err).expect("expected a memdx error");
        assert!(
            matches!(
                memdx_err.kind(),
                ErrorKind::Server(se)
                    if se.kind() == &ServerErrorKind::KeyNotFound
                        || se.kind() == &ServerErrorKind::RangeScanCancelled
            ),
            "continuing a cancelled scan reported {memdx_err}"
        );
    });
}

/// Abandoning a scan mid-drain must not take the connection with it.
///
/// A continue is the only operation whose opaque stays registered across
/// replies, so a caller that walks away from one leaves packets arriving for a
/// receiver that is gone. Those have to be reported as orphans and the
/// connection left alone -- every other operation on that socket is innocent.
#[test]
fn test_range_scan_abandoned_mid_drain_leaves_the_connection_usable() {
    run_test(async |agent| {
        if !feature_supported(&agent, BucketFeature::RangeScan).await {
            return;
        }

        let (prefix, keys) = seed_scan_documents(&agent, 50).await;
        let mut end = prefix.as_bytes().to_vec();
        end.extend_from_slice(RANGE_SCAN_KEY_MAX);

        let (found, _, _) = try_until(
            Instant::now().add(Duration::from_secs(30)),
            Duration::from_millis(250),
            "range scan did not see every seeded document in time",
            || async {
                let res = scan_all_vbuckets(&agent, &prefix, true).await;
                if res.0.len() == keys.len() {
                    return Ok(Some(res));
                }
                Ok(None)
            },
        )
        .await;
        assert_eq!(found.len(), keys.len());

        let num_vbuckets = agent.num_vbuckets().await.unwrap();
        let mut abandoned = 0;
        for vb in 0..num_vbuckets as u16 {
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
                continue;
            };

            // One millisecond is shorter than a scan continue's disk read, so
            // the future is dropped with the reply still in the air. If a run
            // happens to beat the deadline the assertions below still hold; it
            // just did not exercise the abandon path that round.
            let _ = timeout_at(
                Instant::now().add(Duration::from_millis(1)),
                scan.continue_scan(&RangeScanContinueOptions::new(), |_| {}),
            )
            .await;
            drop(scan);
            abandoned += 1;
            if abandoned == 4 {
                break;
            }
        }
        assert!(abandoned > 0, "no vbucket opened a scan to abandon");

        // The connection those scans went out on must still serve everything
        // else, and a fresh fan-out must still read the whole collection.
        let key = keys.first().unwrap();
        agent
            .get(GetOptions::new(key.as_bytes(), "", ""))
            .await
            .expect("a get after an abandoned scan should still work");

        let (found, _, _) = try_until(
            Instant::now().add(Duration::from_secs(30)),
            Duration::from_millis(250),
            "range scan did not recover after a scan was abandoned",
            || async {
                let res = scan_all_vbuckets(&agent, &prefix, true).await;
                if res.0.len() == keys.len() {
                    return Ok(Some(res));
                }
                Ok(None)
            },
        )
        .await;
        assert_eq!(found.len(), keys.len());
    });
}

/// A range scan goes to the bulk connection manager, not the primary one.
///
/// **Membership is not observable from outside, so it is proved by removal.**
/// The bulk manager is the only route a streaming operation has; taking its
/// connections away leaves point operations untouched and range scans with
/// nowhere to go. If a scan ever fell back to the primary manager's connections
/// -- which is the whole thing this separation exists to prevent -- the create
/// below would succeed and this test would fail.
#[test]
fn test_range_scan_uses_the_bulk_connection_manager() {
    setup_test(async |config| {
        let mut agent_opts = create_default_options(config).await;
        agent_opts.kv_config = KvConfig::new().num_bulk_connections(0);

        let agent = Agent::new(agent_opts).await.unwrap();
        agent
            .wait_until_ready(&WaitUntilReadyOptions::new().service_types(vec![ServiceType::MEMD]))
            .await
            .unwrap();

        if !agent
            .bucket_features()
            .await
            .unwrap()
            .contains(&BucketFeature::RangeScan)
        {
            return;
        }

        let strat = Arc::new(FailFastRetryStrategy::default());
        let key = generate_key();
        let value = generate_bytes_value(32);

        // The primary manager still has its connections, so a point operation
        // is unaffected.
        agent
            .upsert(
                UpsertOptions::new(key.as_slice(), "", "", value.as_slice())
                    .retry_strategy(strat.clone()),
            )
            .await
            .expect("a point operation should not go near the bulk manager");
        agent
            .get(GetOptions::new(key.as_slice(), "", "").retry_strategy(strat.clone()))
            .await
            .expect("a point operation should not go near the bulk manager");

        let mut end = key.clone();
        end.extend_from_slice(RANGE_SCAN_KEY_MAX);
        let err = agent
            .range_scan_create(
                RangeScanCreateOptions::new("", "", 0)
                    .range(RangeScanCreateRangeScanConfig {
                        start: Some(&key),
                        end: Some(&end),
                        exclusive_start: None,
                        exclusive_end: None,
                    })
                    .retry_strategy(strat),
            )
            .await
            .expect_err("a range scan with no bulk connections has nowhere to run");
        assert!(
            err.to_string().contains("no connections configured"),
            "a range scan with no bulk connections failed with {err}, which is not \
             the bulk manager refusing it"
        );
    });
}

// ---------------------------------------------------------------------------
// STAT
// ---------------------------------------------------------------------------

#[test]
fn test_stats_sweeps_every_node() {
    run_test(async |agent| {
        let mut entries = vec![];
        let result = agent
            .stats(StatsOptions::new(""), |entry| {
                entries.push((
                    entry.endpoint.to_string(),
                    entry.key_str().into_owned(),
                    entry.value_str().into_owned(),
                ));
            })
            .await
            .unwrap();

        assert!(result.endpoints > 0, "no node answered the sweep");
        assert_eq!(result.entries, entries.len());
        assert!(
            entries.len() > 20,
            "the default stat group returned {} entries, which is not a group",
            entries.len()
        );

        // Every entry is attributed, and nothing empty gets through: the empty
        // packet is the terminator and must not reach the caller.
        for (endpoint, key, _) in &entries {
            assert!(!endpoint.is_empty());
            assert!(!key.is_empty(), "an empty key reached the callback");
        }

        // A stat every kv_engine reports, so this is checking the sweep read the
        // real listing rather than something that merely had the right shape.
        assert!(
            entries.iter().any(|(_, key, _)| key == "uptime"),
            "the default group did not include uptime"
        );

        // Every node the manager knows about answered.
        let distinct: std::collections::HashSet<&String> =
            entries.iter().map(|(endpoint, _, _)| endpoint).collect();
        assert_eq!(distinct.len(), result.endpoints);
    });
}

#[test]
fn test_stats_by_vbucket_reads_one_node() {
    run_test(async |agent| {
        let mut entries = 0usize;
        let mut endpoints = std::collections::HashSet::new();

        let result = agent
            .stats_by_vbucket(StatsByVbucketOptions::new("vbucket-details", 0), |entry| {
                entries += 1;
                endpoints.insert(entry.endpoint.to_string());
            })
            .await
            .unwrap();

        assert_eq!(result.endpoints, 1);
        assert_eq!(result.entries, entries);
        assert!(entries > 0, "vbucket-details returned nothing");
        assert_eq!(
            endpoints.len(),
            1,
            "a per-vbucket sweep hit more than one node"
        );
    });
}

/// A `STAT` sweep goes to the bulk connection manager, like a range scan.
///
/// Proved the same way, by removal: with no bulk connections the sweep has
/// nowhere to run while point operations are untouched. See
/// `test_range_scan_uses_the_bulk_connection_manager`.
#[test]
fn test_stats_uses_the_bulk_connection_manager() {
    setup_test(async |config| {
        let mut agent_opts = create_default_options(config).await;
        agent_opts.kv_config = KvConfig::new().num_bulk_connections(0);

        let agent = Agent::new(agent_opts).await.unwrap();
        agent
            .wait_until_ready(&WaitUntilReadyOptions::new().service_types(vec![ServiceType::MEMD]))
            .await
            .unwrap();

        let strat = Arc::new(FailFastRetryStrategy::default());
        let key = generate_key();
        let value = generate_bytes_value(32);
        agent
            .upsert(
                UpsertOptions::new(key.as_slice(), "", "", value.as_slice())
                    .retry_strategy(strat.clone()),
            )
            .await
            .expect("a point operation should not go near the bulk manager");

        let err = agent
            .stats(StatsOptions::new("").retry_strategy(strat.clone()), |_| {
                unreachable!("no node should have answered")
            })
            .await
            .expect_err("a stats sweep with no bulk connections has nowhere to run");
        assert!(
            err.to_string().contains("no connections configured"),
            "a stats sweep with no bulk connections failed with {err}, which is not \
             the bulk manager refusing it"
        );

        let err = agent
            .stats_by_vbucket(
                StatsByVbucketOptions::new("", 0).retry_strategy(strat),
                |_| unreachable!("no node should have answered"),
            )
            .await
            .expect_err("a per-vbucket sweep with no bulk connections has nowhere to run");
        assert!(
            err.to_string().contains("no connections configured"),
            "a per-vbucket sweep with no bulk connections failed with {err}"
        );
    });
}
