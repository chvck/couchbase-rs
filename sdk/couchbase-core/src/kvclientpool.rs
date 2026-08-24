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

use std::backtrace::Backtrace;
use std::collections::HashMap;
use std::error::Error as StdError;
use std::future::Future;
use std::net::SocketAddr;
use std::ops::{Deref, Sub};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::authenticator::Authenticator;
use crate::connection_state::ConnectionState;
use crate::error;
use crate::error::Result;
use crate::error::{Error, ErrorKind};
use crate::kvclient::{
    KvClient, KvClientBootstrapOptions, KvClientOptions, OnErrMapFetchedHandler,
    OnKvClientCloseHandler, UnsolicitedPacketSender,
};
use crate::kvclient_babysitter::{
    KvClientBabysitter, KvClientBabysitterOptions, KvClientStateChange, KvTarget,
};
use crate::kvclient_ops::KvClientOps;
use crate::memdx::dispatcher::{Dispatcher, OrphanResponseHandler, UnsolicitedPacketHandler};
use crate::memdx::request::PingRequest;
use crate::memdx::response::PingResponse;
use crate::results::diagnostics::EndpointDiagnostics;
use crate::tracingcomponent::TracingComponent;
use arc_swap::ArcSwap;
use futures::executor::block_on;
use futures::future::join_all;
use tokio::select;
use tokio::sync::mpsc::{Sender, UnboundedReceiver};
use tokio::sync::{broadcast, Mutex, MutexGuard, Notify};
use tokio::time::{sleep, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use urlencoding::decode_binary;
use uuid::Uuid;

pub(crate) trait KvClientPool: Send + Sync {
    type Client: KvClient + KvClientOps + Send + Sync;

    fn new(opts: KvClientPoolOptions) -> impl Future<Output = Self> + Send;
    fn id(&self) -> &str;
    fn get_client(&self) -> impl Future<Output = Result<Arc<Self::Client>>> + Send;
    fn ping_all_clients(
        &self,
        req: PingRequest,
    ) -> impl Future<Output = Vec<Result<PingResponse>>> + Send;
    fn endpoint_diagnostics(&self) -> impl Future<Output = Vec<EndpointDiagnostics>> + Send;
    fn update_auth(&self, authenticator: Authenticator) -> impl Future<Output = ()> + Send;
    fn update_target(&self, target: KvTarget) -> impl Future<Output = ()> + Send;
    // async fn update_selected_bucket(&self, bucket_name: String);
    fn close(&self) -> impl Future<Output = Result<()>> + Send;
}

pub(crate) struct KvClientPoolOptions {
    pub id: String,
    pub num_connections: usize,
    pub connect_throttle_period: Duration,
    pub disable_decompression: bool,
    pub bootstrap_options: KvClientBootstrapOptions,
    pub endpoint_id: String,
    pub on_demand_connect: bool,
    pub surface_connect_errors: bool,

    pub target: KvTarget,
    pub auth: Authenticator,
    pub selected_bucket: Option<String>,

    pub unsolicited_packet_tx: Option<UnsolicitedPacketSender>,
    pub orphan_handler: Option<OrphanResponseHandler>,
    pub tracing: Arc<TracingComponent>,
}

struct KvClientPoolFastMap<K> {
    clients: Vec<Arc<K>>,
}

#[derive(Clone)]
struct KvClientPoolEntry<B, K>
where
    B: KvClientBabysitter,
    K: KvClient + KvClientOps,
{
    babysitter: Arc<B>,
    client: Option<Arc<K>>,
    connect_err: Option<Error>,
}

impl<B, K> Drop for KvClientPoolEntry<B, K>
where
    B: KvClientBabysitter,
    K: KvClient + KvClientOps,
{
    fn drop(&mut self) {
        debug!("Dropping KvClientPoolEntry");
    }
}

pub(crate) struct StdKvClientPool<B, K>
where
    B: KvClientBabysitter,
    K: KvClient + KvClientOps,
{
    shutdown_token: CancellationToken,
    id: String,

    client_idx: AtomicUsize,
    surface_connect_errors: bool,
    fast_map: Arc<ArcSwap<KvClientPoolFastMap<K>>>,

    babysitters: Arc<Mutex<Vec<KvClientPoolEntry<B, K>>>>,
}

impl<B, K> KvClientPool for StdKvClientPool<B, K>
where
    B: KvClientBabysitter<Client = K> + Send + 'static + std::marker::Sync,
    K: KvClient + KvClientOps + 'static,
{
    type Client = K;

    async fn new(opts: KvClientPoolOptions) -> Self {
        let id = opts.id;
        info!(
            "Creating new client pool {} for {}",
            &id, &opts.target.address
        );

        let fast_map = Arc::new(ArcSwap::from_pointee(KvClientPoolFastMap {
            clients: vec![],
        }));

        let babysitters: Arc<Mutex<Vec<KvClientPoolEntry<B, K>>>> =
            Arc::new(Mutex::new(Vec::with_capacity(opts.num_connections)));

        let (state_change_tx, mut state_change_rx) = tokio::sync::mpsc::unbounded_channel();

        {
            let mut babysitters_guard = babysitters.lock().await;
            for idx in 0..opts.num_connections {
                let babysitter_id = Uuid::new_v4().to_string();
                info!(
                    "Client pool {} creating babysitter {} (idx={})",
                    &id, &babysitter_id, idx
                );
                let babysitter = KvClientBabysitter::new(KvClientBabysitterOptions {
                    id: babysitter_id,
                    endpoint_id: opts.endpoint_id.clone(),
                    on_demand_connect: opts.on_demand_connect,
                    surface_connect_errors: opts.surface_connect_errors,

                    connect_throttle_period: opts.connect_throttle_period,
                    disable_decompression: opts.disable_decompression,
                    bootstrap_opts: opts.bootstrap_options.clone(),
                    state_change_handler: state_change_tx.clone(),
                    unsolicited_packet_tx: opts.unsolicited_packet_tx.clone(),
                    orphan_handler: opts.orphan_handler.clone(),
                    target: opts.target.clone(),
                    auth: opts.auth.clone(),
                    selected_bucket: opts.selected_bucket.clone(),
                    tracing: opts.tracing.clone(),
                });

                babysitters_guard.insert(
                    idx,
                    KvClientPoolEntry {
                        babysitter: Arc::new(babysitter),
                        client: None,
                        connect_err: None,
                    },
                );
            }
        }

        let shutdown_token = CancellationToken::new();

        let babysitters_clone = babysitters.clone();
        let fast_map_clone = fast_map.clone();
        let shutdown_token_clone = shutdown_token.clone();
        let id_clone = id.clone();
        tokio::spawn(async move {
            loop {
                let change = select! {
                    Some(change) = state_change_rx.recv() => {
                        debug!("Client pool {} received state change for babysitter {}, has client: {}", &id_clone, &change.babysitter_id, change.client.is_some());
                        change
                    },
                    _ = shutdown_token_clone.cancelled() => {
                        debug!("Client pool {} state change handler shutting down", &id_clone);
                        return;
                    }
                };

                let mut guard = babysitters_clone.lock().await;

                let entry = guard
                    .iter_mut()
                    .find(|entry| entry.babysitter.id() == change.babysitter_id);
                if let Some(entry) = entry {
                    entry.client = change.client;
                    entry.connect_err = change.connect_err;
                }

                let mut clients = vec![];
                for entry in guard.iter() {
                    if let Some(client) = &entry.client {
                        clients.push(client.clone());
                    }
                }

                fast_map_clone.store(Arc::new(KvClientPoolFastMap { clients }));
            }
        });

        StdKvClientPool {
            id,
            client_idx: Default::default(),
            surface_connect_errors: opts.surface_connect_errors,
            fast_map,
            babysitters,
            shutdown_token,
        }
    }

    fn id(&self) -> &str {
        &self.id
    }

    async fn get_client(&self) -> Result<Arc<K>> {
        let fast_map = self.fast_map.load();
        let num_fast_map_connections = fast_map.clients.len();
        if num_fast_map_connections > 0 {
            let client_idx = self.client_idx.fetch_add(1, Ordering::Relaxed);
            let client = fast_map.clients[client_idx % num_fast_map_connections].clone();
            return Ok(client);
        }

        self.get_client_slow().await
    }

    async fn ping_all_clients(&self, req: PingRequest<'_>) -> Vec<Result<PingResponse>> {
        let mut babysitters = vec![];
        {
            let guard = self.babysitters.lock().await;

            for babysitter_entry in guard.iter() {
                babysitters.push(babysitter_entry.babysitter.clone())
            }
        }

        let mut pool_handles = Vec::with_capacity(babysitters.len());
        for babysitter in babysitters {
            let req = req.clone();
            let handle = async move {
                let client = babysitter.get_client().await?;
                client
                    .ping(req)
                    .await
                    .map_err(Error::new_contextual_memdx_error)
            };

            pool_handles.push(handle);
        }

        join_all(pool_handles).await
    }

    async fn endpoint_diagnostics(&self) -> Vec<EndpointDiagnostics> {
        let babysitters = self.babysitters.lock().await;

        let mut diags = vec![];
        for babysitter_entry in babysitters.iter() {
            diags.push(babysitter_entry.babysitter.endpoint_diagnostics());
        }

        diags
    }

    async fn update_auth(&self, authenticator: Authenticator) {
        let babysitters = self.babysitters.lock().await;
        for babysitter_entry in babysitters.iter() {
            babysitter_entry
                .babysitter
                .update_auth(authenticator.clone())
                .await;
        }
    }

    async fn update_target(&self, target: KvTarget) {
        let babysitters = self.babysitters.lock().await;
        for babysitter_entry in babysitters.iter() {
            babysitter_entry
                .babysitter
                .update_target(target.clone())
                .await;
        }
    }

    async fn close(&self) -> Result<()> {
        info!("Closing pool {}", self.id);

        self.shutdown_token.cancel();

        self.fast_map
            .swap(Arc::new(KvClientPoolFastMap { clients: vec![] }));

        let mut babysitters = self.babysitters.lock().await;
        for babysitter_entry in babysitters.drain(..) {
            if let Err(e) = babysitter_entry.babysitter.close().await {
                debug!("Failed to close babysitter: {e:?}");
            }
        }

        Ok(())
    }
}

impl<B, K> StdKvClientPool<B, K>
where
    B: KvClientBabysitter<Client = K>,
    K: KvClient + KvClientOps,
{
    async fn get_client_slow(&self) -> Result<Arc<K>> {
        let babysitter = {
            let babysitters = self.babysitters.lock().await;
            // A pool configured with no connections has nothing to hand out, and
            // asking for one used to divide by zero. It is a reachable setting:
            // the bulk manager is switched off by giving it none.
            if babysitters.is_empty() {
                return Err(Error::new_message_error(format!(
                    "client pool {} has no connections configured",
                    &self.id
                )));
            }
            // Nothing is connected, so prefer a babysitter that knows why over
            // one that is still trying: the round-robin pick could otherwise sit
            // through a dial that takes the whole connect timeout while its
            // neighbour was refused immediately. gocbcorex reads every manager
            // for the same reason, and delegates instead when there is only one
            // to read.
            //
            // Only when asked. Without the option a babysitter keeps its reason
            // to itself and waiting is the answer, so the pool must not go behind
            // its back and hand one over.
            if self.surface_connect_errors
                && babysitters.len() > 1
                && babysitters.iter().all(|entry| entry.client.is_none())
            {
                if let Some(err) = babysitters
                    .iter()
                    .find_map(|entry| entry.connect_err.clone())
                {
                    return Err(err);
                }
            }

            let client_idx = self.client_idx.fetch_add(1, Ordering::Relaxed);

            babysitters[client_idx % babysitters.len()]
                .babysitter
                .clone()
        };

        babysitter.get_client().await
    }
}

impl<B, K> Drop for StdKvClientPool<B, K>
where
    B: KvClientBabysitter,
    K: KvClient + KvClientOps,
{
    fn drop(&mut self) {
        self.shutdown_token.cancel();
        info!("Dropping StdKvClientPool {}", self.id,);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::Address;
    use crate::authenticator::PasswordAuthenticator;
    use crate::kvclient::StdKvClient;
    use crate::kvclient_babysitter::StdKvClientBabysitter;
    use crate::memdx::client::Client;
    use crate::tracingcomponent::{TracingComponent, TracingComponentConfig};
    use std::time::Instant;
    use tokio::time::timeout;

    type TestClient = StdKvClient<Client>;
    type TestPool = StdKvClientPool<StdKvClientBabysitter<TestClient>, TestClient>;

    /// A port nothing listens on, so a connect attempt fails, and fails fast.
    async fn closed_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    async fn pool_of(
        num_connections: usize,
        surface_connect_errors: bool,
        host: String,
        port: u16,
        connect_timeout: Duration,
    ) -> TestPool {
        let address = Address { host, port };

        TestPool::new(KvClientPoolOptions {
            id: "test-pool".to_string(),
            num_connections,
            connect_throttle_period: Duration::from_millis(50),
            disable_decompression: false,
            bootstrap_options: KvClientBootstrapOptions {
                client_name: "test".to_string(),
                disable_error_map: false,
                disable_mutation_tokens: false,
                disable_server_durations: false,
                on_err_map_fetched: None,
                tcp_keep_alive_time: Duration::from_secs(60),
                auth_mechanisms: vec![],
                connect_timeout,
            },
            endpoint_id: "test-endpoint".to_string(),
            // On demand, so each babysitter dials only once it is asked for.
            on_demand_connect: true,
            surface_connect_errors,
            target: KvTarget {
                address: address.clone(),
                canonical_address: address,
                tls_config: None,
            },
            auth: Authenticator::PasswordAuthenticator(PasswordAuthenticator {
                username: "user".to_string(),
                password: "pass".to_string(),
            }),
            selected_bucket: None,
            unsolicited_packet_tx: None,
            orphan_handler: None,
            tracing: Arc::new(TracingComponent::new(TracingComponentConfig {
                cluster_labels: None,
            })),
        })
        .await
    }

    #[tokio::test]
    async fn a_pool_of_several_answers_with_a_connect_error_when_asked() {
        let pool = pool_of(
            2,
            true,
            "127.0.0.1".to_string(),
            closed_port().await,
            Duration::from_millis(500),
        )
        .await;

        let res = timeout(Duration::from_secs(5), pool.get_client())
            .await
            .expect("the pool should answer with a connect error, not wait for a reconnect");

        assert!(
            res.is_err(),
            "connecting to a closed port cannot have succeeded"
        );
    }

    #[tokio::test]
    async fn a_pool_of_several_waits_by_default() {
        let pool = pool_of(
            2,
            false,
            "127.0.0.1".to_string(),
            closed_port().await,
            Duration::from_millis(500),
        )
        .await;

        let res = timeout(Duration::from_millis(600), pool.get_client()).await;

        assert!(
            res.is_err(),
            "the default must keep waiting for a connection rather than answering with the connect error"
        );
    }

    /// The point of reading every babysitter rather than the one the round-robin
    /// lands on: a neighbour that already knows why answers now, instead of the
    /// pick spending its whole connect timeout finding out for itself.
    ///
    /// Addressed at TEST-NET-1, which is reserved and normally routed nowhere, so
    /// the first attempt spends the timeout. A network that refuses it outright
    /// instead makes the first call fast too, which leaves this asserting less
    /// than it means to -- but never failing for the wrong reason.
    #[tokio::test]
    async fn a_pool_reads_every_babysitter_not_just_the_one_it_picked() {
        let connect_timeout = Duration::from_secs(3);
        let pool = pool_of(2, true, "192.0.2.1".to_string(), 11210, connect_timeout).await;

        // Lands on the first babysitter, and pays for the dial.
        let first = timeout(connect_timeout * 3, pool.get_client()).await;
        assert!(
            first.expect("the first call should answer").is_err(),
            "a reserved address cannot have connected"
        );

        // Would land on the second babysitter, which has never dialled. Its
        // neighbour's recorded failure is the answer.
        let started = Instant::now();
        let second = timeout(connect_timeout * 3, pool.get_client()).await;
        assert!(
            second.expect("the second call should answer").is_err(),
            "a reserved address cannot have connected"
        );

        assert!(
            started.elapsed() < Duration::from_millis(500),
            "the second call took {:?}, so it dialled again rather than reading the failure \
             its neighbour had already recorded",
            started.elapsed()
        );
    }
}
