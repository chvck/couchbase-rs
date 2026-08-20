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

//! On-behalf-of: one agent, one set of administrator credentials, and a
//! per-request header that makes the cluster apply *someone else's* rights.
//!
//! Ported from cbcore-rs `tests/on_behalf_of_int.rs`. The mechanism existed
//! here — `OnBehalfOfInfo` and an `on_behalf_of` field on every HTTP service's
//! options — and no test in the crate set it, so nothing said whether the
//! header reached the server or whether the server did anything with it.
//!
//! **The pair of tests is the point.** A denial alone is not evidence the
//! mechanism works: a request that is malformed for any reason also produces a
//! denial. The second test grants the role and asserts the same call through
//! the same option now succeeds, which is what separates "the user is not
//! allowed" from "we broke the request".
//!
//! **Both identity forms, because they travel differently.** The password form
//! becomes basic auth as the impersonated user, so it never sends the header at
//! all; the domain form sends `cb-on-behalf-of` alongside the cluster's own
//! credentials, and is the only one usable when the caller does not know the
//! user's password — which is the case a gateway is in. Running the pair over
//! each form is what makes the header itself the thing under test: the password
//! tests would pass with the header permanently broken, and did.
//!
//! That was DEFECTS #17. The client used to replace the caller's credentials
//! with the header rather than adding the header to them, so a domain-form
//! request arrived unauthenticated and the server answered "Failure to
//! authenticate user". Delegation was modelled as a variant of `Auth`, which
//! left it occupying the slot the credentials needed; it now rides on
//! `Request::on_behalf_of` beside them.
//!
//! **KV is a different mechanism and the tests at the bottom do pin it.** A KV
//! ping carries the username as a memcached framing extra rather than an HTTP
//! header, and takes only the username from the identity, so the password form
//! is beside the point there and DEFECTS #17 does not reach it.

use crate::common::features::TestFeatureCode;
use crate::common::helpers::{generate_key_with_letter_prefix, is_memdx_error, try_until};
use crate::common::test_agent::TestAgent;
use crate::common::test_config::run_test;
use couchbase_core::httpx::request::OnBehalfOfInfo as WireOnBehalfOfInfo;
use couchbase_core::memdx::error::ServerErrorKind;
use couchbase_core::memdx::status::Status;
use couchbase_core::mgmtx::user::{Role, User};
use couchbase_core::on_behalf_of::{OboPasswordOrDomain, OnBehalfOfInfo};
use couchbase_core::options::crud::{GetOptions, UpsertOptions};
use couchbase_core::options::management::{
    DeleteUserOptions, EnsureUserOptions, UpsertUserOptions,
};
use couchbase_core::options::ping::PingOptions;
use couchbase_core::options::query::QueryOptions;
use couchbase_core::options::stats::CollectionStatsOptions;
use couchbase_core::results::pingreport::{EndpointPingReport, PingState};
use couchbase_core::service_type::ServiceType;
use couchbase_core::{error, memdx, queryx};
use futures::StreamExt;
use std::ops::Add;
use std::time::Duration;
use tokio::time::Instant;
use tracing::error;

mod common;

/// The identity to act as, by password.
///
/// The client turns this into basic auth as the impersonated user, so it
/// exercises the option and the server's permission check but never sends the
/// `cb-on-behalf-of` header. [`caller_by_domain`] is the half that does.
///
/// Built through `couchbase_core::on_behalf_of::OnBehalfOfInfo` and converted,
/// rather than constructing the wire type directly. That type and its
/// `TryFrom` are the crate's public spelling of this and nothing inside the
/// crate uses either, so the conversion — including its refusal when neither a
/// password nor a domain is set — had no exercise at all.
fn caller(username: &str) -> WireOnBehalfOfInfo {
    OnBehalfOfInfo::new(username)
        .password_or_domain(OboPasswordOrDomain::Password(USER_PASSWORD.to_string()))
        .try_into()
        .expect("an identity with a password should convert")
}

/// The same identity, by domain — the form that sends the header.
///
/// **This is the form a gateway has to use**, because it never learns the
/// caller's password: the request authenticates as the cluster's own
/// administrator and the header names whose rights to apply. `local` is the
/// domain local users live in.
fn caller_by_domain(username: &str) -> WireOnBehalfOfInfo {
    OnBehalfOfInfo::new(username)
        .password_or_domain(OboPasswordOrDomain::Domain("local".to_string()))
        .try_into()
        .expect("an identity with a domain should convert")
}

const USER_PASSWORD: &str = "password";

async fn create_user(agent: &TestAgent, username: &str, roles: Vec<Role>) {
    let user = User::new(username, "on-behalf-of probe", roles).password(USER_PASSWORD);

    agent
        .upsert_user(&UpsertUserOptions::new(&user, "local"))
        .await
        .unwrap();

    try_until(
        Instant::now().add(Duration::from_secs(30)),
        Duration::from_millis(500),
        "the user did not reach every node in time",
        async || match agent
            .ensure_user(&EnsureUserOptions::new(username, "local", false))
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

async fn delete_user(agent: &TestAgent, username: &str) {
    let _ = agent
        .delete_user(&DeleteUserOptions::new(username, "local"))
        .await;
}

/// The error code the query service reports, whether it arrived as a plain
/// server error or as one carrying resource names.
fn query_error_code(err: &error::Error) -> Option<u32> {
    match err.kind() {
        error::ErrorKind::Query(e) => match e.kind() {
            queryx::error::ErrorKind::Server(e) => Some(e.code()),
            queryx::error::ErrorKind::Resource(e) => Some(e.cause().code()),
            _ => None,
        },
        _ => None,
    }
}

/// Run a statement to completion, folding a mid-stream failure into the result
/// so the caller sees one answer whether the refusal arrived in the headers or
/// in the body.
async fn run(
    agent: &TestAgent,
    statement: &str,
    obo: Option<&WireOnBehalfOfInfo>,
) -> Result<usize, error::Error> {
    let mut opts = QueryOptions::default().statement(statement.to_string());
    if let Some(obo) = obo {
        opts = opts.on_behalf_of(Some(obo.clone()));
    }

    let mut stream = agent.query(opts).await?;

    let mut rows = 0;
    while let Some(row) = stream.next().await {
        row?;
        rows += 1;
    }

    Ok(rows)
}

/// The refusal, for one identity form.
///
/// `ro_admin` can read the cluster's configuration and nothing in a bucket, so
/// a SELECT run as that user must be refused even though the agent itself holds
/// administrator credentials. 13014 is the query service's code for it.
async fn refused_without_the_role(agent: &TestAgent, obo_for: fn(&str) -> WireOnBehalfOfInfo) {
    let username = generate_key_with_letter_prefix();
    let bucket = agent.test_setup_config.bucket.clone();
    let statement = format!("SELECT RAW 1 FROM `{bucket}` LIMIT 1");

    create_user(agent, &username, vec![Role::new("ro_admin")]).await;

    // The control: the administrator running the same statement on the same
    // agent. If this fails, the statement is wrong, not the header.
    run(agent, &statement, None)
        .await
        .expect("the administrator could not run the statement");

    let obo = obo_for(&username);
    let code = try_until(
        Instant::now().add(Duration::from_secs(30)),
        Duration::from_millis(500),
        "the statement was never refused for a user with no data access",
        async || {
            Ok(run(agent, &statement, Some(&obo))
                .await
                .err()
                .and_then(|e| query_error_code(&e)))
        },
    )
    .await;

    delete_user(agent, &username).await;

    assert_eq!(
        13014, code,
        "expected an authorisation refusal, got query error {code}"
    );
}

/// The grant, for one identity form.
///
/// The control for the refusal above. Without it, an identity that broke every
/// request would look exactly like one the server was correctly refusing —
/// which is precisely how the domain form passed for being untested rather than
/// broken.
async fn allowed_with_the_role(agent: &TestAgent, obo_for: fn(&str) -> WireOnBehalfOfInfo) {
    let username = generate_key_with_letter_prefix();
    let bucket = agent.test_setup_config.bucket.clone();
    let statement = format!("SELECT RAW 1 FROM `{bucket}` LIMIT 1");

    create_user(
        agent,
        &username,
        vec![
            Role::new("query_select").bucket(&bucket),
            Role::new("query_use_sequential_scans").bucket(&bucket),
        ],
    )
    .await;

    let obo = obo_for(&username);
    try_until(
        Instant::now().add(Duration::from_secs(30)),
        Duration::from_millis(500),
        "the statement was never allowed for a user holding the role",
        async || match run(agent, &statement, Some(&obo)).await {
            Ok(rows) => Ok(Some(rows)),
            Err(e) => {
                error!("on-behalf-of query refused: {e}");
                Ok(None)
            }
        },
    )
    .await;

    delete_user(agent, &username).await;
}

/// Ported from cbcore-rs
/// `a_statement_run_on_behalf_of_a_user_gets_that_users_permissions`.
#[test]
fn a_statement_run_on_behalf_of_a_user_gets_that_users_permissions() {
    run_test(async |mut agent| {
        if !agent.supports_feature(&TestFeatureCode::UsersMB69096) {
            return;
        }
        refused_without_the_role(&agent, caller).await;
    });
}

/// Ported from cbcore-rs `a_user_with_the_role_is_allowed_through_the_same_header`.
///
/// The control for the test above.
#[test]
fn a_user_with_the_role_is_allowed_through_the_same_header() {
    run_test(async |mut agent| {
        if !agent.supports_feature(&TestFeatureCode::UsersMB69096) {
            return;
        }
        allowed_with_the_role(&agent, caller).await;
    });
}

/// **The header form, refused.** The pair below is the one that was missing:
/// the password tests above stay green whether `cb-on-behalf-of` works or not,
/// because they never send it.
#[test]
fn a_statement_run_on_behalf_of_a_domain_user_gets_that_users_permissions() {
    run_test(async |mut agent| {
        if !agent.supports_feature(&TestFeatureCode::UsersMB69096) {
            return;
        }
        refused_without_the_role(&agent, caller_by_domain).await;
    });
}

/// **The header form, allowed** — and the one that pins DEFECTS #17 fixed.
///
/// Both domain tests fail against the old behaviour, and they fail differently,
/// which is worth keeping straight. The request went out carrying the header and
/// no credentials at all, so the server rejected it before reaching any
/// permission check: the refusal above saw 2120, an *authentication* failure,
/// where it expects 13014, an authorisation one, and this test simply never got
/// its rows. Measured by reverting the fix and re-running, both against 8.0.3.
#[test]
fn a_domain_user_with_the_role_is_allowed_through_the_header() {
    run_test(async |mut agent| {
        if !agent.supports_feature(&TestFeatureCode::UsersMB69096) {
            return;
        }
        allowed_with_the_role(&agent, caller_by_domain).await;
    });
}

/// Ported from cbcore-rs `a_failure_after_the_response_starts_reaches_the_caller`.
///
/// The query service answers **200**, then declares failure in the body. A
/// caller that only counts rows cannot tell that from an empty answer, which
/// is the whole hazard.
///
/// The unit tests in `queryx::query_respreader` pin this against a captured
/// body; this is the same behaviour against a server that really produces one,
/// which is the only way to know the captured shape is the shape. Measured
/// against 8.0.3: HTTP 200, `"results": []`, then
/// `errors[0].code == 5010` and `"status": "fatal"` — the fixture exactly.
#[test]
fn a_failure_after_the_response_starts_reaches_the_caller() {
    run_test(async |mut agent| {
        // Large enough that the service starts answering and then runs out of
        // room, rather than refusing up front.
        let statement = "SELECT RAW x FROM ARRAY_RANGE(0, 3000000) AS x";

        let res = run(&agent, statement, None).await;

        let err = res.expect_err("a query that cannot finish reported success");
        assert!(
            query_error_code(&err).is_some(),
            "expected a query error from the server, got {err:?}"
        );
    });
}

/// An identity with neither a password nor a domain cannot be sent, and says so
/// rather than producing a header the server reads as somebody else.
#[test]
fn an_identity_with_no_password_and_no_domain_is_refused() {
    let attempt: Result<WireOnBehalfOfInfo, _> = OnBehalfOfInfo::new("someone").try_into();
    assert!(attempt.is_err(), "an empty identity converted");
}

// ---------------------------------------------------------------------------
// KV
// ---------------------------------------------------------------------------

/// A username short enough to travel in a KV framing extra.
///
/// `extframe::append_ext_frame` refuses a frame body of thirty bytes or more,
/// and `generate_key_with_letter_prefix` produces exactly thirty characters --
/// so the HTTP tests above can use it directly and these cannot. Sixteen random
/// alphanumerics are still unique enough for a user nobody else will hold.
///
/// A separate defect, reported with this change: the protocol's extended
/// length is one byte added to fifteen, so a body of up to 270 bytes
/// is encodable, and the encoder's second fifteen-byte ceiling is its own --
/// matching gocbcore rather than the wire format. It bites every on-behalf-of
/// KV operation, not just the ping.
/// A username long enough to need the framing extra's length escape.
///
/// It used to be truncated to sixteen characters, because the encoder capped an
/// extras body at 29 bytes where the wire form and this crate's own decoder both
/// allow 270 -- so the full thirty-character generated name failed to encode and
/// never reached the server at all. That cap is gone, and the full name being
/// used here is what proves it: this is the escape-byte path, live.
fn short_username() -> String {
    generate_key_with_letter_prefix()
}

/// Ping the KV service alone, and answer with one report per endpoint.
///
/// `service_types` is narrowed to `MEMD` deliberately: query and search reach
/// the same identity down a different road, and a report that mixed them could
/// not say which one refused.
async fn ping_kv(agent: &TestAgent, obo: Option<&WireOnBehalfOfInfo>) -> Vec<EndpointPingReport> {
    let mut opts = PingOptions::new()
        .service_types(vec![ServiceType::MEMD])
        .kv_timeout(Duration::from_secs(5));
    if let Some(obo) = obo {
        opts = opts.on_behalf_of(Some(obo.clone()));
    }

    let mut report = agent
        .ping(&opts)
        .await
        .expect("the KV ping failed outright");

    report
        .services
        .remove(&ServiceType::MEMD)
        .expect("a KV ping produced no KV report")
}

/// **The KV path really does carry the identity.**
///
/// Unlike the HTTP services above, KV sends the username as a memcached
/// framing extra -- `caller()`'s password is inert here, because
/// `DiagnosticsComponent` takes only `username` from the identity. That frame
/// used to be dropped on the floor: `OpsUtil::ping` named its request
/// `_request` and ignored it, so a ping "as another user" ran as the
/// connection's own user and reported `Ok`. Nothing else in the crate sends a
/// KV framing extra for a user, so this was the whole of KV on-behalf-of.
///
/// A username that is not a user is what makes the two cases distinguishable.
/// A `NOOP` needs no privilege, so an impersonated user with no rights at all
/// is still allowed to ping -- see the test below, which relies on exactly
/// that. What the server refuses is an identity it cannot resolve, and it
/// names the identity in the refusal, which is the part that says our bytes
/// arrived: measured against 8.0.3, status `AccessError` (0x24) with context
/// `User "<name>" is not a Couchbase user`.
#[test]
fn a_kv_ping_on_behalf_of_a_user_who_does_not_exist_is_refused() {
    run_test(async |agent| {
        // The control: the administrator's own ping, on the same connections.
        // If this is not Ok, the cluster is unwell and the assertion below
        // would mean nothing.
        for report in ping_kv(&agent, None).await {
            assert_eq!(
                report.state,
                PingState::Ok,
                "a ping with no identity failed: {report:?}"
            );
        }

        let username = short_username();
        let obo = caller(&username);

        let reports = ping_kv(&agent, Some(&obo)).await;
        assert!(!reports.is_empty(), "the KV ping reported no endpoints");

        for report in reports {
            assert_eq!(
                report.state,
                PingState::Error,
                "a ping as a user that does not exist was reported as {:?}",
                report.state
            );

            let err = report.error.expect("a failed ping reported no error");
            let memdx_err =
                is_memdx_error(&err).unwrap_or_else(|| panic!("not a KV error: {err:?}"));
            let server_err = match memdx_err.kind() {
                memdx::error::ErrorKind::Server(e) => e,
                other => panic!("expected a refusal from the server, got {other:?}"),
            };

            assert_eq!(
                server_err.status(),
                Status::AccessError,
                "expected an access error, got {server_err:?}"
            );

            let context = server_err
                .context()
                .and_then(|c| memdx::error::ServerError::parse_context(c))
                .and_then(|c| c.text)
                .unwrap_or_default();
            assert!(
                context.contains(&username),
                "the refusal did not name the user we asked for: {context:?}"
            );
        }
    });
}

/// The control for the test above: a user that exists is let through the same
/// frame, so a refusal there is the server judging the identity rather than
/// the request being broken.
///
/// The role is `ro_admin`, which cannot touch a document -- and the ping
/// succeeds anyway, because a `NOOP` needs no privilege. That is why the test
/// above impersonates a name that is not a user: it is the only KV denial a
/// ping can provoke.
#[test]
fn a_kv_ping_on_behalf_of_a_user_that_exists_is_allowed() {
    run_test(async |mut agent| {
        if !agent.supports_feature(&TestFeatureCode::UsersMB69096) {
            return;
        }

        let username = short_username();
        create_user(&agent, &username, vec![Role::new("ro_admin")]).await;

        let obo = caller(&username);
        try_until(
            Instant::now().add(Duration::from_secs(30)),
            Duration::from_millis(500),
            "the ping was never allowed for a user that exists",
            async || {
                let reports = ping_kv(&agent, Some(&obo)).await;
                if !reports.is_empty() && reports.iter().all(|r| r.state == PingState::Ok) {
                    return Ok(Some(()));
                }
                error!("a ping on behalf of an existing user was refused: {reports:?}");
                Ok(None)
            },
        )
        .await;

        delete_user(&agent, &username).await;
    });
}

// ---------------------------------------------------------------------------
// KV data operations
// ---------------------------------------------------------------------------

/// **A write on behalf of a user who may not write must be refused.**
///
/// The pair below is deliberately the *unprivileged* direction, because the
/// privileged one proves nothing: the request already carries the cluster's own
/// administrator credentials, so an upsert on behalf of anybody succeeds if the
/// identity is silently dropped. Only a caller who should be refused can tell
/// "the server applied their permissions" from "the server never heard of them".
///
/// That is not hypothetical. The crud options carried no identity at all until
/// this test was written — `CrudComponent` passed `on_behalf_of: None` at every
/// request site while memdx had encoded the frame all along — so every KV
/// operation ran with the agent's own rights. A gateway impersonating its callers
/// would have had their writes succeed regardless of what they were allowed to
/// do, which is the one failure direction worth a test of its own.
#[test]
fn a_write_on_behalf_of_a_user_without_the_role_is_refused() {
    run_test(async |mut agent| {
        if !agent.supports_feature(&TestFeatureCode::UsersMB69096) {
            return;
        }

        let username = generate_key_with_letter_prefix();
        let bucket = agent.test_setup_config.bucket.clone();
        let key = generate_key_with_letter_prefix();

        // `ro_admin` can read the cluster's configuration and nothing in a
        // bucket, so it may not write this document.
        create_user(&agent, &username, vec![Role::new("ro_admin")]).await;

        // The control: the administrator's own write, same agent, same key. If
        // this fails the assertion below would mean nothing.
        agent
            .upsert(UpsertOptions::new(
                key.as_bytes(),
                "_default",
                "_default",
                b"{}",
            ))
            .await
            .expect("the administrator could not write the document");

        let obo = caller(&username);
        let refused = try_until(
            Instant::now().add(Duration::from_secs(30)),
            Duration::from_millis(500),
            "the write was never refused for a user with no data access",
            async || {
                Ok(agent
                    .upsert(
                        UpsertOptions::new(key.as_bytes(), "_default", "_default", b"{}")
                            .on_behalf_of(Some(&obo)),
                    )
                    .await
                    .err())
            },
        )
        .await;

        delete_user(&agent, &username).await;

        // `Access` rather than the raw status, and via `is_server_error_kind`
        // rather than a bare match: the crate re-wraps some server errors as
        // `Resource` to carry the keyspace names, and that predicate is the one
        // that covers both arms.
        let memdx = is_memdx_error(&refused).unwrap_or_else(|| {
            panic!("expected a memdx error, got {refused}");
        });
        assert!(
            memdx.is_server_error_kind(ServerErrorKind::Access),
            "expected an access refusal, got {refused}"
        );
    });
}

/// The control for the test above: the same call, on behalf of a user who *may*
/// write, is allowed. Without it, an identity that broke every request would look
/// exactly like one the server was correctly refusing.
#[test]
fn a_write_on_behalf_of_a_user_with_the_role_is_allowed() {
    run_test(async |mut agent| {
        if !agent.supports_feature(&TestFeatureCode::UsersMB69096) {
            return;
        }

        let username = generate_key_with_letter_prefix();
        let bucket = agent.test_setup_config.bucket.clone();
        let key = generate_key_with_letter_prefix();

        create_user(
            &agent,
            &username,
            vec![Role::new("data_writer").bucket(&bucket)],
        )
        .await;

        let obo = caller(&username);
        try_until(
            Instant::now().add(Duration::from_secs(30)),
            Duration::from_millis(500),
            "the write was never allowed for a user holding the role",
            async || match agent
                .upsert(
                    UpsertOptions::new(key.as_bytes(), "_default", "_default", b"{}")
                        .on_behalf_of(Some(&obo)),
                )
                .await
            {
                Ok(res) => Ok(Some(res)),
                Err(e) => {
                    error!("on-behalf-of write refused: {e}");
                    Ok(None)
                }
            },
        )
        .await;

        delete_user(&agent, &username).await;
    });
}

/// **A collection's stats are a read of the collection**, and this is the pair
/// that says so.
///
/// `CollectionStatsOptions` carried no identity while `StatsRequest` had encoded
/// the frame all along — the same gap A12 closed for the crud options, missed
/// here because stats live in their own options module. It matters more than a
/// missing identity usually does: a client that already holds the keyspace name
/// in its own map has *nothing else* consulting the server about permission, so
/// without this a gateway answering `collStats` for a caller answers with its
/// own rights and reports a document count the caller may not see.
///
/// **The refusal is proven to be about permission, and that took some care.** A
/// user the KV nodes have not heard of yet is refused with the *same*
/// `ServerErrorKind::Access` as one who is known and not permitted — only the
/// context string differs ("is not a Couchbase user") — so asserting on the kind
/// alone would pass with the identity never reaching the server at all. The wait
/// below therefore retries *the operation under test* until the answer is a
/// permission one, which is also the only wait that can be right here: this call
/// sweeps **every** KV node, so a user known to one of them is not enough. An
/// earlier version waited on a `get` instead; it passed alone and failed in the
/// full suite, when the sweep reached a node that had not caught up.
///
/// The role grants document reads and not stats, which is a sharper statement
/// than no access at all: the same user is permitted one KV operation and
/// refused another, so the server is demonstrably applying *their* rights.
/// Measured while writing this against 8.0.3 — `data_monitoring` grants the
/// stats read and `data_reader` does not.
#[test]
fn collection_stats_on_behalf_of_a_user_without_the_role_is_refused() {
    run_test(async |mut agent| {
        if !agent.supports_feature(&TestFeatureCode::UsersMB69096) {
            return;
        }

        let username = generate_key_with_letter_prefix();
        let bucket = agent.test_setup_config.bucket.clone();

        create_user(
            &agent,
            &username,
            vec![Role::new("data_reader").bucket(&bucket)],
        )
        .await;

        // The control: the administrator's own read of the same collection. A
        // refusal below means nothing if this cannot succeed.
        agent
            .collection_stats(CollectionStatsOptions::new("_default", "_default"))
            .await
            .expect("the administrator could not read the collection's stats");

        let obo = caller(&username);
        let refused = try_until(
            Instant::now().add(Duration::from_secs(30)),
            Duration::from_millis(500),
            "the stats read was never refused on permission grounds",
            async || {
                match agent
                    .collection_stats(
                        CollectionStatsOptions::new("_default", "_default")
                            .on_behalf_of(Some(&obo)),
                    )
                    .await
                {
                    // Allowed: either the identity was dropped or the role does
                    // grant this. Keep waiting, so a failure names the wait
                    // rather than asserting on a kind that was never reached.
                    Ok(_) => Ok(None),
                    // A node that has not learned the user yet, and this call has
                    // to satisfy all of them.
                    Err(e) if e.to_string().contains("is not a Couchbase user") => Ok(None),
                    Err(e) => Ok(Some(e)),
                }
            },
        )
        .await;

        delete_user(&agent, &username).await;

        // `Access` rather than the raw status, and via `is_server_error_kind`
        // rather than a bare match, for the reason the write pair above gives.
        // The kind only arrives because `OpsCore::decode_error` learned this
        // status alongside this test: `STAT` is a core operation, and core
        // operations reported every refusal as `UnknownStatus` before that.
        let memdx = is_memdx_error(&refused).unwrap_or_else(|| {
            panic!("expected a memdx error, got {refused}");
        });
        assert!(
            memdx.is_server_error_kind(ServerErrorKind::Access),
            "expected an access refusal, got {refused}"
        );
    });
}

/// The control: the same read on behalf of a user who holds the stats role.
///
/// Without it, an identity that broke every `STAT` request would be
/// indistinguishable from one the server was correctly refusing.
#[test]
fn collection_stats_on_behalf_of_a_user_with_the_role_is_allowed() {
    run_test(async |mut agent| {
        if !agent.supports_feature(&TestFeatureCode::UsersMB69096) {
            return;
        }

        let username = generate_key_with_letter_prefix();
        let bucket = agent.test_setup_config.bucket.clone();

        create_user(
            &agent,
            &username,
            vec![Role::new("data_monitoring").bucket(&bucket)],
        )
        .await;

        let obo = caller(&username);
        try_until(
            Instant::now().add(Duration::from_secs(30)),
            Duration::from_millis(500),
            "the stats read was never allowed for a user holding the role",
            async || match agent
                .collection_stats(
                    CollectionStatsOptions::new("_default", "_default").on_behalf_of(Some(&obo)),
                )
                .await
            {
                Ok(res) => Ok(Some(res)),
                Err(e) => {
                    error!("on-behalf-of stats read refused: {e}");
                    Ok(None)
                }
            },
        )
        .await;

        delete_user(&agent, &username).await;
    });
}
