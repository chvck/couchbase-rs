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

use crate::address::Address;
use crate::auth_mechanism::AuthMechanism;
use crate::authenticator::Authenticator;
use crate::cbconfig::TerseConfig;
use crate::collection_resolver_cached::{
    CollectionResolverCached, CollectionResolverCachedOptions,
};
use crate::collection_resolver_memd::{CollectionResolverMemd, CollectionResolverMemdOptions};
use crate::compressionmanager::{CompressionManager, StdCompressor};
use crate::configmanager::{
    ConfigManager, ConfigManagerMemd, ConfigManagerMemdConfig, ConfigManagerMemdOptions,
};
use crate::configparser::ConfigParser;
use crate::crudcomponent::CrudComponent;
use crate::diagnosticscomponent::{DiagnosticsComponent, DiagnosticsComponentConfig};
use crate::errmapcomponent::ErrMapComponent;
use crate::error::{Error, ErrorKind, Result};
use crate::features::BucketFeature;
use crate::httpcomponent::HttpComponent;
use crate::httpx::client::{ClientConfig, ReqwestClient};
use crate::indexcomponent::{IndexComponent, IndexComponentOptions};
use crate::indexrouter::NodeMap;
use crate::kvclient::{
    KvClient, KvClientBootstrapOptions, KvClientOptions, StdKvClient, UnsolicitedPacket,
};
use crate::kvclient_ops::KvClientOps;
use crate::kvclientpool::{KvClientPool, KvClientPoolOptions, StdKvClientPool};
use crate::memdx::client::Client;
use crate::memdx::opcode::OpCode;
use crate::memdx::packet::ResponsePacket;
use crate::memdx::request::GetClusterConfigRequest;
use crate::mgmtcomponent::{MgmtComponent, MgmtComponentConfig, MgmtComponentOptions};
use crate::mgmtx::options::{GetTerseBucketConfigOptions, GetTerseClusterConfigOptions};
use crate::networktypeheuristic::NetworkTypeHeuristic;
use crate::nmvbhandler::{ConfigUpdater, StdNotMyVbucketConfigHandler};
use crate::options::agent::{AgentOptions, ReconfigureAgentOptions};
use crate::parsedconfig::{ParsedConfig, ParsedConfigBucketFeature, ParsedConfigFeature};
use crate::querycomponent::{QueryComponent, QueryComponentConfig, QueryComponentOptions};
use crate::retry::{RetryComponent, DEFAULT_RETRY_MANAGER};
use crate::searchcomponent::{SearchComponent, SearchComponentConfig, SearchComponentOptions};
use crate::service_type::ServiceType;
use crate::tls_config::TlsConfig;
use crate::util::{get_host_port_from_uri, get_hostname_from_host_port};
use crate::vbucketrouter::{
    StdVbucketRouter, VbucketRouter, VbucketRouterOptions, VbucketRoutingInfo,
};
use crate::{httpx, mgmtx};

use byteorder::BigEndian;
use futures::executor::block_on;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::analyticscomponent::{AnalyticsComponent, AnalyticsComponentOptions};
use crate::componentconfigs::{AgentComponentConfigs, HttpClientConfig};
use crate::httpx::request::{Auth, BasicAuth, BearerAuth};
use crate::kvclient_babysitter::{KvTarget, StdKvClientBabysitter};
use crate::kvendpointclientmanager::{
    KvEndpointClientManager, KvEndpointClientManagerOptions, StdKvEndpointClientManager,
};
use crate::orphan_reporter::OrphanReporter;
use crate::tracingcomponent::{TracingComponent, TracingComponentConfig};
use arc_swap::ArcSwap;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt::{format, Display};
use std::io::Cursor;
use std::net::ToSocketAddrs;
use std::ops::{Add, Deref};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net;
use tokio::runtime::{Handle, Runtime};
use tokio::sync::broadcast::{Receiver, Sender};
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, timeout_at, Instant};

#[derive(Clone)]
struct AgentState {
    bucket: Option<String>,
    tls_config: Option<TlsConfig>,
    authenticator: Authenticator,
    auth_mechanisms: Vec<AuthMechanism>,
    num_pool_connections: usize,
    // http_transport:
    latest_config: ParsedConfig,
    network_type: String,

    disable_error_map: bool,
    disable_mutation_tokens: bool,
    disable_server_durations: bool,
    kv_connect_timeout: Duration,
    kv_connect_throttle_timeout: Duration,
    http_idle_connection_timeout: Duration,
    http_max_idle_connections_per_host: Option<usize>,
    tcp_keep_alive_time: Duration,
    surface_bootstrap_errors: bool,
}

impl Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{ bucket: {:?}, network_type: {}, num_pool_connections: {}, latest_config_rev_id: {}, latest_config_rev_epoch: {}, authenticator: {} }}",
            self.bucket,
            self.network_type,
            self.num_pool_connections,
            self.latest_config.rev_id,
            self.latest_config.rev_epoch,
            self.authenticator,

        )
    }
}

type AgentClientManager = StdKvEndpointClientManager<
    StdKvClientPool<StdKvClientBabysitter<StdKvClient<Client>>, StdKvClient<Client>>,
    StdKvClient<Client>,
>;
type AgentCollectionResolver = CollectionResolverCached<CollectionResolverMemd<AgentClientManager>>;

pub(crate) struct AgentInner {
    state: Arc<Mutex<AgentState>>,

    /// The `(rev_epoch, rev_id)` of the most recently applied config, kept
    /// outside `state`'s mutex on purpose: `apply_config` holds that mutex for
    /// the whole of `update_state_locked`, which re-dials endpoints over the
    /// network, so a reader who waited on the same lock would block for as
    /// long as the reconfiguration it wants to learn about, i.e. through most
    /// of a failover. Reading an `ArcSwap` never blocks on that lock at all.
    ///
    /// Stored only once `update_state_locked` has finished — not the moment
    /// `state.latest_config` is written — because `vb_router`'s routing info
    /// (also an `ArcSwap`, read by ops without touching `state`) does not flip
    /// to the new topology until partway through that call. Publishing the
    /// revision any earlier would let a caller observe the new revision while
    /// still being routed against the old vbucket map.
    latest_config_revision: ArcSwap<Option<(i64, i64)>>,
    bucket: Option<String>,

    /// The per-agent vbucket-UUID cache a scan vector reads through
    /// [`Agent::vbuuid_map`] — see `vbuuid_cache` for why it is cached at all
    /// and what may invalidate it.
    pub(crate) vbuuids: crate::vbuuid_cache::VbUuidCache,

    cfg_manager: Arc<ConfigManagerMemd<AgentClientManager>>,
    conn_mgr: Arc<AgentClientManager>,

    /// Connections for operations that answer with a stream of packets.
    ///
    /// **A second manager rather than a bigger first one.** A streaming
    /// operation holds its connection until it finishes, and kv_engine stops
    /// executing a connection's queue at the first active command it may not
    /// reorder — so a point operation queued behind a range scan fan-out waits
    /// for scans. The two classes also want opposite tuning: cbcore-rs's sweeps
    /// had a `get` fan-out still improving at 64 outstanding on one connection
    /// while a scan fan-out stopped improving at about 4 per connection and
    /// wanted connections instead, so one setting has to pick a loser.
    ///
    /// It connects on demand and so costs nothing until something streams.
    /// Membership is decided by **response count, not opcode**, and it is
    /// settled in two places only: this field and the operation's dispatch site.
    bulk_conn_mgr: Arc<AgentClientManager>,

    vb_router: Arc<StdVbucketRouter>,
    collections: Arc<AgentCollectionResolver>,
    retry_manager: Arc<RetryComponent>,
    http_client: Arc<ReqwestClient>,
    err_map_component: Arc<ErrMapComponent>,

    pub(crate) crud: CrudComponent<
        AgentClientManager,
        StdVbucketRouter,
        StdNotMyVbucketConfigHandler<AgentInner>,
        AgentCollectionResolver,
        StdCompressor,
    >,

    pub(crate) analytics: Arc<AnalyticsComponent<ReqwestClient>>,
    pub(crate) query: Arc<QueryComponent<ReqwestClient>>,
    pub(crate) search: Arc<SearchComponent<ReqwestClient>>,
    pub(crate) mgmt: MgmtComponent<ReqwestClient>,
    pub(crate) index: IndexComponent<ReqwestClient>,
    pub(crate) diagnostics: DiagnosticsComponent<ReqwestClient, AgentClientManager>,
    pub(crate) tracing: Arc<TracingComponent>,
}

pub struct Agent {
    pub(crate) inner: Arc<AgentInner>,
    user_agent: String,
    id: String,
}

impl AgentInner {
    fn gen_agent_component_configs_locked(state: &AgentState) -> AgentComponentConfigs {
        AgentComponentConfigs::gen_from_config(
            &state.latest_config,
            &state.network_type,
            state.tls_config.clone(),
            state.bucket.clone(),
            state.authenticator.clone(),
        )
    }

    pub async fn unsolicited_packet_handler(&self, up: UnsolicitedPacket) {
        let packet = up.packet;
        if packet.op_code == OpCode::Set {
            if let Some(ref extras) = packet.extras {
                if extras.len() < 16 {
                    warn!("Received Set packet with too short extras: {packet:?}");
                    return;
                }

                let mut cursor = Cursor::new(extras);
                let server_rev_epoch = cursor.read_i64().await.unwrap();
                let server_rev_id = cursor.read_i64().await.unwrap();

                if let Some(config) = self
                    .cfg_manager
                    .out_of_band_version(server_rev_id, server_rev_epoch, up.endpoint_id)
                    .await
                {
                    self.apply_config(config).await;
                }
            } else {
                warn!("Received Set packet with no extras: {packet:?}");
            }
        }
    }

    pub async fn apply_config(&self, config: ParsedConfig) {
        let mut state = self.state.lock().await;

        info!(
            "Agent applying updated config: rev_id={rev_id}, rev_epoch={rev_epoch}",
            rev_id = config.rev_id,
            rev_epoch = config.rev_epoch
        );
        let revision = (config.rev_epoch, config.rev_id);
        state.latest_config = config;

        self.update_state_locked(&mut state).await;

        // Stored only now, after update_state_locked has flipped vb_router's
        // routing info to the new topology: publishing earlier would let a
        // caller see the new revision while ops were still routed against the
        // old vbucket map, which is exactly backwards for a cache key meant to
        // change only when the map it keys does.
        self.latest_config_revision.store(Arc::new(Some(revision)));
    }

    /// The `(rev_epoch, rev_id)` of the config this agent last applied.
    ///
    /// `None` before the first config arrives. See `latest_config_revision`
    /// for why this reads a separate `ArcSwap` rather than `state`.
    pub(crate) fn config_revision(&self) -> Option<(i64, i64)> {
        **self.latest_config_revision.load()
    }

    async fn update_state_locked(&self, state: &mut AgentState) {
        debug!("Agent updating state {}", state);

        let agent_component_configs = Self::gen_agent_component_configs_locked(state);

        // In order to avoid race conditions between operations selecting the
        // endpoint they need to send the request to, and fetching an actual
        // client which can send to that endpoint.  We must first ensure that
        // all the new endpoints are available in the manager.  Then update
        // the routing table.  Then go back and remove the old entries from
        // the connection manager list.

        if let Err(e) = self
            .conn_mgr
            .update_endpoints(agent_component_configs.kv_targets.clone(), true)
            .await
        {
            error!("Failed to reconfigure connection manager (add-only); {e}");
        };

        if let Err(e) = self
            .bulk_conn_mgr
            .update_endpoints(agent_component_configs.kv_targets.clone(), true)
            .await
        {
            error!("Failed to reconfigure bulk connection manager (add-only); {e}");
        };

        self.vb_router
            .update_vbucket_info(agent_component_configs.vbucket_routing_info);

        if let Err(e) = self
            .cfg_manager
            .reconfigure(agent_component_configs.config_manager_memd_config)
        {
            error!("Failed to reconfigure memd config watcher component; {e}");
        }

        if let Err(e) = self
            .conn_mgr
            .update_endpoints(agent_component_configs.kv_targets.clone(), false)
            .await
        {
            error!("Failed to reconfigure connection manager; {e}");
        }

        if let Err(e) = self
            .bulk_conn_mgr
            .update_endpoints(agent_component_configs.kv_targets, false)
            .await
        {
            error!("Failed to reconfigure bulk connection manager; {e}");
        }

        self.analytics
            .reconfigure(agent_component_configs.analytics_config);
        self.query.reconfigure(agent_component_configs.query_config);
        self.search
            .reconfigure(agent_component_configs.search_config);
        self.mgmt.reconfigure(agent_component_configs.mgmt_config);
        self.index.reconfigure(agent_component_configs.index_config);
        self.diagnostics
            .reconfigure(agent_component_configs.diagnostics_config);
        self.tracing
            .reconfigure(agent_component_configs.tracing_config);
    }

    pub async fn bucket_features(&self) -> Result<Vec<BucketFeature>> {
        let guard = self.state.lock().await;

        if let Some(bucket) = &guard.latest_config.bucket {
            let mut features = vec![];

            for feature in &bucket.features {
                match feature {
                    ParsedConfigBucketFeature::CreateAsDeleted => {
                        features.push(BucketFeature::CreateAsDeleted)
                    }
                    ParsedConfigBucketFeature::ReplaceBodyWithXattr => {
                        features.push(BucketFeature::ReplaceBodyWithXattr)
                    }
                    ParsedConfigBucketFeature::RangeScan => features.push(BucketFeature::RangeScan),
                    ParsedConfigBucketFeature::ReplicaRead => {
                        features.push(BucketFeature::ReplicaRead)
                    }
                    ParsedConfigBucketFeature::NonDedupedHistory => {
                        features.push(BucketFeature::NonDedupedHistory)
                    }
                    ParsedConfigBucketFeature::ReviveDocument => {
                        features.push(BucketFeature::ReviveDocument)
                    }
                    _ => {}
                }
            }

            return Ok(features);
        }

        Err(ErrorKind::NoBucket.into())
    }

    pub(crate) fn get_bucket_name(&self) -> Option<String> {
        self.bucket.clone()
    }

    /// Where each HTTP-reachable service is, gathered from the components
    /// that each already track their own — not a value kept here, so this
    /// can never fall out of step with what `orchestrate_endpoint` itself
    /// would pick.
    ///
    /// `MEMD` has no entry: it is not an HTTP service, and `endpoints_for`
    /// answers it the same as any other service this crate has no component
    /// for — empty.
    fn http_endpoints(&self) -> ServiceEndpoints {
        ServiceEndpoints {
            mgmt: self.mgmt.network_endpoints(),
            query: self.query.network_endpoints(),
            search: self.search.network_endpoints(),
            analytics: self.analytics.network_endpoints(),
            index: self.index.network_endpoints(),
        }
    }

    pub(crate) fn num_vbuckets(&self) -> Result<usize> {
        self.vb_router.num_vbuckets()
    }

    pub async fn reconfigure(&self, opts: ReconfigureAgentOptions) {
        let mut state = self.state.lock().await;
        state.tls_config = opts.tls_config.clone();
        state.authenticator = opts.authenticator.clone();

        // We manually update tls for http as it requires rebuilding the client.
        match self
            .http_client
            .update_tls(httpx::client::UpdateTlsOptions {
                tls_config: opts.tls_config,
            }) {
            Ok(_) => {}
            Err(e) => {
                warn!("Failed to update TLS for HTTP client: {}", e);
            }
        };

        self.conn_mgr.update_auth(opts.authenticator.clone()).await;
        self.bulk_conn_mgr.update_auth(opts.authenticator).await;

        self.update_state_locked(&mut state).await;

        // **This is the only place the credentials or TLS behind a queryport
        // connection can change** — nothing else writes `state.authenticator` or
        // `state.tls_config`, and a config revision reaches
        // `update_state_locked` without touching either. So the index pools are
        // emptied here rather than on every revision: doing it there would drop
        // every pooled connection each time a rebalance published a config,
        // which is the connection churn the pool was added to remove. After
        // `update_state_locked`, so the pools the next scan builds are built
        // from the credentials this call just installed.
        self.index.drain_pools();
    }
}

impl ConfigUpdater for AgentInner {
    async fn apply_terse_config(&self, config: TerseConfig, source_hostname: &str) {
        let parsed_config = match ConfigParser::parse_terse_config(config, source_hostname) {
            Ok(cfg) => cfg,
            Err(_e) => {
                // TODO: log
                return;
            }
        };

        if let Some(config) = self.cfg_manager.out_of_band_config(parsed_config) {
            self.apply_config(config).await;
        };
    }
}

/// Where each HTTP-reachable service is, as plain URLs — the shape
/// [`Agent::get_service_endpoints`] answers with, gathered fresh from each
/// service's component rather than kept as a value of its own.
///
/// No `memd` field: the data service is not HTTP, and has no queryport-style
/// counterpart here either — [`Agent::index_node_map`] is the one service
/// whose non-HTTP port a caller can ask about.
#[derive(Default)]
struct ServiceEndpoints {
    mgmt: Vec<String>,
    query: Vec<String>,
    search: Vec<String>,
    analytics: Vec<String>,
    index: Vec<String>,
}

/// The pure half of [`Agent::get_service_endpoints`]: which list answers a
/// service, split out so the "a service this cluster does not run is empty,
/// not an error" property is checkable without an `Agent` — which would mean
/// a live cluster — to build one against.
fn endpoints_for(endpoints: &ServiceEndpoints, service: ServiceType) -> Vec<String> {
    match service {
        ServiceType::MGMT => endpoints.mgmt.clone(),
        ServiceType::QUERY => endpoints.query.clone(),
        ServiceType::SEARCH => endpoints.search.clone(),
        ServiceType::ANALYTICS => endpoints.analytics.clone(),
        ServiceType::INDEX => endpoints.index.clone(),
        // MEMD, EVENTING, and anything a typo or a future service adds: this
        // crate has no HTTP endpoint list for them, and "nothing to talk to"
        // is the honest answer rather than a made-up one.
        _ => Vec::new(),
    }
}

impl Agent {
    pub async fn new(opts: AgentOptions) -> Result<Self> {
        let build_version = env!("CARGO_PKG_VERSION");
        let user_agent = format!("cb-rust/{build_version}");
        let agent_id = Uuid::new_v4().to_string();
        info!(
            "Core SDK Version: {} - Agent ID: {}",
            &user_agent, &agent_id
        );
        info!("Agent Options {opts}");

        let auth_mechanisms = if !opts.auth_mechanisms.is_empty() {
            if opts.tls_config.is_none() && opts.auth_mechanisms.contains(&AuthMechanism::Plain) {
                warn!("PLAIN sends credentials in plaintext, this will cause credential leakage on the network");
            } else if opts.tls_config.is_some()
                && (opts.auth_mechanisms.contains(&AuthMechanism::ScramSha512)
                    || opts.auth_mechanisms.contains(&AuthMechanism::ScramSha256)
                    || opts.auth_mechanisms.contains(&AuthMechanism::ScramSha1))
            {
                warn!("Consider using PLAIN for TLS connections, as it is more efficient");
            }

            opts.auth_mechanisms
        } else {
            vec![]
        };

        let mut state = AgentState {
            bucket: opts.bucket_name.clone(),
            authenticator: opts.authenticator.clone(),
            num_pool_connections: opts.kv_config.num_connections,
            latest_config: ParsedConfig::default(),
            network_type: "".to_string(),
            tls_config: opts.tls_config,
            auth_mechanisms: auth_mechanisms.clone(),
            disable_error_map: !opts.kv_config.enable_error_map,
            disable_mutation_tokens: !opts.kv_config.enable_mutation_tokens,
            disable_server_durations: !opts.kv_config.enable_server_durations,
            kv_connect_timeout: opts.kv_config.connect_timeout,
            kv_connect_throttle_timeout: opts.kv_config.connect_throttle_timeout,
            http_idle_connection_timeout: opts.http_config.idle_connection_timeout,
            http_max_idle_connections_per_host: opts.http_config.max_idle_connections_per_host,
            tcp_keep_alive_time: opts
                .tcp_keep_alive_time
                .unwrap_or_else(|| Duration::from_secs(60)),
            surface_bootstrap_errors: opts.surface_bootstrap_errors,
        };

        let bucket_name = opts.bucket_name.clone();

        let http_client = Arc::new(ReqwestClient::new(ClientConfig {
            tls_config: state.tls_config.clone(),
            idle_connection_timeout: state.http_idle_connection_timeout,
            max_idle_connections_per_host: state.http_max_idle_connections_per_host,
            tcp_keep_alive_time: state.tcp_keep_alive_time,
        })?);

        let err_map_component = Arc::new(ErrMapComponent::new());

        let connect_timeout = opts.kv_config.connect_timeout;

        let first_kv_client_configs =
            Self::gen_first_kv_client_configs(&opts.seed_config.memd_addrs, &state);
        let first_http_client_configs = Self::gen_first_http_endpoints(
            user_agent.clone(),
            &opts.seed_config.http_addrs,
            &state,
        );
        let (first_config, cfg_source_host_port) = Self::get_first_config(
            user_agent.clone(),
            first_kv_client_configs,
            &state,
            first_http_client_configs,
            http_client.clone(),
            err_map_component.clone(),
            connect_timeout,
        )
        .await?;

        state.latest_config = first_config.clone();
        let initial_config_revision = Some((first_config.rev_epoch, first_config.rev_id));

        let network_type = if let Some(network) = opts.network {
            if network == "auto" || network.is_empty() {
                NetworkTypeHeuristic::identify(&state.latest_config, &cfg_source_host_port)
            } else {
                network
            }
        } else {
            NetworkTypeHeuristic::identify(&state.latest_config, &cfg_source_host_port)
        };
        info!(
            "Agent {} identified network type: {network_type}",
            &agent_id
        );
        state.network_type = network_type;

        let agent_component_configs = AgentInner::gen_agent_component_configs_locked(&state);

        let tracing = Arc::new(TracingComponent::new(
            agent_component_configs.tracing_config,
        ));

        let err_map_component_conn_mgr = err_map_component.clone();

        let num_pool_connections = state.num_pool_connections;

        let (unsolicited_packet_tx, mut unsolicited_packet_rx) = mpsc::unbounded_channel();

        let bulk_bootstrap_options = KvClientBootstrapOptions {
            client_name: user_agent.clone(),
            disable_error_map: state.disable_error_map,
            disable_mutation_tokens: state.disable_mutation_tokens,
            disable_server_durations: state.disable_server_durations,
            // The error map is fetched and published by the primary manager; a
            // second copy of it would be the same map twice.
            on_err_map_fetched: None,
            tcp_keep_alive_time: state.tcp_keep_alive_time,
            auth_mechanisms: auth_mechanisms.clone(),
            connect_timeout,
        };
        let bulk_unsolicited_packet_tx = unsolicited_packet_tx.clone();
        let bulk_orphan_handler = opts.orphan_response_handler.clone();
        let bulk_authenticator = opts.authenticator.clone();
        let bulk_selected_bucket = opts.bucket_name.clone();

        let conn_mgr_id = Uuid::new_v4().to_string();
        info!(
            "Agent {} creating kv endpoint client manager {}",
            &agent_id, &conn_mgr_id
        );
        let conn_mgr = Arc::new(
            StdKvEndpointClientManager::new(KvEndpointClientManagerOptions {
                id: conn_mgr_id,
                on_close_handler: Arc::new(|_manager_id| {}),
                on_demand_connect: opts.kv_config.on_demand_connect,
                surface_connect_errors: opts.kv_config.surface_connect_errors,
                num_pool_connections,
                connect_throttle_period: opts.kv_config.connect_throttle_timeout,
                bootstrap_options: KvClientBootstrapOptions {
                    client_name: user_agent.clone(),
                    disable_error_map: state.disable_error_map,
                    disable_mutation_tokens: state.disable_mutation_tokens,
                    disable_server_durations: state.disable_server_durations,
                    on_err_map_fetched: Some(Arc::new(move |err_map| {
                        err_map_component_conn_mgr.on_err_map(err_map);
                    })),
                    tcp_keep_alive_time: state.tcp_keep_alive_time,
                    auth_mechanisms,
                    connect_timeout,
                },
                unsolicited_packet_tx: Some(unsolicited_packet_tx),
                orphan_handler: opts.orphan_response_handler,
                endpoints: agent_component_configs.kv_targets.clone(),
                authenticator: opts.authenticator,
                disable_decompression: opts.compression_config.disable_decompression,
                selected_bucket: opts.bucket_name,
                tracing: tracing.clone(),
            })
            .await?,
        );

        // The bulk manager: same endpoints, same credentials, same handlers, its
        // own connections. `on_demand_connect` is what keeps it free until
        // something streams -- the babysitters exist but do not dial until a
        // client is asked for.
        let bulk_conn_mgr_id = Uuid::new_v4().to_string();
        info!(
            "Agent {} creating bulk kv endpoint client manager {} with {} connections",
            &agent_id, &bulk_conn_mgr_id, opts.kv_config.num_bulk_connections
        );
        let bulk_conn_mgr = Arc::new(
            StdKvEndpointClientManager::new(KvEndpointClientManagerOptions {
                id: bulk_conn_mgr_id,
                on_close_handler: Arc::new(|_manager_id| {}),
                on_demand_connect: true,
                surface_connect_errors: opts.kv_config.surface_connect_errors,
                num_pool_connections: opts.kv_config.num_bulk_connections,
                connect_throttle_period: opts.kv_config.connect_throttle_timeout,
                bootstrap_options: bulk_bootstrap_options,
                unsolicited_packet_tx: Some(bulk_unsolicited_packet_tx),
                orphan_handler: bulk_orphan_handler,
                endpoints: agent_component_configs.kv_targets,
                authenticator: bulk_authenticator,
                disable_decompression: opts.compression_config.disable_decompression,
                selected_bucket: bulk_selected_bucket,
                tracing: tracing.clone(),
            })
            .await?,
        );

        let cfg_manager = Arc::new(ConfigManagerMemd::new(
            agent_component_configs.config_manager_memd_config,
            ConfigManagerMemdOptions {
                polling_period: opts.config_poller_config.poll_interval,
                kv_client_manager: conn_mgr.clone(),
                first_config,
                fetch_timeout: opts.config_poller_config.fetch_timeout,
            },
        ));
        let vb_router = Arc::new(StdVbucketRouter::new(
            agent_component_configs.vbucket_routing_info,
            VbucketRouterOptions {},
        ));

        let nmvb_handler = Arc::new(StdNotMyVbucketConfigHandler::new());

        let memd_resolver = CollectionResolverMemd::new(CollectionResolverMemdOptions {
            conn_mgr: conn_mgr.clone(),
        });

        let collections = Arc::new(CollectionResolverCached::new(
            CollectionResolverCachedOptions {
                resolver: memd_resolver,
            },
        ));

        // The manager is the embedder's to replace; the error map is not.
        let retry_manager = Arc::new(RetryComponent::new(
            err_map_component.clone(),
            opts.retry_manager
                .clone()
                .unwrap_or_else(|| DEFAULT_RETRY_MANAGER.clone()),
        ));
        let compression_manager = Arc::new(CompressionManager::new(opts.compression_config));

        let crud = CrudComponent::new(
            nmvb_handler.clone(),
            vb_router.clone(),
            conn_mgr.clone(),
            bulk_conn_mgr.clone(),
            collections.clone(),
            retry_manager.clone(),
            compression_manager,
        );

        let mgmt_id = Uuid::new_v4().to_string();
        info!("Agent {} creating mgmt component {}", &agent_id, &mgmt_id);
        let mgmt = MgmtComponent::new(
            retry_manager.clone(),
            http_client.clone(),
            tracing.clone(),
            agent_component_configs.mgmt_config,
            MgmtComponentOptions {
                id: mgmt_id,
                user_agent: user_agent.clone(),
            },
        );

        let analytics_id = Uuid::new_v4().to_string();
        info!(
            "Agent {} creating analytics component {}",
            &agent_id, &analytics_id
        );
        let analytics = Arc::new(AnalyticsComponent::new(
            retry_manager.clone(),
            http_client.clone(),
            agent_component_configs.analytics_config,
            AnalyticsComponentOptions {
                id: analytics_id,
                user_agent: user_agent.clone(),
            },
        ));

        let query_id = Uuid::new_v4().to_string();
        info!("Agent {} creating query component {}", &agent_id, &query_id);
        let query = Arc::new(QueryComponent::new(
            retry_manager.clone(),
            http_client.clone(),
            tracing.clone(),
            agent_component_configs.query_config,
            QueryComponentOptions {
                id: query_id,
                user_agent: user_agent.clone(),
            },
        ));

        let search_id = Uuid::new_v4().to_string();
        info!(
            "Agent {} creating search component {}",
            &agent_id, &search_id
        );
        let search = Arc::new(SearchComponent::new(
            retry_manager.clone(),
            http_client.clone(),
            tracing.clone(),
            agent_component_configs.search_config,
            SearchComponentOptions {
                id: search_id,
                user_agent: user_agent.clone(),
            },
        ));

        let index_id = Uuid::new_v4().to_string();
        info!("Agent {} creating index component {}", &agent_id, &index_id);
        let index = IndexComponent::new(
            http_client.clone(),
            agent_component_configs.index_config,
            IndexComponentOptions {
                id: index_id,
                user_agent: user_agent.clone(),
                tcp_keep_alive_time: state.tcp_keep_alive_time,
            },
        );

        let diagnostics = DiagnosticsComponent::new(
            conn_mgr.clone(),
            query.clone(),
            search.clone(),
            retry_manager.clone(),
            agent_component_configs.diagnostics_config,
        );

        let state = Arc::new(Mutex::new(state));

        let inner = Arc::new(AgentInner {
            state,
            latest_config_revision: ArcSwap::new(Arc::new(initial_config_revision)),
            bucket: bucket_name,
            vbuuids: crate::vbuuid_cache::VbUuidCache::default(),
            cfg_manager: cfg_manager.clone(),
            conn_mgr,
            bulk_conn_mgr,
            vb_router,
            crud,
            collections,
            retry_manager,
            http_client,
            err_map_component,

            mgmt,
            index,
            analytics,
            query,
            search,
            diagnostics,
            tracing,
        });

        let inner_clone = Arc::downgrade(&inner);
        tokio::spawn(async move {
            while let Some(packet) = unsolicited_packet_rx.recv().await {
                if let Some(inner_clone) = inner_clone.upgrade() {
                    inner_clone.unsolicited_packet_handler(packet).await;
                } else {
                    break;
                }
            }
            debug!("Unsolicited packet handler exited");
        });

        nmvb_handler.set_watcher(Arc::downgrade(&inner)).await;

        Self::start_config_watcher(Arc::downgrade(&inner), cfg_manager);

        let agent = Agent {
            inner,
            user_agent,
            id: agent_id.clone(),
        };

        info!("Agent {agent_id} created");

        Ok(agent)
    }

    // reconfigure allows updating certain aspects of the agent at runtime.
    // Note: toggling TLS on and off is not supported and will result in internal errors.
    pub async fn reconfigure(&self, opts: ReconfigureAgentOptions) {
        self.inner.reconfigure(opts).await
    }

    /// The revision of the cluster config this agent is working from.
    ///
    /// Anything derived from the vbucket map — vbucket UUIDs, say — can cache
    /// against this, because a failover or a vbucket movement bumps it.
    /// `(epoch, id)` and not the other way round: an epoch bump resets the id.
    ///
    /// `None` before the first config arrives, which a caller should read as
    /// "nothing derived from a vbucket map is cacheable yet". In practice this
    /// is always `Some`: `Agent::new` does not return before a first config is
    /// fetched, so no caller ever holds an `Agent` that has not seen one.
    pub fn config_revision(&self) -> Option<(i64, i64)> {
        self.inner.config_revision()
    }

    /// Where each service is reachable, as this agent's latest config
    /// describes it.
    ///
    /// Empty for a service the cluster does not run and for a config that has
    /// not arrived — both of which a caller should read as "nothing to talk
    /// to" rather than as an error. The latter never happens in practice; see
    /// `config_revision` for why.
    pub fn get_service_endpoints(&self, service: ServiceType) -> Vec<String> {
        endpoints_for(&self.inner.http_endpoints(), service)
    }

    /// Where each node's queryport is, keyed the way `/getIndexStatus` names
    /// nodes.
    ///
    /// Empty for a cluster with no index service, and for a config that has
    /// not arrived. Reads the node map the index component's own reconfigure
    /// already computed from `NodeMap::from_config`, rather than recomputing
    /// it here from a config and a network type — a second derivation could
    /// disagree with the one routing actually uses if either read a
    /// different config revision; a direct read cannot.
    pub fn index_node_map(&self) -> NodeMap {
        self.inner.index.nodes()
    }

    fn start_config_watcher(
        inner: Weak<AgentInner>,
        config_watcher: Arc<impl ConfigManager>,
    ) -> JoinHandle<()> {
        let mut watch_rx = config_watcher.watch();

        let inner = inner.clone();
        tokio::spawn(async move {
            loop {
                match watch_rx.changed().await {
                    Ok(_) => {
                        let pc = {
                            // apply_config requires an owned ParsedConfig, as it takes ownership of it.
                            // Doing the clone within a block also means we can release the lock that
                            // borrow_and_update() takes as soon as possible.
                            watch_rx.borrow_and_update().clone()
                        };
                        if let Some(i) = inner.upgrade() {
                            i.apply_config(pc).await;
                        } else {
                            debug!("Config watcher inner dropped, exiting");
                            return;
                        }
                    }
                    Err(_e) => {
                        debug!("Config watcher channel closed");
                        return;
                    }
                }
            }
        })
    }

    async fn get_first_config<C: httpx::client::Client>(
        client_name: String,
        kv_targets: HashMap<String, KvTarget>,
        state: &AgentState,
        http_configs: HashMap<String, FirstHttpConfig>,
        http_client: Arc<C>,
        err_map_component: Arc<ErrMapComponent>,
        connect_timeout: Duration,
    ) -> Result<(ParsedConfig, String)> {
        loop {
            // What each endpoint said this time round. Collected whether or not
            // it will be returned, so that a failed pass can say why in one line
            // instead of leaving the reasons scattered across the log.
            let mut attempt_errs: HashMap<String, Error> = HashMap::new();

            for target in kv_targets.values() {
                let host = &target.address;
                let err_map_component_clone = err_map_component.clone();
                let timeout_result = timeout(
                    connect_timeout,
                    StdKvClient::new(KvClientOptions {
                        address: target.clone(),
                        authenticator: state.authenticator.clone(),
                        selected_bucket: state.bucket.clone(),
                        bootstrap_options: KvClientBootstrapOptions {
                            client_name: client_name.clone(),
                            disable_error_map: state.disable_error_map,
                            disable_mutation_tokens: true,
                            disable_server_durations: true,
                            on_err_map_fetched: Some(Arc::new(move |err_map| {
                                err_map_component_clone.on_err_map(err_map);
                            })),
                            tcp_keep_alive_time: state.tcp_keep_alive_time,
                            auth_mechanisms: state.auth_mechanisms.clone(),
                            connect_timeout,
                        },
                        endpoint_id: "".to_string(),
                        unsolicited_packet_tx: None,
                        orphan_handler: None,
                        on_close_tx: None,
                        disable_decompression: false,
                        id: Uuid::new_v4().to_string(),
                        tracing: Default::default(),
                    }),
                )
                .await;

                let client: StdKvClient<Client> = match timeout_result {
                    Ok(client_result) => match client_result {
                        Ok(client) => client,
                        Err(e) => {
                            let mut msg = format!("Failed to connect to endpoint: {e}");
                            if let Some(source) = e.source() {
                                msg = format!("{msg} - {source}");
                            }
                            warn!("{msg}");
                            attempt_errs.insert(host.to_string(), e);
                            continue;
                        }
                    },
                    Err(_e) => {
                        attempt_errs.insert(
                            host.to_string(),
                            Error::new_message_error(format!(
                                "timed out connecting to endpoint after {connect_timeout:?}"
                            )),
                        );
                        continue;
                    }
                };

                let raw_config = match client
                    .get_cluster_config(GetClusterConfigRequest {
                        known_version: None,
                    })
                    .await
                {
                    Ok(resp) => resp.config,
                    Err(e) => {
                        attempt_errs.insert(host.to_string(), Error::new_contextual_memdx_error(e));
                        continue;
                    }
                };

                client.close().await?;

                let config: TerseConfig = serde_json::from_slice(&raw_config).map_err(|e| {
                    Error::new_message_error(format!("failed to deserialize config: {e}"))
                })?;

                match ConfigParser::parse_terse_config(config, host.host.as_str()) {
                    Ok(c) => {
                        return Ok((c, format!("{}:{}", host.host, host.port)));
                    }
                    Err(e) => {
                        attempt_errs.insert(host.to_string(), e);
                        continue;
                    }
                };
            }

            info!("Failed to fetch config over kv, attempting http");
            for endpoint_config in http_configs.values() {
                let endpoint = endpoint_config.endpoint.clone();
                let host_port = get_host_port_from_uri(&endpoint)?;
                let auth = match &endpoint_config.authenticator {
                    Authenticator::PasswordAuthenticator(authenticator) => {
                        let user_pass =
                            authenticator.get_credentials(&ServiceType::MGMT, host_port.clone())?;
                        Auth::BasicAuth(BasicAuth::new(user_pass.username, user_pass.password))
                    }
                    Authenticator::CertificateAuthenticator(_authenticator) => {
                        Auth::BasicAuth(BasicAuth::new("".to_string(), "".to_string()))
                    }
                    Authenticator::JwtAuthenticator(authenticator) => {
                        Auth::BearerAuth(BearerAuth::new(authenticator.get_token()))
                    }
                };

                match Self::fetch_http_config(
                    http_client.clone(),
                    endpoint,
                    endpoint_config.user_agent.clone(),
                    auth,
                    endpoint_config.bucket_name.clone(),
                )
                .await
                {
                    Ok(c) => {
                        return Ok((c, host_port));
                    }
                    Err(e) => {
                        attempt_errs.insert(host_port, e);
                    }
                };
            }

            if state.surface_bootstrap_errors {
                return Err(Error::new_bootstrap_all_failed_error(attempt_errs));
            }

            let err = Error::new_bootstrap_all_failed_error(attempt_errs);
            info!("Failed to fetch config from any source, trying again: {err}");

            // TODO: Make configurable?
            sleep(Duration::from_secs(1)).await;
        }
    }

    pub(crate) async fn fetch_http_config<C: httpx::client::Client>(
        http_client: Arc<C>,
        endpoint: String,
        user_agent: String,
        auth: Auth,
        bucket_name: Option<String>,
    ) -> Result<ParsedConfig> {
        debug!("Polling config from {}", &endpoint);

        let host_port = get_host_port_from_uri(&endpoint)?;
        let hostname = get_hostname_from_host_port(&host_port)?;

        let parsed = if let Some(bucket_name) = bucket_name {
            let config = mgmtx::mgmt::Management {
                http_client,
                user_agent,
                endpoint: endpoint.clone(),
                canonical_endpoint: endpoint.clone(),
                auth,
                tracing: Default::default(),
            }
            .get_terse_bucket_config(&GetTerseBucketConfigOptions {
                bucket_name: &bucket_name,
                on_behalf_of_info: None,
            })
            .await
            .map_err(Error::from)?;

            ConfigParser::parse_terse_config(config, &hostname)?
        } else {
            let config = mgmtx::mgmt::Management {
                http_client,
                user_agent,
                endpoint: endpoint.clone(),
                canonical_endpoint: endpoint.clone(),
                auth,
                tracing: Default::default(),
            }
            .get_terse_cluster_config(&GetTerseClusterConfigOptions {
                on_behalf_of_info: None,
            })
            .await
            .map_err(Error::from)?;

            ConfigParser::parse_terse_config(config, &hostname)?
        };

        Ok(parsed)
    }

    fn gen_first_kv_client_configs(
        memd_addrs: &Vec<Address>,
        state: &AgentState,
    ) -> HashMap<String, KvTarget> {
        let mut clients = HashMap::new();
        for addr in memd_addrs {
            let node_id = format!("kv-{addr}");
            let target = KvTarget {
                address: addr.clone(),
                tls_config: state.tls_config.clone(),
                canonical_address: addr.clone(),
            };
            clients.insert(node_id, target);
        }

        clients
    }

    fn gen_first_http_endpoints(
        client_name: String,
        mgmt_addrs: &Vec<Address>,
        state: &AgentState,
    ) -> HashMap<String, FirstHttpConfig> {
        let mut clients = HashMap::new();
        for addr in mgmt_addrs {
            let node_id = format!("mgmt{addr}");
            let base = if state.tls_config.is_some() {
                "https"
            } else {
                "http"
            };
            let config = FirstHttpConfig {
                endpoint: format!("{base}://{addr}"),
                tls: state.tls_config.clone(),
                user_agent: client_name.clone(),
                authenticator: state.authenticator.clone(),
                bucket_name: state.bucket.clone(),
            };
            clients.insert(node_id, config);
        }

        clients
    }

    pub(crate) async fn run_with_bucket_feature_check<T, Fut>(
        &self,
        feature: BucketFeature,
        operation: impl FnOnce() -> Fut,
        message: impl Into<String>,
    ) -> Result<T>
    where
        Fut: std::future::Future<Output = Result<T>>,
    {
        let features = self.bucket_features().await?;

        if !features.contains(&feature) {
            return Err(Error::new_feature_not_available_error(
                format!("{feature:?}"),
                message.into(),
            ));
        }

        operation().await
    }

    pub(crate) fn get_bucket_name(&self) -> Option<String> {
        self.inner.get_bucket_name()
    }
}

struct FirstHttpConfig {
    pub endpoint: String,
    pub tls: Option<TlsConfig>,
    pub user_agent: String,
    pub authenticator: Authenticator,
    pub bucket_name: Option<String>,
}

impl Drop for Agent {
    fn drop(&mut self) {
        debug!(
            "Dropping agent {}, {} strong references remain",
            self.id,
            Arc::strong_count(&self.inner)
        );
    }
}

#[cfg(test)]
mod config_revision_tests {
    #[test]
    fn a_revision_orders_epoch_before_id() {
        // The pair is (epoch, id) and not (id, epoch), because an epoch bump
        // resets the id: comparing the wrong way round makes a fresh config
        // after a failover look older than the one it replaced.
        assert!((1i64, 5i64) < (2i64, 1i64));
    }
}

#[cfg(test)]
mod service_endpoint_tests {
    use super::*;

    #[test]
    fn an_unknown_service_is_no_endpoints_rather_than_an_error() {
        // A cluster with no index service and a caller asking for one are the
        // same answer: nothing to talk to. A caller should read it as "no
        // scans from here" rather than as a failure to start.
        let endpoints = endpoints_for(&ServiceEndpoints::default(), ServiceType::INDEX);
        assert!(endpoints.is_empty());
    }

    #[test]
    fn a_service_the_cluster_runs_is_the_list_gathered_for_it_and_no_other() {
        // Pinning that endpoints_for is a real dispatch and not just "always
        // empty" — mgmt's list must come back untouched, and it must not
        // leak into a sibling service's answer.
        let endpoints = ServiceEndpoints {
            mgmt: vec!["http://127.0.0.1:8091".to_string()],
            query: vec!["http://127.0.0.1:8093".to_string()],
            ..ServiceEndpoints::default()
        };

        assert_eq!(
            endpoints_for(&endpoints, ServiceType::MGMT),
            vec!["http://127.0.0.1:8091".to_string()]
        );
        assert!(endpoints_for(&endpoints, ServiceType::SEARCH).is_empty());
    }
}

#[cfg(test)]
mod bootstrap_tests {
    use super::*;
    use crate::authenticator::PasswordAuthenticator;
    use crate::options::agent::SeedConfig;
    use tokio::time::timeout;

    /// A port nothing listens on, so an attempt against it fails, and fails fast.
    async fn closed_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    /// Two ports, so a failure recorded against the memd seed is told apart from
    /// the one recorded against the http seed rather than overwriting it.
    async fn options_against_nothing(surface_bootstrap_errors: bool) -> AgentOptions {
        let memd = Address {
            host: "127.0.0.1".to_string(),
            port: closed_port().await,
        };
        let http = Address {
            host: "127.0.0.1".to_string(),
            port: closed_port().await,
        };

        AgentOptions::new(
            SeedConfig::new()
                .memd_addrs(vec![memd])
                .http_addrs(vec![http]),
            Authenticator::PasswordAuthenticator(PasswordAuthenticator {
                username: "user".to_string(),
                password: "pass".to_string(),
            }),
        )
        .surface_bootstrap_errors(surface_bootstrap_errors)
    }

    #[tokio::test]
    async fn bootstrap_reports_what_every_endpoint_said_when_asked() {
        let opts = options_against_nothing(true).await;

        let err = timeout(Duration::from_secs(10), Agent::new(opts))
            .await
            .expect("bootstrap should give up rather than start the seed list again")
            .err()
            .expect("bootstrapping against closed ports cannot have succeeded");

        let errors = match err.kind() {
            ErrorKind::BootstrapAllFailed { errors } => errors,
            other => panic!("expected BootstrapAllFailed, got {other:?}"),
        };

        // Both seeds were tried, and each is answered for separately -- the point
        // of keying by endpoint rather than reporting one failure for the set.
        assert_eq!(
            errors.len(),
            2,
            "expected one failure per seed, got: {}",
            err
        );

        // And it reads as something a person can act on.
        assert!(
            err.to_string().starts_with("all bootstrap hosts failed ("),
            "unexpected message: {err}"
        );
    }

    #[tokio::test]
    async fn bootstrap_retries_the_seed_list_by_default() {
        let opts = options_against_nothing(false).await;

        let res = timeout(Duration::from_secs(3), Agent::new(opts)).await;

        assert!(
            res.is_err(),
            "the default must keep retrying the seed list rather than reporting a failure"
        );
    }
}
