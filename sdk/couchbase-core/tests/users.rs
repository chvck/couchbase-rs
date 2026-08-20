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

use crate::common::features::TestFeatureCode;
use crate::common::helpers::{generate_key_with_letter_prefix, try_until};
use crate::common::test_agent::TestAgent;
use crate::common::test_config::run_test;
use couchbase_core::agent::Agent;
use couchbase_core::mgmtx::user::{Group, Role, User, UserAndMetadata};
use couchbase_core::options::management::{
    DeleteGroupOptions, DeleteUserOptions, EnsureGroupOptions, EnsureUserOptions,
    GetAllGroupsOptions, GetAllUsersOptions, GetGroupOptions, GetRolesOptions, GetUserOptions,
    MayManageLocalUsersOptions, UpsertGroupOptions, UpsertUserOptions,
};
use couchbase_core::{error, mgmtx};
use std::ops::Add;
use std::time::Duration;
use tokio::time::{sleep, timeout_at, Instant};
use tracing::error;

mod common;

#[test]
fn test_get_all_roles() {
    run_test(async |mut agent| {
        let opts = GetRolesOptions::new();
        let roles = agent.get_roles(&opts).await.unwrap();

        assert!(!roles.is_empty(), "expected roles to not be empty");
        assert!(!roles[0].display_name.is_empty());
        assert!(!roles[0].description.is_empty());
        assert!(!roles[0].role.name.is_empty());
    });
}

#[test]
fn test_delete_group() {
    run_test(async |mut agent| {
        if !agent.supports_feature(&TestFeatureCode::UserGroups) {
            return;
        }

        let group_name = generate_key_with_letter_prefix();
        let desc = generate_key_with_letter_prefix();
        let roles = vec![
            Role::new("bucket_full_access").bucket(&agent.test_setup_config.bucket),
            Role::new("ro_admin"),
        ];

        let group = Group::new(&group_name, desc, roles);
        create_and_ensure_group(&agent, &group).await;

        delete_and_ensure_group(&agent, &group_name).await;

        let opts = GetGroupOptions::new(&group_name);
        let err = agent
            .get_group(&opts)
            .await
            .expect_err("expected get after delete to error");

        match err.kind() {
            error::ErrorKind::Mgmt(e) => {
                if let mgmtx::error::ErrorKind::Server(e, ..) = e.kind() {
                    assert_eq!(e.kind(), &mgmtx::error::ServerErrorKind::GroupNotFound);
                } else {
                    panic!("expected get after delete to error with GroupNotFound");
                }
            }
            _ => panic!("expected get after delete to error with GroupNotFound"),
        };
    });
}

#[test]
fn test_get_group() {
    run_test(async |mut agent| {
        if !agent.supports_feature(&TestFeatureCode::UserGroups) {
            return;
        }

        let group_name = generate_key_with_letter_prefix();
        let desc = generate_key_with_letter_prefix();
        let roles = vec![
            Role::new("bucket_full_access").bucket(&agent.test_setup_config.bucket),
            Role::new("ro_admin"),
        ];

        let group = Group::new(&group_name, desc, roles);
        create_and_ensure_group(&agent, &group).await;

        let opts = GetGroupOptions::new(&group_name);
        let actual = agent.get_group(&opts).await.unwrap();

        assert_eq!(actual.name, group.name);
        assert_eq!(actual.roles, group.roles);
        assert_eq!(actual.description, group.description);
        assert_eq!(actual.ldap_group_reference, group.ldap_group_reference);
    });
}

#[test]
fn test_get_all_groups() {
    run_test(async |mut agent| {
        if !agent.supports_feature(&TestFeatureCode::UserGroups) {
            return;
        }

        let group_name = generate_key_with_letter_prefix();
        let desc = generate_key_with_letter_prefix();
        let roles = vec![
            Role::new("bucket_full_access").bucket(&agent.test_setup_config.bucket),
            Role::new("ro_admin"),
        ];

        let group = Group::new(&group_name, desc, roles);
        create_and_ensure_group(&agent, &group).await;

        let opts = GetAllGroupsOptions::new();
        let groups = agent.get_all_groups(&opts).await.unwrap();

        let mut actual = None;
        for actual_group in groups {
            if actual_group.name == group_name {
                actual = Some(actual_group);
            }
        }

        let actual = actual.unwrap();

        assert_eq!(actual.name, group.name);
        assert_eq!(actual.roles, group.roles);
        assert_eq!(actual.description, group.description);
        assert_eq!(actual.ldap_group_reference, group.ldap_group_reference);
    });
}

#[test]
fn test_delete_user() {
    run_test(async |mut agent| {
        if !agent.supports_feature(&TestFeatureCode::UsersMB69096) {
            return;
        }

        let username = generate_key_with_letter_prefix();
        let display_name = generate_key_with_letter_prefix();
        let roles = vec![
            Role::new("bucket_full_access").bucket(&agent.test_setup_config.bucket),
            Role::new("ro_admin"),
        ];

        let user = User::new(&username, display_name, roles).password("password");
        create_and_ensure_user(&agent, &user).await;

        delete_and_ensure_user(&agent, &username).await;

        let opts = GetUserOptions::new(&username, "local");
        let err = agent
            .get_user(&opts)
            .await
            .expect_err("expected get after delete to error");

        match err.kind() {
            error::ErrorKind::Mgmt(e) => {
                if let mgmtx::error::ErrorKind::Server(e, ..) = e.kind() {
                    assert_eq!(e.kind(), &mgmtx::error::ServerErrorKind::UserNotFound);
                } else {
                    panic!("expected get after delete to error with UserNotFound");
                }
            }
            _ => panic!("expected get after delete to error with UserNotFound"),
        };
    });
}

#[test]
fn test_get_user() {
    run_test(async |mut agent| {
        if !agent.supports_feature(&TestFeatureCode::UsersMB69096) {
            return;
        }

        let username = generate_key_with_letter_prefix();
        let display_name = generate_key_with_letter_prefix();
        let roles = vec![
            Role::new("bucket_full_access").bucket(&agent.test_setup_config.bucket),
            Role::new("ro_admin"),
        ];

        let user = User::new(&username, display_name, roles).password("password");
        create_and_ensure_user(&agent, &user).await;

        let opts = GetUserOptions::new(&username, "local");
        let actual = agent.get_user(&opts).await.unwrap();

        assert_user(&user, &actual);
    });
}

#[test]
fn test_get_all_users() {
    run_test(async |mut agent| {
        if !agent.supports_feature(&TestFeatureCode::UsersMB69096) {
            return;
        }

        let username = generate_key_with_letter_prefix();
        let display_name = generate_key_with_letter_prefix();
        let roles = vec![
            Role::new("bucket_full_access").bucket(&agent.test_setup_config.bucket),
            Role::new("ro_admin"),
        ];

        let user = User::new(username, display_name, roles).password("password");
        create_and_ensure_user(&agent, &user).await;

        let opts = GetAllUsersOptions::new("local");
        let users = agent.get_all_users(&opts).await.unwrap();

        let mut actual = None;
        for actual_user in users {
            if actual_user.user.username == user.username {
                actual = Some(actual_user);
            }
        }

        assert_user(&user, actual.as_ref().unwrap());
    });
}

fn assert_user(expected: &User, actual: &UserAndMetadata) {
    assert_eq!(actual.domain, "local");
    assert_eq!(2, actual.effective_roles.len());
    assert!(actual.external_groups.is_empty());
    assert_eq!(actual.user.username, expected.username);
    assert_eq!(actual.user.display_name, expected.display_name);
    assert_eq!(actual.user.groups, expected.groups);
    assert_eq!(actual.user.roles, expected.roles);
}

async fn create_and_ensure_user(agent: &TestAgent, user: &User) {
    agent
        .upsert_user(&UpsertUserOptions::new(user, "local"))
        .await
        .unwrap();

    try_until(
        Instant::now().add(Duration::from_secs(30)),
        Duration::from_millis(500),
        "failed to ensure group in time",
        async || match agent
            .ensure_user(&EnsureUserOptions::new(&user.username, "local", false))
            .await
        {
            Ok(_) => Ok(Some(())),
            Err(e) => {
                error!("failed to ensure user: {e}");
                Err(e)
            }
        },
    )
    .await;
}

async fn create_and_ensure_group(agent: &TestAgent, group: &Group) {
    agent
        .upsert_group(&UpsertGroupOptions::new(group))
        .await
        .unwrap();

    try_until(
        Instant::now().add(Duration::from_secs(30)),
        Duration::from_millis(500),
        "failed to ensure group in time",
        async || match agent
            .ensure_group(&EnsureGroupOptions::new(&group.name, false))
            .await
        {
            Ok(_) => Ok(Some(())),
            Err(e) => {
                error!("failed to ensure group: {e}");
                Err(e)
            }
        },
    )
    .await;
}

async fn delete_and_ensure_user(agent: &TestAgent, username: &str) {
    agent
        .delete_user(&DeleteUserOptions::new(username, "local"))
        .await
        .unwrap();

    try_until(
        Instant::now().add(Duration::from_secs(30)),
        Duration::from_millis(500),
        "failed to ensure group in time",
        async || match agent
            .ensure_user(&EnsureUserOptions::new(username, "local", true))
            .await
        {
            Ok(_) => Ok(Some(())),
            Err(e) => {
                error!("failed to ensure user: {e}");
                Err(e)
            }
        },
    )
    .await;
}

async fn delete_and_ensure_group(agent: &TestAgent, group_name: &str) {
    agent
        .delete_group(&DeleteGroupOptions::new(group_name))
        .await
        .unwrap();

    try_until(
        Instant::now().add(Duration::from_secs(30)),
        Duration::from_millis(500),
        "failed to ensure group in time",
        async || match agent
            .ensure_group(&EnsureGroupOptions::new(group_name, true))
            .await
        {
            Ok(_) => Ok(Some(())),
            Err(e) => {
                error!("failed to ensure group: {e}");
                Err(e)
            }
        },
    )
    .await;
}

/// Ported from cbcore-rs `tests/users_int.rs::a_group_carries_roles_a_user_inherits`.
///
/// The group tests above never create a user and the user tests never grant a
/// group, so nothing crossed the two — and `RoleAndOrigins::origins`, the only
/// thing that says *where* a privilege came from, was never read by a test at
/// all. A user granted only a group must still come back holding that group's
/// roles, marked as inherited.
#[test]
fn a_group_carries_roles_a_user_inherits() {
    run_test(async |mut agent| {
        if !agent.supports_feature(&TestFeatureCode::UserGroups)
            || !agent.supports_feature(&TestFeatureCode::UsersMB69096)
        {
            return;
        }

        let group_name = generate_key_with_letter_prefix();
        let username = generate_key_with_letter_prefix();
        let bucket = agent.test_setup_config.bucket.clone();
        let inherited = Role::new("data_reader").bucket(&bucket);

        let group = Group::new(
            &group_name,
            generate_key_with_letter_prefix(),
            vec![inherited.clone()],
        );
        create_and_ensure_group(&agent, &group).await;

        // No direct roles at all: everything this user can do arrives through
        // the group.
        let user = User::new(&username, generate_key_with_letter_prefix(), vec![])
            .groups(vec![group_name.clone()])
            .password("password");
        create_and_ensure_user(&agent, &user).await;

        let actual = agent
            .get_user(&GetUserOptions::new(&username, "local"))
            .await
            .unwrap();

        // Read everything first, then tear down, so a failed assertion does not
        // leave a user and a group behind on the cluster.
        delete_and_ensure_user(&agent, &username).await;
        delete_and_ensure_group(&agent, &group_name).await;

        assert_eq!(vec![group_name.clone()], actual.user.groups);
        assert!(
            actual.user.roles.is_empty(),
            "the user was granted no roles directly, got {:?}",
            actual.user.roles
        );

        // Matched on name and bucket rather than on the whole role: the server
        // reports an effective bucket-scoped role with its scope and collection
        // filled in as `*`, where the grant named neither.
        let effective = actual
            .effective_roles
            .iter()
            .find(|r| r.role.name == inherited.name && r.role.bucket == inherited.bucket)
            .unwrap_or_else(|| {
                panic!(
                    "the group's role did not reach the user: {:?}",
                    actual.effective_roles
                )
            });

        assert_eq!(Some("*"), effective.role.scope.as_deref());
        assert_eq!(Some("*"), effective.role.collection.as_deref());

        assert!(
            effective
                .origins
                .iter()
                .any(|o| o.origin_type == "group" && o.name.as_deref() == Some(group_name.as_str())),
            "the role did not say it was inherited from the group: {:?}",
            effective.origins
        );
    });
}

/// Ported from cbcore-rs `tests/users_int.rs::an_unknown_role_is_refused_rather_than_dropped`.
///
/// There was no negative-path user test of any kind. The failure this guards
/// against is quiet: a user created with fewer privileges than were asked for
/// looks like a success and fails much later, somewhere else.
#[test]
fn an_unknown_role_is_refused_rather_than_dropped() {
    run_test(async |mut agent| {
        if !agent.supports_feature(&TestFeatureCode::UsersMB69096) {
            return;
        }

        let username = generate_key_with_letter_prefix();
        let user = User::new(
            &username,
            generate_key_with_letter_prefix(),
            vec![Role::new("no_such_role_at_all")],
        )
        .password("password");

        agent
            .upsert_user(&UpsertUserOptions::new(&user, "local"))
            .await
            .expect_err("the server accepted a role it does not have");

        let err = agent
            .get_user(&GetUserOptions::new(&username, "local"))
            .await
            .expect_err("a refused upsert still created the user");

        match err.kind() {
            error::ErrorKind::Mgmt(e) => {
                if let mgmtx::error::ErrorKind::Server(e, ..) = e.kind() {
                    assert_eq!(e.kind(), &mgmtx::error::ServerErrorKind::UserNotFound);
                } else {
                    panic!("expected UserNotFound, got {e:?}");
                }
            }
            _ => panic!("expected UserNotFound, got {err:?}"),
        }
    });
}

/// Ported from cbcore-rs `tests/users_int.rs::the_cluster_publishes_its_role_catalogue`.
///
/// `test_get_all_roles` above asserts the list is non-empty and that its first
/// entry has non-empty fields, which passes whatever the entries are. Which
/// roles exist, and which of them take a bucket target, is a property of the
/// server version rather than something to hard-code — so it is read, and this
/// pins that reading it works.
#[test]
fn the_role_catalogue_says_which_roles_take_a_bucket() {
    run_test(async |mut agent| {
        let roles = agent.get_roles(&GetRolesOptions::new()).await.unwrap();

        assert!(
            roles.iter().any(|r| r.role.name == "admin"),
            "the catalogue did not contain admin"
        );

        let data_reader = roles
            .iter()
            .find(|r| r.role.name == "data_reader")
            .expect("the catalogue did not contain data_reader");

        assert!(
            data_reader.role.bucket.is_some(),
            "data_reader is bucket-scoped and the catalogue should say so: {:?}",
            data_reader.role
        );
        assert!(
            roles
                .iter()
                .find(|r| r.role.name == "admin")
                .is_some_and(|r| r.role.bucket.is_none()),
            "admin is cluster-wide and should carry no bucket"
        );
    });
}

/// **The caller can ask whether it may manage users without naming one.**
///
/// Both answers are exercised, because the allowed case passes on its own
/// whether or not the question is really being asked: a call that always
/// returned `Ok` would look identical. The denial is the detector.
///
/// The impersonated identity carries a password, so it reaches the server as
/// that user's own basic auth and the server's permission check is genuinely
/// what answers -- see the note in `tests/on_behalf_of.rs`.
///
/// Ported from cbcore-rs `src/services/users.rs::may_manage_local_users`, the
/// one entry point of its users service with no counterpart here.
#[test]
fn may_manage_local_users_answers_both_ways() {
    run_test(async |mut agent| {
        if !agent.supports_feature(&TestFeatureCode::UserGroups) {
            return;
        }

        let admin_verdict = agent
            .may_manage_local_users(&MayManageLocalUsersOptions::new())
            .await;
        assert!(
            admin_verdict.is_ok(),
            "the test cluster's administrator should be allowed to manage users, got {admin_verdict:?}"
        );

        let username = generate_key_with_letter_prefix();
        let powerless = User::new(&username, "may-manage probe", vec![Role::new("ro_admin")])
            .password("password");

        agent
            .upsert_user(&UpsertUserOptions::new(&powerless, "local"))
            .await
            .unwrap();

        try_until(
            Instant::now().add(Duration::from_secs(30)),
            Duration::from_millis(500),
            "the probe user did not reach every node in time",
            async || match agent
                .ensure_user(&EnsureUserOptions::new(&username, "local", false))
                .await
            {
                Ok(_) => Ok(Some(())),
                Err(e) => Err(e),
            },
        )
        .await;

        let as_powerless: couchbase_core::httpx::request::OnBehalfOfInfo =
            couchbase_core::on_behalf_of::OnBehalfOfInfo::new(&username)
                .password_or_domain(couchbase_core::on_behalf_of::OboPasswordOrDomain::Password(
                    "password".to_string(),
                ))
                .try_into()
                .expect("an identity with a password should convert");

        let verdict = agent
            .may_manage_local_users(&MayManageLocalUsersOptions::new().on_behalf_of(&as_powerless))
            .await;

        let _ = agent
            .delete_user(&DeleteUserOptions::new(&username, "local"))
            .await;

        match verdict {
            Err(e) => assert!(
                format!("{e}").contains("access denied"),
                "a user without the role should be refused for the reason it lacks, got {e}"
            ),
            Ok(()) => panic!("a user holding only ro_admin should not be allowed to manage users"),
        }
    });
}
