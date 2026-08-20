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

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, RwLock};

use crate::httpx::client::Client;
use crate::queryx::error;
use crate::queryx::error::Error;
use crate::queryx::query::Query;
use crate::queryx::query_options::QueryOptions;
use crate::queryx::query_respreader::QueryRespReader;

/// Independent shards the statements are spread across.
///
/// Every prepared query reads this cache, so one lock over one map makes each
/// of them wait on all the others. Sixteen read-write shards is the structure
/// cbcore-rs arrived at, and it is the one ported here.
const SHARD_COUNT: usize = 16;

/// Entries kept per shard, so `SHARD_COUNT * MAX_ENTRIES_PER_SHARD` in total.
///
/// The cache had no bound at all, which leaks against any workload that
/// generates statement text — an inlined literal, a rotating keyspace name —
/// because every distinct string became a permanent entry. This is a leak-stop
/// rather than a tuned working set: 4096 distinct statements is far more than an
/// application prepares, so a cache this size only ever evicts under abuse.
const MAX_ENTRIES_PER_SHARD: usize = 256;

#[derive(Debug)]
pub struct PreparedStatementCache {
    shards: [RwLock<HashMap<String, String>>; SHARD_COUNT],
}

impl Default for PreparedStatementCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PreparedStatementCache {
    pub fn new() -> Self {
        Self {
            shards: std::array::from_fn(|_| RwLock::new(HashMap::new())),
        }
    }

    fn shard(&self, statement: &str) -> &RwLock<HashMap<String, String>> {
        let mut hasher = DefaultHasher::new();
        statement.hash(&mut hasher);
        &self.shards[(hasher.finish() as usize) % SHARD_COUNT]
    }

    pub fn get(&self, statement: &str) -> Option<String> {
        self.shard(statement)
            .read()
            .unwrap()
            .get(statement)
            .cloned()
    }

    pub fn put(&self, statement: &str, prepared_name: &str) {
        let mut shard = self.shard(statement).write().unwrap();

        if shard.len() >= MAX_ENTRIES_PER_SHARD && !shard.contains_key(statement) {
            // An arbitrary victim, not the least recently used one: losing an
            // entry costs one extra PREPARE round trip, which is not worth the
            // per-lookup bookkeeping an LRU would add to the hot path.
            let victim = shard.keys().next().cloned();
            if let Some(victim) = victim {
                shard.remove(&victim);
            }
        }

        shard.insert(statement.to_string(), prepared_name.to_string());
    }
}

pub struct PreparedQuery<C: Client> {
    pub executor: Query<C>,
    pub cache: Arc<PreparedStatementCache>,
}

impl<C: Client> PreparedQuery<C> {
    pub async fn prepared_query(&self, opts: &QueryOptions) -> error::Result<QueryRespReader> {
        // We need to clone the options so that we can modify it with any cached statement.
        let mut opts = (*opts).clone();

        if let Some(ae) = opts.auto_execute {
            // If this is already marked as auto-execute, we just pass it through
            if ae {
                return self.executor.query(&opts).await;
            }
        }

        let statement = if let Some(statement) = opts.statement {
            statement
        } else {
            return Err(Error::new_invalid_argument_error(
                "statement must be present if auto_execute is true",
                Some("statement".to_string()),
            ));
        };

        if let Some(cached_statement) = self.cache.get(&statement) {
            opts.statement = None;
            opts.prepared = Some(cached_statement);

            match self.executor.query(&opts).await {
                Ok(reader) => return Ok(reader),
                Err(e) => {
                    // Only a prepared-statement failure says the cached plan is
                    // the problem. Any other error is the real answer to this
                    // query — an authentication failure, a syntax error, a dead
                    // connection — and re-issuing it as a PREPARE would hide it
                    // behind whatever the second request happened to return.
                    if !e.is_prepared_statement_failure() {
                        return Err(e);
                    }
                }
            }

            // Otherwise fall through and prepare it again. The plan name has to
            // go, or the request would carry both a prepared name and a
            // statement and leave the server to choose.
            opts.prepared = None;
        }

        opts.statement = Some(format!("PREPARE {statement}"));
        opts.auto_execute = Some(true);

        let res = self.executor.query(&opts).await?;

        let early_metadata = res.early_metadata();
        if let Some(prepared) = &early_metadata.prepared {
            self.cache.put(&statement, prepared);
        }

        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::httpx::error::Result as HttpxResult;
    use crate::httpx::request::{Auth, BasicAuth, Request};
    use crate::httpx::response::Response;
    use crate::tracingcomponent::{TracingComponent, TracingComponentConfig};
    use async_trait::async_trait;
    use bytes::Bytes;
    use std::sync::Mutex;

    /// Answers each request with the next canned response and records the bodies
    /// it was asked to send, so a test can assert how many requests were issued
    /// and what was in them.
    struct ScriptedClient {
        responses: Mutex<Vec<(u16, &'static str)>>,
        requests: Mutex<Vec<Bytes>>,
    }

    impl ScriptedClient {
        fn new(responses: Vec<(u16, &'static str)>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses),
                requests: Mutex::new(Vec::new()),
            })
        }

        fn request_bodies(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|b| String::from_utf8_lossy(b).to_string())
                .collect()
        }
    }

    #[async_trait]
    impl Client for ScriptedClient {
        async fn execute(&self, req: Request) -> HttpxResult<Response> {
            self.requests
                .lock()
                .unwrap()
                .push(req.body.clone().unwrap_or_default());

            let (status, body) = self.responses.lock().unwrap().remove(0);

            let resp = http::Response::builder()
                .status(status)
                .body(body.to_string())
                .unwrap();

            Ok(Response::from(reqwest::Response::from(resp)))
        }
    }

    fn prepared_query(client: Arc<ScriptedClient>) -> PreparedQuery<ScriptedClient> {
        PreparedQuery {
            executor: Query {
                http_client: client,
                user_agent: "test".to_string(),
                endpoint: "http://localhost:8093".to_string(),
                canonical_endpoint: "http://localhost:8093".to_string(),
                auth: Auth::BasicAuth(BasicAuth::new("user", "pass")),
                tracing: Arc::new(TracingComponent::new(TracingComponentConfig {
                    cluster_labels: None,
                })),
            },
            cache: Arc::new(PreparedStatementCache::default()),
        }
    }

    const AUTH_FAILURE: &str = r#"{"errors":[{"code":10000,"msg":"authentication failure"}]}"#;
    const PLAN_NOT_FOUND: &str =
        r#"{"errors":[{"code":4040,"msg":"prepared statement not found"}]}"#;
    const SUCCESS: &str = r#"{"requestID":"1","prepared":"[123]plan","results":[{"a":1}],"status":"success","metrics":{}}"#;

    #[tokio::test]
    async fn a_non_plan_error_on_a_cache_hit_reaches_the_caller() {
        let client = ScriptedClient::new(vec![(401, AUTH_FAILURE)]);
        let query = prepared_query(client.clone());
        query.cache.put("SELECT 1=1", "[123]plan");

        let opts = QueryOptions::new().statement("SELECT 1=1".to_string());
        let err = query.prepared_query(&opts).await.err().unwrap();

        assert!(
            err.is_authentication_failure(),
            "expected the authentication failure to propagate, got {err}"
        );

        let bodies = client.request_bodies();
        assert_eq!(
            1,
            bodies.len(),
            "the failure was re-issued as a prepare: {bodies:?}"
        );
        assert!(bodies[0].contains("[123]plan"));
    }

    #[tokio::test]
    async fn a_plan_failure_on_a_cache_hit_re_prepares() {
        let client = ScriptedClient::new(vec![(404, PLAN_NOT_FOUND), (200, SUCCESS)]);
        let query = prepared_query(client.clone());
        query.cache.put("SELECT 1=1", "[123]stale");

        let opts = QueryOptions::new().statement("SELECT 1=1".to_string());
        query.prepared_query(&opts).await.unwrap();

        let bodies = client.request_bodies();
        assert_eq!(2, bodies.len(), "expected a re-prepare, got {bodies:?}");
        assert!(bodies[0].contains("[123]stale"));
        assert!(bodies[1].contains("PREPARE SELECT 1=1"));
        assert!(
            !bodies[1].contains("[123]stale"),
            "the prepare still carried the stale plan name: {}",
            bodies[1]
        );

        // The new plan name replaced the stale one.
        assert_eq!(Some("[123]plan".to_string()), query.cache.get("SELECT 1=1"));
    }

    #[test]
    fn the_cache_is_bounded() {
        let cache = PreparedStatementCache::default();

        let total = SHARD_COUNT * MAX_ENTRIES_PER_SHARD;
        for i in 0..total * 4 {
            cache.put(&format!("statement {i}"), &format!("plan {i}"));
        }

        let held: usize = cache
            .shards
            .iter()
            .map(|shard| shard.read().unwrap().len())
            .sum();

        assert!(held <= total, "cache held {held} entries, bound is {total}");
        for shard in &cache.shards {
            assert!(shard.read().unwrap().len() <= MAX_ENTRIES_PER_SHARD);
        }
    }
}
