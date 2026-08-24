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
use crate::memdx::dispatcher::OrphanResponseHandler;
use crate::retry::RetryManager;
use crate::tls_config::TlsConfig;
use std::fmt::{Debug, Display};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
#[non_exhaustive]
pub struct AgentOptions {
    pub seed_config: SeedConfig,
    pub authenticator: Authenticator,

    // By default, the SDK will default to using the mechanisms provided by the
    // Authenticator, but this can be overridden here.
    pub auth_mechanisms: Vec<AuthMechanism>,
    pub tls_config: Option<TlsConfig>,
    pub bucket_name: Option<String>,
    pub network: Option<String>,

    pub compression_config: CompressionConfig,
    pub config_poller_config: ConfigPollerConfig,
    pub kv_config: KvConfig,
    pub http_config: HttpConfig,
    pub tcp_keep_alive_time: Option<Duration>,

    /// Whether bootstrapping an agent gives up once every endpoint has failed,
    /// rather than starting the list again.
    ///
    /// Off by default, which retries the seed list indefinitely. An agent whose
    /// cluster is not up yet then arrives once it is, without the caller having
    /// to arrange that -- but a cluster that will never accept these credentials
    /// looks exactly the same from outside, and nothing is reported while it is
    /// tried again.
    ///
    /// Turning it on returns [`ErrorKind::BootstrapAllFailed`] after one pass,
    /// carrying what each endpoint said, which is what gocbcorex does. Bounding
    /// how long to wait for a cluster that might still arrive is then the
    /// caller's to decide, as deadlines are throughout this crate.
    pub surface_bootstrap_errors: bool,

    pub orphan_response_handler: Option<OrphanResponseHandler>,
    /// Who decides whether a failed operation is retried.
    ///
    /// `None` uses the SDK's own [`DefaultRetryManager`]. Supply one when the
    /// embedder already owns the recovery for a condition the SDK would otherwise
    /// retry underneath it — see [`RetryManager`] for the case that motivated
    /// this.
    pub retry_manager: Option<Arc<dyn RetryManager>>,
}

impl Debug for AgentOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentOptions")
            .field("seed_config", &self.seed_config)
            .field("auth_mechanisms", &self.auth_mechanisms)
            .field("tls_config", &self.tls_config)
            .field("bucket_name", &self.bucket_name)
            .field("network", &self.network)
            .field("compression_config", &self.compression_config)
            .field("config_poller_config", &self.config_poller_config)
            .field("kv_config", &self.kv_config)
            .field("http_config", &self.http_config)
            .field("tcp_keep_alive_time", &self.tcp_keep_alive_time)
            .field("surface_bootstrap_errors", &self.surface_bootstrap_errors)
            .finish()
    }
}

impl AgentOptions {
    pub fn new(seed_config: SeedConfig, authenticator: Authenticator) -> Self {
        Self {
            tls_config: None,
            authenticator,
            bucket_name: None,
            network: None,
            seed_config,
            compression_config: CompressionConfig::default(),
            config_poller_config: ConfigPollerConfig::default(),
            auth_mechanisms: vec![],
            kv_config: KvConfig::default(),
            http_config: HttpConfig::default(),
            tcp_keep_alive_time: None,
            surface_bootstrap_errors: false,
            orphan_response_handler: None,
            retry_manager: None,
        }
    }

    pub fn surface_bootstrap_errors(mut self, surface_bootstrap_errors: bool) -> Self {
        self.surface_bootstrap_errors = surface_bootstrap_errors;
        self
    }

    pub fn seed_config(mut self, seed_config: SeedConfig) -> Self {
        self.seed_config = seed_config;
        self
    }

    pub fn authenticator(mut self, authenticator: Authenticator) -> Self {
        self.authenticator = authenticator;
        self
    }

    pub fn tls_config(mut self, tls_config: impl Into<Option<TlsConfig>>) -> Self {
        self.tls_config = tls_config.into();
        self
    }

    pub fn bucket_name(mut self, bucket_name: impl Into<Option<String>>) -> Self {
        self.bucket_name = bucket_name.into();
        self
    }

    pub fn network(mut self, network: impl Into<Option<String>>) -> Self {
        self.network = network.into();
        self
    }

    pub fn compression_config(mut self, compression_config: CompressionConfig) -> Self {
        self.compression_config = compression_config;
        self
    }

    pub fn config_poller_config(mut self, config_poller_config: ConfigPollerConfig) -> Self {
        self.config_poller_config = config_poller_config;
        self
    }

    pub fn auth_mechanisms(mut self, auth_mechanisms: Vec<AuthMechanism>) -> Self {
        self.auth_mechanisms = auth_mechanisms;
        self
    }

    pub fn kv_config(mut self, kv_config: KvConfig) -> Self {
        self.kv_config = kv_config;
        self
    }

    pub fn http_config(mut self, http_config: HttpConfig) -> Self {
        self.http_config = http_config;
        self
    }

    pub fn tcp_keep_alive_time(mut self, tcp_keep_alive: Duration) -> Self {
        self.tcp_keep_alive_time = Some(tcp_keep_alive);
        self
    }

    pub fn orphan_reporter_handler(
        mut self,
        orphan_response_handler: OrphanResponseHandler,
    ) -> Self {
        self.orphan_response_handler = Some(orphan_response_handler);
        self
    }

    /// Replace the retry manager. See [`RetryManager`].
    pub fn retry_manager(mut self, retry_manager: Arc<dyn RetryManager>) -> Self {
        self.retry_manager = Some(retry_manager);
        self
    }
}

#[derive(Default, Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct SeedConfig {
    pub http_addrs: Vec<Address>,
    pub memd_addrs: Vec<Address>,
}

impl SeedConfig {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn http_addrs(mut self, http_addrs: Vec<Address>) -> Self {
        self.http_addrs = http_addrs;
        self
    }

    pub fn memd_addrs(mut self, memd_addrs: Vec<Address>) -> Self {
        self.memd_addrs = memd_addrs;
        self
    }
}

#[derive(Default, Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct CompressionConfig {
    pub disable_decompression: bool,
    pub mode: CompressionMode,
}

impl CompressionConfig {
    pub fn new(mode: CompressionMode) -> Self {
        Self {
            disable_decompression: false,
            mode,
        }
    }

    pub fn disable_decompression(mut self, disable_decompression: bool) -> Self {
        self.disable_decompression = disable_decompression;
        self
    }

    pub fn mode(mut self, mode: CompressionMode) -> Self {
        self.mode = mode;
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum CompressionMode {
    Enabled { min_size: usize, min_ratio: f64 },
    Disabled,
}

impl Default for CompressionMode {
    fn default() -> Self {
        Self::Enabled {
            min_size: 32,
            min_ratio: 0.83,
        }
    }
}

impl Display for CompressionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompressionMode::Enabled {
                min_size,
                min_ratio,
            } => {
                write!(f, "{{ min_size: {}, min_ratio: {} }}", min_size, min_ratio)
            }
            CompressionMode::Disabled => write!(f, "disabled"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ConfigPollerConfig {
    pub poll_interval: Duration,
    pub fetch_timeout: Duration,
}

impl ConfigPollerConfig {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    pub fn fetch_timeout(mut self, fetch_timeout: Duration) -> Self {
        self.fetch_timeout = fetch_timeout;
        self
    }
}

impl Default for ConfigPollerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(2500),
            fetch_timeout: Duration::from_millis(2500),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct KvConfig {
    pub on_demand_connect: bool,

    /// Whether an operation that cannot get a connection fails with the error
    /// from the last connect attempt instead of waiting for a reconnect.
    ///
    /// Off by default, which leaves an operation waiting until a connection is
    /// established. That is the right answer when a connection is merely slow to
    /// come back, but it makes a permanent failure -- a wrong password, a
    /// rejected certificate -- indistinguishable from a slow one: the cause goes
    /// to the log and the caller keeps waiting, with nothing in this crate
    /// bounding the wait.
    ///
    /// Turning it on hands that stored error to the caller instead, which is what
    /// gocbcorex does. A *first* connect still waits, because until an attempt
    /// has failed there is no error to report.
    pub surface_connect_errors: bool,
    pub enable_error_map: bool,
    pub enable_mutation_tokens: bool,
    pub enable_server_durations: bool,
    pub num_connections: usize,

    /// Connections available to operations that answer with a **stream** of
    /// packets: a range scan's continues, a `stats` sweep.
    ///
    /// These live on a second connection manager, because a streaming operation
    /// holds its connection for as long as the answer takes and the server stops
    /// executing a connection's queue at the first command it may not reorder —
    /// so a point operation behind a fan-out waits for scans rather than for
    /// itself. cbcore-rs measured a `get` at 26.0 ms behind a scan against a
    /// 216 µs control, and its connection sweeps put a scan fan-out's optimum at
    /// about sixteen connections, which is where this default comes from.
    ///
    /// The second manager connects **on demand**, so it costs nothing until
    /// something streams. Zero disables it: streaming operations then fail
    /// rather than borrowing the connections point operations are using.
    pub num_bulk_connections: usize,

    pub connect_timeout: Duration,
    pub connect_throttle_timeout: Duration,
}

impl KvConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_demand_connect(mut self, on_demand_connect: bool) -> Self {
        self.on_demand_connect = on_demand_connect;
        self
    }

    pub fn surface_connect_errors(mut self, enable: bool) -> Self {
        self.surface_connect_errors = enable;
        self
    }

    pub fn enable_error_map(mut self, enable: bool) -> Self {
        self.enable_error_map = enable;
        self
    }

    pub fn enable_mutation_tokens(mut self, enable: bool) -> Self {
        self.enable_mutation_tokens = enable;
        self
    }

    pub fn enable_server_durations(mut self, enable: bool) -> Self {
        self.enable_server_durations = enable;
        self
    }

    pub fn connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.connect_timeout = connect_timeout;
        self
    }

    pub fn connect_throttle_timeout(mut self, connect_throttle_timeout: Duration) -> Self {
        self.connect_throttle_timeout = connect_throttle_timeout;
        self
    }

    pub fn num_connections(mut self, num: usize) -> Self {
        self.num_connections = num;
        self
    }

    pub fn num_bulk_connections(mut self, num: usize) -> Self {
        self.num_bulk_connections = num;
        self
    }
}

impl Default for KvConfig {
    fn default() -> Self {
        Self {
            on_demand_connect: false,
            surface_connect_errors: false,
            enable_error_map: true,
            enable_mutation_tokens: true,
            enable_server_durations: true,
            num_connections: 1,
            num_bulk_connections: 16,
            connect_timeout: Duration::from_secs(10),
            connect_throttle_timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct HttpConfig {
    pub max_idle_connections_per_host: Option<usize>,
    pub idle_connection_timeout: Duration,
}

impl HttpConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn max_idle_connections_per_host(mut self, max_idle_connections_per_host: usize) -> Self {
        self.max_idle_connections_per_host = Some(max_idle_connections_per_host);
        self
    }

    pub fn idle_connection_timeout(mut self, idle_connection_timeout: Duration) -> Self {
        self.idle_connection_timeout = idle_connection_timeout;
        self
    }
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            max_idle_connections_per_host: None,
            idle_connection_timeout: Duration::from_secs(1),
        }
    }
}

#[derive(Clone)]
#[non_exhaustive]
pub struct ReconfigureAgentOptions {
    pub authenticator: Authenticator,
    pub tls_config: Option<TlsConfig>,
}

impl ReconfigureAgentOptions {
    pub fn new(authenticator: Authenticator) -> Self {
        Self {
            tls_config: None,
            authenticator,
        }
    }

    pub fn tls_config(mut self, tls_config: impl Into<Option<TlsConfig>>) -> Self {
        self.tls_config = tls_config.into();
        self
    }
}

impl Display for SeedConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{ http_addrs: {:?}, memd_addrs: {:?} }}",
            self.http_addrs, self.memd_addrs
        )
    }
}

impl Display for CompressionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{ disable_decompression: {}, mode: {} }}",
            self.disable_decompression, self.mode
        )
    }
}

impl Display for ConfigPollerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{ poll_interval: {:?}, fetch_timeout: {:?} }}",
            self.poll_interval, self.fetch_timeout
        )
    }
}

impl Display for KvConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{ on_demand_connect: {}, enable_error_map: {}, enable_mutation_tokens: {}, enable_server_durations: {}, num_connections: {}, num_bulk_connections: {}, connect_timeout: {:?}, connect_throttle_timeout: {:?} }}",
            self.on_demand_connect,
            self.enable_error_map,
            self.enable_mutation_tokens,
            self.enable_server_durations,
            self.num_connections,
            self.num_bulk_connections,
            self.connect_timeout,
            self.connect_throttle_timeout
        )
    }
}

impl Display for HttpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{ max_idle_connections_per_host: {:?}, idle_connection_timeout: {:?} }}",
            self.max_idle_connections_per_host, self.idle_connection_timeout
        )
    }
}

impl Display for AgentOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tls_config = if cfg!(feature = "rustls-tls") {
            "rustls-tls"
        } else if cfg!(feature = "native-tls") {
            "native-tls"
        } else {
            "none"
        };

        write!(
            f,
            "{{ seed_config: {}, auth_mechanisms: {:?}, tls_config: {}, bucket_name: {:?}, network: {:?}, compression_config: {}, config_poller_config: {}, kv_config: {}, http_config: {}, tcp_keep_alive_time: {:?}, orphan_response_handler: {}, retry_manager: {} }}",
            self.seed_config,
            self.auth_mechanisms,
            tls_config,
            self.bucket_name.clone(),
            self.network.clone(),
            self.compression_config,
            self.config_poller_config,
            self.kv_config,
            self.http_config,
            self.tcp_keep_alive_time,
            if self.orphan_response_handler.is_some() { "Some" } else { "None" },
            if self.retry_manager.is_some() { "Some" } else { "None" },
        )
    }
}
