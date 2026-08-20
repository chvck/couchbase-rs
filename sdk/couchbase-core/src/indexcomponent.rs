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

//! Reading a secondary index, from the agent's side of it.
//!
//! Three layers meet here and each keeps its own job:
//! [`indexerx`](crate::indexerx) speaks the queryport protocol,
//! [`indexrouter`](crate::indexrouter) decides which indexer holds what, and
//! this opens the connections and owns the one piece of policy neither of them
//! can: **when to go and re-read the index topology.**
//!
//! ### The two ports, which are not interchangeable
//!
//! The indexing service answers on two, and this component talks to both. The
//! REST port (`indexHttp`, 9102) answers `/getIndexStatus` over HTTP and is
//! reached through [`HttpComponent`] like every other REST surface in this
//! crate. The queryport (`indexScan`, 9101) speaks the protobuf-over-framing
//! protocol in [`indexerx`](crate::indexerx) and is dialled directly. A
//! connection to one is useless to the other.
//!
//! ### When the topology is re-read
//!
//! Three moments, and no polling:
//!
//! - **Before the first scan**, because there is nothing to route against yet.
//! - **After a cluster config changed the index nodes**, which
//!   [`IndexRouter::set_nodes`] notices from `reconfigure`. A rebalance or a
//!   failover is exactly what moves partitions, and it is also exactly what
//!   bumps the config — so the config watcher is the signal, and nothing here
//!   has a timer.
//! - **Once after a failure that a fresher topology could fix**, which
//!   [`worth_refreshing`] decides from the error's type.
//!
//! **The retry is before the first entry, and only there.** Routing and opening
//! every host happen before the caller has read anything, so re-issuing them
//! repeats nothing. Once entries are flowing, one host's rows have already gone
//! out and there is no honest way to start that host again — a failure then is
//! the caller's to see.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::debug;
use uuid::Uuid;

use crate::address::Address;
use crate::authenticator::Authenticator;
use crate::componentconfigs::NetworkAndCanonicalEndpoint;
use crate::error::{Error, ErrorKind, Result};
use crate::httpcomponent::{auth_from_authenticator, HttpComponent, HttpComponentState};
use crate::httpx::client::Client;
use crate::httpx::request::Auth;
use crate::indexerclient_provider::{ClientPool, PoolOptions};
use crate::indexerx::proto::{Consistency, ScanOptions, ScanVector, ScanVectorEntry};
use crate::indexerx::status::Indexing;
use crate::indexerx::ConnectOptions;
use crate::indexrouter::{worth_refreshing, IndexRef, IndexRouter, NodeMap, Route};
use crate::options::index::{IndexScanConsistency, IndexScanOptions};
use crate::results::index_scan::{IndexScanResults, IndexScanStream};
use crate::service_type::ServiceType;
use crate::tls_config::TlsConfig;

pub(crate) struct IndexComponentConfig {
    /// The indexing service's **REST** endpoints, for `/getIndexStatus`.
    pub endpoints: HashMap<String, NetworkAndCanonicalEndpoint>,
    /// Every node's **queryport**, keyed the way `/getIndexStatus` names nodes.
    pub nodes: NodeMap,
    pub authenticator: Authenticator,
    pub tls_config: Option<TlsConfig>,
}

pub(crate) struct IndexComponentOptions {
    pub id: String,
    pub user_agent: String,
    /// The agent's, because a socket to a queryport is a socket like any other.
    /// The *connect* timeout is not here: it is
    /// [`indexerx`](crate::indexerx)'s own default, which is deliberately
    /// longer than the data service's, and there is nothing on
    /// [`AgentOptions`](crate::options::agent::AgentOptions) to override it
    /// with yet.
    pub tcp_keep_alive_time: Duration,
}

struct IndexComponentState {
    nodes: NodeMap,
    authenticator: Authenticator,
    tls_config: Option<TlsConfig>,
}

pub(crate) struct IndexComponent<C: Client> {
    id: String,
    http_component: HttpComponent<C>,
    router: IndexRouter,
    state: Mutex<IndexComponentState>,
    /// Held across the `/getIndexStatus` call so that a hundred scans meeting a
    /// stale topology at once make one request and not a hundred.
    refreshing: tokio::sync::Mutex<()>,
    /// One pool of queryport connections per scan endpoint.
    ///
    /// **A scan owns its connection until its stream ends**, so without this
    /// every route of every scan dials and closes a socket. On the perf lab that
    /// was the whole difference: connecting per route cost 7-9% of scans to
    /// client timeouts and left 45,869 sockets in `TIME_WAIT` on the indexer.
    pools: Mutex<HashMap<Address, Arc<ClientPool>>>,
    tcp_keep_alive_time: Duration,
}

impl<C: Client + 'static> IndexComponent<C> {
    pub fn new(
        http_client: Arc<C>,
        config: IndexComponentConfig,
        opts: IndexComponentOptions,
    ) -> Self {
        Self {
            id: opts.id,
            http_component: HttpComponent::new(
                ServiceType::INDEX,
                opts.user_agent,
                http_client,
                HttpComponentState::new(config.endpoints, config.authenticator.clone()),
            ),
            router: IndexRouter::new(config.nodes.clone()),
            state: Mutex::new(IndexComponentState {
                nodes: config.nodes,
                authenticator: config.authenticator,
                tls_config: config.tls_config,
            }),
            refreshing: tokio::sync::Mutex::new(()),
            pools: Mutex::new(HashMap::new()),
            tcp_keep_alive_time: opts.tcp_keep_alive_time,
        }
    }

    pub fn reconfigure(&self, config: IndexComponentConfig) {
        debug!(
            "Index component {} updating endpoints to {:?}, {} indexer node(s)",
            self.id,
            &config.endpoints.keys().collect::<Vec<_>>(),
            config.nodes.len(),
        );

        self.http_component.reconfigure(HttpComponentState::new(
            config.endpoints,
            config.authenticator.clone(),
        ));

        {
            let mut state = self.state.lock().unwrap();
            state.nodes = config.nodes.clone();
            state.authenticator = config.authenticator;
            state.tls_config = config.tls_config;
        }

        // Last, and deliberately: whatever the router now says about placement
        // is answered against the node map this config brought.
        self.router.set_nodes(config.nodes);

        // **The pools are deliberately left alone here.** This runs on every
        // accepted config revision, and a rebalance produces a stream of them —
        // dropping every connection cluster-wide on each one is exactly the
        // churn the pool exists to remove, arriving precisely when the topology
        // is already moving. Nothing a config revision carries can change what a
        // queryport connection authenticated with: `Agent::reconfigure` is the
        // only writer of the credentials and TLS that `connect_options` is built
        // from, and it calls [`drain_pools`](Self::drain_pools) itself.
    }

    /// Throw away every pooled connection, because what they authenticated with
    /// has changed.
    ///
    /// **Called from the credential path and nowhere else.** Draining marks
    /// every idle connection for close and every in-flight lease for discard on
    /// release, so a rotation reaches the wire at the next scan rather than
    /// whenever a connection happens to age out. A scan already streaming keeps
    /// running: it is still a valid scan, and killing it mid-read would turn a
    /// rotation into a user-visible error.
    ///
    /// The pools are dropped as well as drained, because a pool's
    /// [`ConnectOptions`] are fixed when it is built — one left in the map would
    /// go on dialling with the credentials that just changed. The next scan
    /// builds a fresh one. Leases still open keep the old pool alive until they
    /// end, so a host can transiently hold both pools' connections; that is
    /// bounded by the scans in flight at the moment an operator rotates, which
    /// is rare and is not a steady state.
    pub fn drain_pools(&self) {
        let pools = std::mem::take(&mut *self.pools.lock().unwrap());
        for pool in pools.into_values() {
            pool.drain();
        }
    }

    /// Read an index, scattered over every host that holds part of it.
    pub async fn scan(
        &self,
        bucket_name: &str,
        opts: &IndexScanOptions<'_>,
    ) -> Result<IndexScanResults> {
        let index = IndexRef::new(
            bucket_name,
            opts.scope_name,
            opts.collection_name,
            opts.index_name,
        );
        let consistency = consistency_from(&opts.consistency)?;
        let request_id = opts
            .request_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        if self.router.needs_refresh() {
            self.refresh(opts).await?;
        }

        match self.try_scan(&index, opts, &consistency, &request_id).await {
            Err(e) if worth_refreshing(&e) => {
                debug!(
                    "Index component {} refreshing topology after {e}, then retrying {index}",
                    self.id
                );
                self.refresh(opts).await?;
                self.try_scan(&index, opts, &consistency, &request_id).await
            }
            other => other,
        }
    }

    async fn try_scan(
        &self,
        index: &IndexRef,
        opts: &IndexScanOptions<'_>,
        consistency: &Consistency,
        request_id: &str,
    ) -> Result<IndexScanResults> {
        let routes = self.router.routes(index)?;
        let (defn_id, inst_id, replica_id) =
            (routes[0].defn_id, routes[0].inst_id, routes[0].replica_id);

        // Opened together rather than in turn: a scattered scan cannot yield its
        // first entry until every host has one, so serialising the opens would
        // add a round trip per host before anything at all came back.
        //
        // One host failing abandons the others, which is right — a partial
        // scatter reads part of an index, and there is no result that can be
        // built out of it that is not silently short.
        let streams = futures::future::try_join_all(routes.iter().map(|route| async move {
            let pool = self.pool_for(&route.scan_address)?;
            let mut lease = pool.acquire().await?;
            let stream = lease
                .start_scan(&scan_options(route, opts, consistency, request_id))
                .await
                .map_err(ErrorKind::Indexer)?;

            Ok::<_, Error>(IndexScanStream::new(
                route.scan_address.clone(),
                route.partitions.clone(),
                stream,
                lease,
            ))
        }))
        .await?;

        Ok(IndexScanResults::new(streams, defn_id, inst_id, replica_id))
    }

    /// Re-read `/getIndexStatus`.
    ///
    /// Any one indexer answers for the whole cluster — the endpoint consolidates
    /// what its peers report — so this needs *a* reachable index node rather than
    /// a particular one, and moves on when one does not answer.
    async fn refresh(&self, opts: &IndexScanOptions<'_>) -> Result<()> {
        let generation = self.router.generation();
        let _guard = self.refreshing.lock().await;
        if self.router.generation() != generation {
            // Somebody else refreshed while this was waiting for the lock, and
            // their answer is no older than ours would have been.
            return Ok(());
        }

        let on_behalf_of = opts
            .on_behalf_of
            .cloned()
            .map(crate::httpx::request::OnBehalfOfInfo::try_from)
            .transpose()?;

        // Read before the fetch, not after: a node map from a *later* config
        // than the index list would name hosts the list has never heard of.
        let nodes = self.state.lock().unwrap().nodes.clone();

        let mut tried = Vec::new();
        loop {
            let Some((client, endpoint)) = self.http_component.select_endpoint(&tried)? else {
                return Err(ErrorKind::ServiceNotAvailable {
                    service: ServiceType::INDEX,
                }
                .into());
            };

            let indexing = Indexing {
                http_client: client,
                user_agent: self.http_component.user_agent().to_string(),
                endpoint: endpoint.endpoint.clone(),
                auth: endpoint.auth,
            };

            match indexing.get_index_status(true, on_behalf_of.clone()).await {
                Ok(indexes) => {
                    debug!(
                        "Index component {} read {} index instance(s) from {}",
                        self.id,
                        indexes.len(),
                        endpoint.endpoint
                    );
                    self.router.set_topology(indexes, nodes);
                    return Ok(());
                }
                Err(e) => {
                    debug!(
                        "Index component {} could not read index status from {}: {e}",
                        self.id, endpoint.endpoint
                    );
                    match endpoint.endpoint_id {
                        Some(id) => tried.push(id),
                        // Nothing to exclude means nothing to try next, so the
                        // one failure is the answer.
                        None => return Err(ErrorKind::Indexer(e).into()),
                    }
                }
            }
        }
    }

    /// The pool for one queryport, created on first use.
    ///
    /// Keyed by address rather than by node id, because a node that moves its
    /// queryport is a different endpoint to connect to and the old pool's idle
    /// connections should age out rather than be reused.
    fn pool_for(&self, address: &Address) -> Result<Arc<ClientPool>> {
        let mut pools = self.pools.lock().unwrap();
        if let Some(pool) = pools.get(address) {
            return Ok(pool.clone());
        }
        let connect_options = self.connect_options(&address.host)?;
        let pool = Arc::new(ClientPool::new(
            address.clone(),
            PoolOptions::new(connect_options),
        ));
        pools.insert(address.clone(), pool.clone());
        Ok(pool)
    }

    /// How to dial a queryport.
    ///
    /// **TLS is decided by whether the agent holds a [`TlsConfig`], never by
    /// which port set the address came out of** — the server advertises
    /// `indexScan` and no `indexScanSSL`, so both sets carry the same number and
    /// reading the transport off them would be reading a coin that always lands
    /// the same way.
    fn connect_options(&self, host: &str) -> Result<ConnectOptions> {
        let state = self.state.lock().unwrap();

        let (username, password) =
            match auth_from_authenticator(&state.authenticator, &ServiceType::INDEX, host)? {
                Auth::BasicAuth(basic) => (basic.username, basic.password),
                // The queryport's AuthRequest carries a username and a password
                // and has no other shape, so a bearer token cannot be presented
                // to it at all. Refusing here names the reason; sending empty
                // credentials would get "invalid credentials" from the server
                // and hide it.
                Auth::BearerAuth(_) => {
                    return Err(ErrorKind::FeatureNotAvailable {
                        feature: "index scan".to_string(),
                        msg: "the indexing service's queryport authenticates with a username and password, which this authenticator does not provide".to_string(),
                    }
                    .into())
                }
            };

        Ok(ConnectOptions {
            tcp_keep_alive_time: self.tcp_keep_alive_time,
            ..ConnectOptions::new(username, password).with_tls_config(state.tls_config.clone())
        })
    }
}

/// One scan request, for one host's share of the index.
fn scan_options(
    route: &Route,
    opts: &IndexScanOptions<'_>,
    consistency: &Consistency,
    request_id: &str,
) -> ScanOptions {
    ScanOptions {
        defn_id: route.defn_id,
        request_id: request_id.to_string(),
        // Empty means "whatever this index has", which is what a
        // non-partitioned index needs: its single partition is numbered 0 and
        // naming it is not how the server expects to be asked.
        partitions: if route.partitioned {
            route.partitions.clone()
        } else {
            Vec::new()
        },
        consistency: consistency.clone(),
        scans: opts.scans.clone(),
        projection: opts.projection.clone(),
        distinct: opts.distinct,
        offset: opts.offset,
        limit: opts.limit.unwrap_or(i64::MAX),
        data_encoding: opts.data_encoding,
        user: opts.on_behalf_of.map(|obo| obo.username.clone()),
        timeout: opts.timeout,
    }
}

/// The crate's consistency, as the wire's.
///
/// The whole of the conversion is `AtPlus`: a caller holds mutation tokens, and
/// the indexer wants a vector of `(vbucket, seqno, vbuuid)`. **Deduplicated by
/// keeping the greatest sequence number per vbucket**, because a list of tokens
/// gathered from several mutations names the same vbucket repeatedly and the
/// last write is the one to wait for. A token with a zero vbuuid is refused
/// rather than sent: the indexer silently skips those, so it would read as a
/// fast scan rather than as a scan that ignored the guarantee it was asked for.
fn consistency_from(consistency: &IndexScanConsistency) -> Result<Consistency> {
    Ok(match consistency {
        IndexScanConsistency::Any => Consistency::Any,
        IndexScanConsistency::Session => Consistency::Session,
        IndexScanConsistency::AtPlus(tokens) => {
            let mut highest: HashMap<u16, ScanVectorEntry> = HashMap::new();
            for token in tokens {
                let entry = ScanVectorEntry {
                    vbucket: token.vbid(),
                    seqno: token.seqno(),
                    vbuuid: token.vbuuid(),
                };
                highest
                    .entry(entry.vbucket)
                    .and_modify(|held| {
                        if entry.seqno > held.seqno {
                            *held = entry;
                        }
                    })
                    .or_insert(entry);
            }

            let mut entries: Vec<ScanVectorEntry> = highest.into_values().collect();
            entries.sort_unstable_by_key(|e| e.vbucket);

            Consistency::Query(ScanVector::new(entries).map_err(|msg| {
                Error::from(ErrorKind::InvalidArgument {
                    msg: msg.to_string(),
                    arg: Some("consistency".to_string()),
                })
            })?)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutationtoken::MutationToken;

    #[test]
    fn session_and_any_carry_no_vector() {
        assert_eq!(
            consistency_from(&IndexScanConsistency::Session).expect("session"),
            Consistency::Session
        );
        assert_eq!(
            consistency_from(&IndexScanConsistency::Any).expect("any"),
            Consistency::Any
        );
    }

    #[test]
    fn at_plus_keeps_the_latest_mutation_for_each_vbucket() {
        // A caller collects a token per mutation, so several name one vbucket.
        // Waiting for the earliest of them would be waiting for less than the
        // caller's own writes, which is the whole guarantee they asked for.
        let consistency = consistency_from(&IndexScanConsistency::AtPlus(vec![
            MutationToken::new(7, 0xaaaa, 12),
            MutationToken::new(3, 0xbbbb, 99),
            MutationToken::new(7, 0xaaaa, 40),
            MutationToken::new(7, 0xaaaa, 5),
        ]))
        .expect("a vector");

        let Consistency::Query(vector) = consistency else {
            panic!("at_plus is a Query consistency");
        };
        assert_eq!(
            vector.entries(),
            [
                ScanVectorEntry {
                    vbucket: 3,
                    seqno: 99,
                    vbuuid: 0xbbbb
                },
                ScanVectorEntry {
                    vbucket: 7,
                    seqno: 40,
                    vbuuid: 0xaaaa
                },
            ]
        );
    }

    #[test]
    fn a_token_with_no_vbuuid_is_refused_rather_than_silently_ignored() {
        // The indexer skips a vbucket whose vbuuid is zero — including the
        // sequence-number check — so such a vector is `Any` wearing a disguise:
        // every scan succeeds, latency collapses, and the reads are
        // unbounded-stale while looking like a win.
        let err = consistency_from(&IndexScanConsistency::AtPlus(vec![MutationToken::new(
            1, 0, 10,
        )]))
        .expect_err("a zero vbuuid is not a scan vector");

        assert!(matches!(
            err.kind(),
            ErrorKind::InvalidArgument { arg: Some(arg), .. } if arg == "consistency"
        ));
    }
}
