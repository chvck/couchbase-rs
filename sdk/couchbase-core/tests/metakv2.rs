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

//! metakv2 is an internal, cluster-wide configuration store shared with
//! whatever else on the cluster uses it, so every test here confines itself to
//! one randomly named subtree under [`TEST_ROOT`] and deletes it again. Each
//! test also sweeps the whole root first, which is safe because these tests are
//! serialised, and which cleans up after a previous run that panicked partway
//! through.

use crate::common::features::TestFeatureCode;
use crate::common::helpers::generate_key_with_letter_prefix;
use crate::common::test_agent::TestAgent;
use crate::common::test_config::run_test;
use couchbase_core::mgmtx::metakv2::{MetaKv2Revision, MetaKv2Write};
use couchbase_core::options::management::{
    DeleteMetaKv2DirOptions, GetMetaKv2DirOptions, GetMetaKv2Options, SetMetaKv2MultipleOptions,
    SetMetaKv2Options, SyncMetaKv2QuorumOptions,
};
use couchbase_core::{error, mgmtx};
use serial_test::serial;
use std::collections::BTreeMap;

mod common;

/// Everything these tests write lives under here, so a sweep of one path is
/// enough to leave the store as it was found.
const TEST_ROOT: &str = "/couchbase-core-test-metakv2/";

#[test]
#[serial]
fn test_metakv2_set_get_and_delete_dir() {
    run_test(async |agent| {
        if !agent.supports_feature(&TestFeatureCode::Metakv2) {
            return;
        }

        let dir = fresh_dir(&agent).await;
        let leaf = format!("{dir}a");
        let nested = format!("{dir}sub/b");

        let set = agent
            .set_metakv2(&SetMetaKv2Options::new(&leaf, "hello"))
            .await
            .unwrap();
        let revision = set.revision.expect("a first write must issue a revision");

        let entry = agent
            .get_metakv2(&GetMetaKv2Options::new(&leaf))
            .await
            .unwrap();
        assert_eq!(entry.value, "hello");
        assert_eq!(entry.revision, revision);
        assert!(
            !entry.revision.history().is_empty(),
            "a revision must carry a history uuid, got {}",
            entry.revision
        );

        // A recursive listing nests a subdirectory's children inside that
        // subdirectory's own node, so this leaf is the regression guard for a
        // parse that stops at the first level.
        agent
            .set_metakv2(&SetMetaKv2Options::new(&nested, "world"))
            .await
            .unwrap();

        let listing = agent
            .get_metakv2_dir(&GetMetaKv2DirOptions::new(&dir))
            .await
            .unwrap();

        assert_eq!(
            listing.entries.keys().cloned().collect::<Vec<String>>(),
            vec![leaf.clone(), nested.clone()],
        );
        assert_eq!(listing.entries[&leaf].value, "hello");
        assert_eq!(listing.entries[&nested].value, "world");
        assert!(!listing.revision.0.is_empty());

        let missing = format!("{dir}nope");
        let err = agent
            .get_metakv2(&GetMetaKv2Options::new(&missing))
            .await
            .expect_err("expected a read of an unpublished key to fail");
        assert_server_kind(&err, &mgmtx::error::ServerErrorKind::MetaKvEntryNotFound);

        agent
            .delete_metakv2_dir(&DeleteMetaKv2DirOptions::new(&dir))
            .await
            .unwrap();

        let err = agent
            .get_metakv2_dir(&GetMetaKv2DirOptions::new(&dir))
            .await
            .expect_err("expected a read of a deleted directory to fail");
        assert_server_kind(&err, &mgmtx::error::ServerErrorKind::MetaKvEntryNotFound);

        // A second delete answers not-found. Note that a 404 here is not proof
        // of absence in general — see the module docs — it is only that on a
        // healthy cluster reading its own write.
        let err = agent
            .delete_metakv2_dir(&DeleteMetaKv2DirOptions::new(&dir))
            .await
            .expect_err("expected a second delete to fail");
        assert_server_kind(&err, &mgmtx::error::ServerErrorKind::MetaKvEntryNotFound);

        // A directory outlives its last child, so the root marker is still there
        // even though the subtree under it is gone.
        cleanup(&agent, TEST_ROOT).await;
    });
}

#[test]
#[serial]
fn test_metakv2_a_commit_that_changes_nothing_issues_no_revision() {
    run_test(async |agent| {
        if !agent.supports_feature(&TestFeatureCode::Metakv2) {
            return;
        }

        let dir = fresh_dir(&agent).await;
        let leaf = format!("{dir}a");

        let first = agent
            .set_metakv2(&SetMetaKv2Options::new(&leaf, "hello"))
            .await
            .unwrap();
        let revision = first.revision.expect("a first write must issue a revision");

        // Writing a key the value it already holds changes nothing, and the
        // store says so by issuing no revision at all.
        let again = agent
            .set_metakv2(&SetMetaKv2Options::new(&leaf, "hello"))
            .await
            .unwrap();
        assert_eq!(again.revision, None);

        // The key keeps the revision it was stamped with.
        let entry = agent
            .get_metakv2(&GetMetaKv2Options::new(&leaf))
            .await
            .unwrap();
        assert_eq!(entry.revision, revision);

        // The same holds for a multi-key commit, and for an empty one, which is
        // answered without a round trip.
        let mut writes = BTreeMap::new();
        writes.insert(leaf.clone(), MetaKv2Write::set("hello", None));
        let commit = agent
            .set_metakv2_multiple(&SetMetaKv2MultipleOptions::new(&writes))
            .await
            .unwrap();
        assert_eq!(commit.revision, None);

        let empty = BTreeMap::new();
        let commit = agent
            .set_metakv2_multiple(&SetMetaKv2MultipleOptions::new(&empty))
            .await
            .unwrap();
        assert_eq!(commit.revision, None);

        cleanup(&agent, TEST_ROOT).await;
    });
}

#[test]
#[serial]
fn test_metakv2_a_stale_revision_conflicts() {
    run_test(async |agent| {
        if !agent.supports_feature(&TestFeatureCode::Metakv2) {
            return;
        }

        let dir = fresh_dir(&agent).await;
        let leaf = format!("{dir}a");

        let revision = agent
            .set_metakv2(&SetMetaKv2Options::new(&leaf, "one"))
            .await
            .unwrap()
            .revision
            .expect("a first write must issue a revision");

        let stale = MetaKv2Revision::new(format!("{}:1", revision.history()));
        let err = agent
            .set_metakv2(&SetMetaKv2Options::new(&leaf, "two").revision(&stale))
            .await
            .expect_err("expected a stale revision to conflict");

        // The body names the conflicting path and the revision the key actually
        // stands at, which is what lets a caller rebase.
        assert_server_kind(
            &err,
            &mgmtx::error::ServerErrorKind::MetaKvConflict {
                path: leaf.clone(),
                current_revision: Some(revision.0.clone()),
            },
        );

        // Nothing was applied.
        let entry = agent
            .get_metakv2(&GetMetaKv2Options::new(&leaf))
            .await
            .unwrap();
        assert_eq!(entry.value, "one");

        // The current revision does apply.
        let updated = agent
            .set_metakv2(&SetMetaKv2Options::new(&leaf, "two").revision(&revision))
            .await
            .unwrap()
            .revision
            .expect("a write that changes a value must issue a revision");
        assert_ne!(updated, revision);

        let entry = agent
            .get_metakv2(&GetMetaKv2Options::new(&leaf))
            .await
            .unwrap();
        assert_eq!(entry.value, "two");
        assert_eq!(entry.revision, updated);

        cleanup(&agent, TEST_ROOT).await;
    });
}

#[test]
#[serial]
fn test_metakv2_create_over_an_existing_key_conflicts() {
    run_test(async |agent| {
        if !agent.supports_feature(&TestFeatureCode::Metakv2) {
            return;
        }

        let dir = fresh_dir(&agent).await;
        let leaf = format!("{dir}a");

        let mut writes = BTreeMap::new();
        writes.insert(leaf.clone(), MetaKv2Write::create("one"));

        assert!(
            agent
                .set_metakv2_multiple(&SetMetaKv2MultipleOptions::new(&writes))
                .await
                .unwrap()
                .revision
                .is_some(),
            "expected a create of an absent key to issue a revision"
        );

        // A create collision is the same 409 a stale precondition gives, so the
        // caller tells them apart by knowing that its own entry carried no
        // revision.
        let err = agent
            .set_metakv2_multiple(&SetMetaKv2MultipleOptions::new(&writes))
            .await
            .expect_err("expected a create over an existing key to conflict");

        match server_kind(&err) {
            mgmtx::error::ServerErrorKind::MetaKvConflict { path, .. } => {
                assert_eq!(path, &leaf)
            }
            other => panic!("expected a metakv conflict, got {other}"),
        }

        cleanup(&agent, TEST_ROOT).await;
    });
}

#[test]
#[serial]
fn test_metakv2_a_multi_key_commit_is_all_or_nothing() {
    run_test(async |agent| {
        if !agent.supports_feature(&TestFeatureCode::Metakv2) {
            return;
        }

        let dir = fresh_dir(&agent).await;
        let first = format!("{dir}a");
        let second = format!("{dir}sub/b");

        let mut writes = BTreeMap::new();
        writes.insert(first.clone(), MetaKv2Write::set("one", None));
        writes.insert(second.clone(), MetaKv2Write::set("two", None));

        // Missing parent directories are created, which is what every write on a
        // cold start needs.
        let revision = agent
            .set_metakv2_multiple(&SetMetaKv2MultipleOptions::new(&writes))
            .await
            .unwrap()
            .revision
            .expect("a first commit must issue a revision");

        // Every key the commit changed carries the one revision the commit was
        // stamped with.
        let listing = agent
            .get_metakv2_dir(&GetMetaKv2DirOptions::new(&dir))
            .await
            .unwrap();
        assert_eq!(listing.entries[&first].revision, revision);
        assert_eq!(listing.entries[&second].revision, revision);

        // One stale precondition rolls the whole commit back, including the
        // entry that carried no precondition at all.
        let stale = MetaKv2Revision::new(format!("{}:1", revision.history()));
        let mut writes = BTreeMap::new();
        writes.insert(first.clone(), MetaKv2Write::set("three", None));
        writes.insert(second.clone(), MetaKv2Write::set("four", Some(stale)));

        let err = agent
            .set_metakv2_multiple(&SetMetaKv2MultipleOptions::new(&writes))
            .await
            .expect_err("expected one stale revision to fail the whole commit");
        assert_server_kind(
            &err,
            &mgmtx::error::ServerErrorKind::MetaKvConflict {
                path: second.clone(),
                current_revision: Some(revision.0.clone()),
            },
        );

        let listing = agent
            .get_metakv2_dir(&GetMetaKv2DirOptions::new(&dir))
            .await
            .unwrap();
        assert_eq!(listing.entries[&first].value, "one");
        assert_eq!(listing.entries[&second].value, "two");

        cleanup(&agent, TEST_ROOT).await;
    });
}

#[test]
#[serial]
fn test_metakv2_sync_quorum() {
    run_test(async |agent| {
        if !agent.supports_feature(&TestFeatureCode::Metakv2) {
            return;
        }

        // The only measured way to tell a current node from a stale one. On a
        // healthy node it is a few milliseconds; on a node without quorum it
        // fails after ~15s regardless of any timeout asked for.
        agent
            .sync_metakv2_quorum(&SyncMetaKv2QuorumOptions::new())
            .await
            .unwrap();
    });
}

#[test]
#[serial]
fn test_metakv2_rejects_the_wrong_path_shape() {
    run_test(async |agent| {
        if !agent.supports_feature(&TestFeatureCode::Metakv2) {
            return;
        }

        // The server answers a mis-shaped path with a bare 404, which is
        // indistinguishable from an absent key, so the shape is checked here
        // instead of being sent.
        let err = agent
            .get_metakv2(&GetMetaKv2Options::new(TEST_ROOT))
            .await
            .expect_err("expected a leaf read of a directory path to be rejected");
        assert_invalid_argument(&err);

        let err = agent
            .get_metakv2_dir(&GetMetaKv2DirOptions::new("/couchbase-core-test-metakv2"))
            .await
            .expect_err("expected a directory read without a trailing slash to be rejected");
        assert_invalid_argument(&err);

        let err = agent
            .delete_metakv2_dir(&DeleteMetaKv2DirOptions::new(
                "/couchbase-core-test-metakv2",
            ))
            .await
            .expect_err("expected a directory delete without a trailing slash to be rejected");
        assert_invalid_argument(&err);

        let err = agent
            .get_metakv2(&GetMetaKv2Options::new(""))
            .await
            .expect_err("expected an empty path to be rejected");
        assert_invalid_argument(&err);
    });
}

/// Sweep anything a previous run left behind and hand back a subtree of this
/// run's own.
///
/// The sweep assumes one metakv2 test run at a time against a given cluster:
/// `#[serial]` covers the tests in this binary and no other test file touches
/// `/_metakv2`, but two checkouts pointed at the same cluster would sweep each
/// other.
async fn fresh_dir(agent: &TestAgent) -> String {
    cleanup(agent, TEST_ROOT).await;

    format!("{TEST_ROOT}{}/", generate_key_with_letter_prefix())
}

/// Best effort: a not-found here is the expected answer when there is nothing
/// to remove.
async fn cleanup(agent: &TestAgent, dir: &str) {
    let _ = agent
        .delete_metakv2_dir(&DeleteMetaKv2DirOptions::new(dir))
        .await;
}

fn server_kind(err: &error::Error) -> &mgmtx::error::ServerErrorKind {
    match err.kind() {
        error::ErrorKind::Mgmt(e) => match e.kind() {
            mgmtx::error::ErrorKind::Server(e, ..) => e.kind(),
            other => panic!("expected a server error, got {other}"),
        },
        other => panic!("expected a management error, got {other}"),
    }
}

fn assert_server_kind(err: &error::Error, expected: &mgmtx::error::ServerErrorKind) {
    assert_eq!(server_kind(err), expected);
}

fn assert_invalid_argument(err: &error::Error) {
    match err.kind() {
        error::ErrorKind::Mgmt(e) => assert!(
            matches!(e.kind(), mgmtx::error::ErrorKind::InvalidArgument { .. }),
            "expected an invalid argument error, got {e}"
        ),
        other => panic!("expected a management error, got {other}"),
    }
}
