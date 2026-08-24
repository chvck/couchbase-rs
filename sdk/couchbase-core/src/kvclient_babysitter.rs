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
use crate::authenticator::Authenticator;
use crate::connection_state::ConnectionState;
use crate::error::{Error, ErrorKind};
use crate::kvclient::{
    KvClient, KvClientBootstrapOptions, KvClientOptions, OnKvClientCloseHandler,
    UnsolicitedPacketSender,
};
use crate::kvclient_ops::{KvClientOps, ReconfigureAuthenticatorRequest};
use crate::memdx::dispatcher::OrphanResponseHandler;
use crate::memdx::op_auth_saslauto::Credentials;
use crate::memdx::op_bootstrap::BootstrapOptions;
use crate::memdx::packet::ResponsePacket;
use crate::orphan_reporter::OrphanContext;
use crate::results::diagnostics::EndpointDiagnostics;
use crate::service_type::ServiceType;
use crate::tls_config::TlsConfig;
use crate::tracingcomponent::TracingComponent;
use crate::{authenticator, error, kvclient};
use arc_swap::ArcSwap;
use chrono::Utc;
use futures_core::future::BoxFuture;
use std::error::Error as stdError;
use std::future::Future;
use std::mem::take;
use std::ops::{Add, Sub};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::select;
use tokio::sync::mpsc::{Sender, UnboundedReceiver, UnboundedSender};
use tokio::sync::{mpsc, oneshot, watch, MutexGuard};
use tokio::time::{sleep, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub(crate) struct KvTarget {
    pub address: Address,
    pub canonical_address: Address,
    pub tls_config: Option<TlsConfig>,
}

/// What a babysitter tells its pool when its connection state changes.
pub(crate) struct KvClientStateChange<K> {
    pub babysitter_id: String,
    pub client: Option<Arc<K>>,
    /// Why the most recent connect attempt failed, when one has.
    ///
    /// Carried so the pool can answer for an endpoint on behalf of a babysitter
    /// other than the one it happened to pick. Cleared by the next success,
    /// because it arrives alongside the client rather than separately.
    pub connect_err: Option<Error>,
}

pub(crate) type KvClientStateChangeHandler<K> = UnboundedSender<KvClientStateChange<K>>;

pub(crate) trait KvClientBabysitter {
    type Client: KvClient + KvClientOps + Send + Sync;

    fn new(opts: KvClientBabysitterOptions<Self::Client>) -> Self;
    fn id(&self) -> &str;
    fn get_client(&self) -> impl Future<Output = error::Result<Arc<Self::Client>>> + Send;
    fn endpoint_diagnostics(&self) -> EndpointDiagnostics;
    fn update_auth(&self, authenticator: Authenticator) -> impl Future<Output = ()> + Send;
    fn update_target(&self, target: KvTarget) -> impl Future<Output = ()> + Send;
    // async fn update_selected_bucket(&self, bucket_name: String);
    fn close(&self) -> impl Future<Output = error::Result<()>> + Send;
}

#[derive(Clone)]
pub(crate) struct KvClientBabysitterClientConfig {
    pub target: KvTarget,
    pub auth: Authenticator,
    pub selected_bucket: Option<String>,
}

pub(crate) struct KvClientBabysitterOptions<K: KvClient> {
    pub id: String,

    pub on_demand_connect: bool,
    pub surface_connect_errors: bool,
    pub connect_throttle_period: Duration,
    pub disable_decompression: bool,
    pub bootstrap_opts: KvClientBootstrapOptions,
    pub endpoint_id: String,

    pub state_change_handler: KvClientStateChangeHandler<K>,

    pub unsolicited_packet_tx: Option<UnsolicitedPacketSender>,
    pub orphan_handler: Option<OrphanResponseHandler>,

    pub target: KvTarget,
    pub auth: Authenticator,
    pub selected_bucket: Option<String>,
    pub tracing: Arc<TracingComponent>,
}

#[derive(Debug, Clone)]
struct ConnectionError {
    pub connect_error: Error,
    pub connect_error_time: Instant,
}

struct StdKvClientBabysitterState<K: KvClient> {
    // current_config: Option<KvClientBabysitterClientConfig>,
    desired_config: KvClientBabysitterClientConfig,
    connect_err: Option<ConnectionError>,
    client: Option<Arc<K>>,
    current_state: ConnectionState,
    is_building: bool,
}

struct StdKvClientBabysitterClientState<K: KvClient> {
    client: Option<Arc<K>>,
}

#[derive(Clone)]
struct StaticKvClientOptions {
    pub bootstrap_options: KvClientBootstrapOptions,

    pub disable_decompression: bool,
    pub unsolicited_packet_tx: Option<UnsolicitedPacketSender>,
    pub orphan_handler: Option<OrphanResponseHandler>,
}

struct ClientThreadOptions<K: KvClient> {
    id: String,
    endpoint_id: String,
    on_demand_connect: bool,

    connect_throttle_period: Duration,

    static_kv_client_options: StaticKvClientOptions,

    // on_client_close_tx: watch::Sender<String>,
    state_change_handler: KvClientStateChangeHandler<K>,
    on_client_connected_tx: watch::Sender<Option<Arc<K>>>,

    fast_client: Arc<ArcSwap<StdKvClientBabysitterClientState<K>>>,
    slow_state: Arc<Mutex<StdKvClientBabysitterState<K>>>,
    shutdown_token: CancellationToken,
    tracing: Arc<TracingComponent>,
}

pub(crate) struct StdKvClientBabysitter<K: KvClient> {
    id: String,
    endpoint_id: String,
    on_demand_connect: bool,
    surface_connect_errors: bool,

    connect_throttle_period: Duration,

    state_change_handler: KvClientStateChangeHandler<K>,

    kv_client_options: StaticKvClientOptions,

    fast_client: Arc<ArcSwap<StdKvClientBabysitterClientState<K>>>,
    slow_state: Arc<Mutex<StdKvClientBabysitterState<K>>>,
    on_client_connected_tx: watch::Sender<Option<Arc<K>>>,

    shutdown_token: CancellationToken,
    tracing: Arc<TracingComponent>,
}

impl<K: KvClient + 'static> StdKvClientBabysitter<K> {
    fn maybe_begin_client(client_opts: Arc<ClientThreadOptions<K>>) -> bool {
        {
            let mut state = client_opts.slow_state.lock().unwrap();
            if state.is_building {
                return false;
            }

            state.is_building = true;
        }

        Self::begin_client_build(client_opts);
        true
    }

    async fn maybe_throttle_on_error(
        babysitter_id: &str,
        throttle_period: Duration,
        connection_error: Option<ConnectionError>,
        shutdown_token: &CancellationToken,
    ) -> error::Result<()> {
        if let Some(e) = connection_error {
            let elapsed = e.connect_error_time.elapsed();
            if elapsed < throttle_period {
                let to_sleep = throttle_period.sub(elapsed);
                debug!(
                    "Client babysitter {} throttling new connection attempt for {:?}",
                    &babysitter_id, to_sleep
                );
                return select! {
                    _ = shutdown_token.cancelled() => {
                        debug!("Client babysitter {babysitter_id} shutdown notified during throttle sleep");
                        Err(ErrorKind::Shutdown.into())
                    }
                    _ = sleep(to_sleep) => Ok(()),
                };
            }
        }

        Ok(())
    }

    async fn create_client_with_shutdown(
        babysitter_id: &str,
        opts: KvClientOptions,
        shutdown_token: &CancellationToken,
    ) -> error::Result<K> {
        select! {
            _ = shutdown_token.cancelled() => {
                debug!("Client babysitter {babysitter_id} shutdown notified during client creation");
                Err(ErrorKind::Shutdown.into())
            }
            c = K::new(opts) => c,
        }
    }

    fn begin_client_build(client_opts: Arc<ClientThreadOptions<K>>) {
        let state = client_opts.slow_state.clone();

        let desired_config = {
            let guard = state.lock().unwrap();

            guard.desired_config.clone()
        };

        let opts = KvClientOptions {
            address: desired_config.target.clone(),
            authenticator: desired_config.auth.clone(),
            selected_bucket: desired_config.selected_bucket.clone(),
            bootstrap_options: client_opts
                .static_kv_client_options
                .bootstrap_options
                .clone(),
            endpoint_id: client_opts.endpoint_id.clone(),
            unsolicited_packet_tx: client_opts
                .static_kv_client_options
                .unsolicited_packet_tx
                .clone(),
            orphan_handler: client_opts.static_kv_client_options.orphan_handler.clone(),
            on_close_tx: None,
            disable_decompression: client_opts.static_kv_client_options.disable_decompression,
            tracing: client_opts.tracing.clone(),
            id: String::new(),
        };

        tokio::spawn(async move {
            loop {
                let connect_err = {
                    let mut guard = state.lock().unwrap();
                    guard.connect_err.clone()
                };
                if Self::maybe_throttle_on_error(
                    &client_opts.id,
                    client_opts.connect_throttle_period,
                    connect_err,
                    &client_opts.shutdown_token,
                )
                .await
                .is_err()
                {
                    debug!(
                        "Client babysitter {} shutdown during connection throttling",
                        &client_opts.id
                    );
                    return;
                };

                let client_id = Uuid::new_v4().to_string();
                info!(
                    "Client babysitter {} creating kvclient {}",
                    &client_opts.id, &client_id
                );
                let (on_close_tx, mut on_close_rx) = mpsc::channel(1);

                let opts = {
                    let mut guard = state.lock().unwrap();
                    guard.current_state = ConnectionState::Connecting;

                    let mut opts = opts.clone();
                    opts.authenticator = guard.desired_config.auth.clone();
                    opts.address = guard.desired_config.target.clone();
                    opts.selected_bucket = guard.desired_config.selected_bucket.clone();
                    opts.on_close_tx = Some(on_close_tx);
                    opts.id = client_id.clone();

                    opts
                };

                match Self::create_client_with_shutdown(
                    &client_opts.id,
                    opts,
                    &client_opts.shutdown_token,
                )
                .await
                {
                    Ok(client) => {
                        let client = Arc::new(client);
                        debug!(
                            "Client babysitter {} changing client {} connection state to Connected",
                            &client_opts.id,
                            client.id()
                        );

                        {
                            let mut guard = state.lock().unwrap();
                            guard.is_building = false;
                            guard.current_state = ConnectionState::Connected;
                            guard.client = Some(client.clone());
                            // This connection works, so the last failure is
                            // history: it must not be handed to a later caller
                            // as though it were still the state of things, nor
                            // throttle the next attempt after this one drops.
                            guard.connect_err = None;
                        }

                        client_opts
                            .fast_client
                            .store(Arc::new(StdKvClientBabysitterClientState {
                                client: Some(client.clone()),
                            }));

                        match client_opts
                            .on_client_connected_tx
                            .send(Some(client.clone()))
                        {
                            Ok(_) => {}
                            Err(_e) => {
                                // This only happens if there are no receivers, which is only possible
                                // when called from new and is fine.
                            }
                        }

                        if let Err(e) = client_opts.state_change_handler.send(KvClientStateChange {
                            babysitter_id: client_opts.id.clone(),
                            client: Some(client),
                            connect_err: None,
                        }) {
                            debug!(
                                "Client babysitter {} failed to notify of new client {}",
                                &client_opts.id, e
                            );
                        }

                        // Spawn the close-watcher only after a successful connection.
                        // This prevents failed bootstrap attempts from prematurely
                        // consuming the close signal and killing the watcher.
                        let on_close_opts = client_opts.clone();
                        tokio::spawn(async move {
                            select! {
                                _ = on_close_opts.shutdown_token.cancelled() => {
                                    debug!("Client babysitter {} shutdown during on_close wait", &on_close_opts.id);
                                    return;
                                }
                                _ = on_close_rx.recv() => {
                                    debug!("Client babysitter {} detected client {} closed", &on_close_opts.id, &client_id);
                                }
                            };

                            {
                                let mut guard = on_close_opts.slow_state.lock().unwrap();
                                guard.is_building = false;
                                if let Some(cli) = &guard.client {
                                    if cli.id() != client_id {
                                        return;
                                    }
                                } else {
                                    return;
                                }

                                guard.client = None;
                                on_close_opts.fast_client.store(Arc::new(
                                    StdKvClientBabysitterClientState { client: None },
                                ));
                            }

                            if let Err(e) =
                                on_close_opts
                                    .state_change_handler
                                    .send(KvClientStateChange {
                                        babysitter_id: on_close_opts.id.clone(),
                                        client: None,
                                        connect_err: None,
                                    })
                            {
                                debug!(
                                    "Client babysitter {} failed to notify of closed client {}: {}",
                                    &on_close_opts.id, &client_id, e
                                );
                            }

                            if !on_close_opts.on_demand_connect {
                                Self::maybe_begin_client(on_close_opts.clone());
                            }
                        });

                        return;
                    }
                    Err(e) => {
                        client_opts
                            .fast_client
                            .store(Arc::new(StdKvClientBabysitterClientState { client: None }));
                        let mut msg = format!(
                            "Client babysitter {} error creating new client {}",
                            client_opts.id, e
                        );
                        if *e.kind() == ErrorKind::Shutdown {
                            return;
                        }

                        if let Some(source) = e.source() {
                            msg = format!("{msg} - {source}");
                        }
                        info!("{msg}");

                        {
                            let mut guard = state.lock().unwrap();

                            guard.current_state = ConnectionState::Disconnected;
                            guard.connect_err = Some(ConnectionError {
                                connect_error: e.clone(),
                                connect_error_time: Instant::now(),
                            });
                        }

                        // The pool keeps its own copy, so that a caller asking it
                        // for any connection can be answered by this failure even
                        // when it picked one of our neighbours.
                        let _ = client_opts.state_change_handler.send(KvClientStateChange {
                            babysitter_id: client_opts.id.clone(),
                            client: None,
                            connect_err: Some(e),
                        });

                        // Wake anyone in get_client now that the failure is
                        // recorded. Success is not the only news worth waking
                        // for: without this a caller sleeps until the next
                        // attempt succeeds, which is exactly the wait that
                        // `surface_connect_errors` exists to cut short. The
                        // value stays None, so a waiter that does not want the
                        // error simply goes back to waiting.
                        let _ = client_opts.on_client_connected_tx.send(None);
                    }
                }
            }
        });
    }

    async fn get_client_for_reauth(
        fast_client: Arc<ArcSwap<StdKvClientBabysitterClientState<K>>>,
        slow_state: Arc<Mutex<StdKvClientBabysitterState<K>>>,
        on_client_connected_tx: watch::Sender<Option<Arc<K>>>,
        shutdown_token: CancellationToken,
    ) -> error::Result<Arc<K>> {
        let state = fast_client.load();
        if let Some(client) = &state.client {
            return Ok(client.clone());
        }

        {
            let guard = slow_state.lock().unwrap();
            if let Some(client) = &guard.client {
                return Ok(client.clone());
            }
        }

        let mut rx = on_client_connected_tx.subscribe();

        loop {
            let changed = select! {
                () = shutdown_token.cancelled() => {
                    return Err(Error::new_message_error("client babysitter shutdown"))
                },
                (res) = rx.changed() => res
            };

            match changed {
                Ok(_) => {
                    if let Some(client) = rx.borrow_and_update().clone() {
                        return Ok(client);
                    }
                }
                Err(e) => {}
            }
        }
    }
}

impl<K: KvClient + KvClientOps + 'static> KvClientBabysitter for StdKvClientBabysitter<K> {
    type Client = K;

    fn new(opts: KvClientBabysitterOptions<K>) -> StdKvClientBabysitter<K> {
        let (on_client_connected_tx, _) = watch::channel(None);
        let babysitter = StdKvClientBabysitter {
            id: opts.id.clone(),
            endpoint_id: opts.endpoint_id.clone(),
            on_demand_connect: opts.on_demand_connect,
            surface_connect_errors: opts.surface_connect_errors,
            connect_throttle_period: opts.connect_throttle_period,
            state_change_handler: opts.state_change_handler,
            on_client_connected_tx,
            kv_client_options: StaticKvClientOptions {
                bootstrap_options: opts.bootstrap_opts,
                unsolicited_packet_tx: opts.unsolicited_packet_tx,
                orphan_handler: opts.orphan_handler,
                disable_decompression: opts.disable_decompression,
            },
            fast_client: Arc::new(ArcSwap::from_pointee(StdKvClientBabysitterClientState {
                client: None,
            })),
            slow_state: Arc::new(Mutex::new(StdKvClientBabysitterState {
                // current_config: None,
                desired_config: KvClientBabysitterClientConfig {
                    target: opts.target,
                    auth: opts.auth,
                    selected_bucket: opts.selected_bucket,
                },
                connect_err: None,
                client: None,
                current_state: ConnectionState::Disconnected,
                is_building: false,
            })),
            shutdown_token: CancellationToken::new(),
            tracing: opts.tracing.clone(),
        };

        if !opts.on_demand_connect {
            debug!(
                "Client babysitter {} starting to build new client",
                &opts.id
            );

            Self::maybe_begin_client(Arc::new(ClientThreadOptions {
                id: babysitter.id.clone(),
                endpoint_id: opts.endpoint_id,
                on_demand_connect: opts.on_demand_connect,
                connect_throttle_period: babysitter.connect_throttle_period,
                static_kv_client_options: babysitter.kv_client_options.clone(),
                state_change_handler: babysitter.state_change_handler.clone(),
                on_client_connected_tx: babysitter.on_client_connected_tx.clone(),
                fast_client: babysitter.fast_client.clone(),
                slow_state: babysitter.slow_state.clone(),
                shutdown_token: babysitter.shutdown_token.clone(),
                tracing: babysitter.tracing.clone(),
            }));
        }

        babysitter
    }

    fn id(&self) -> &str {
        &self.id
    }

    async fn get_client(&self) -> error::Result<Arc<K>> {
        let state = self.fast_client.load();
        if let Some(client) = &state.client {
            return Ok(client.clone());
        }

        {
            let guard = self.slow_state.lock().unwrap();
            if let Some(client) = &guard.client {
                return Ok(client.clone());
            }
        }

        // We subscribe before possibly creating the new client just to be sure that we're
        // listening for updates.
        let mut rx = self.on_client_connected_tx.subscribe();

        let is_building = Self::maybe_begin_client(Arc::new(ClientThreadOptions {
            id: self.id.clone(),
            endpoint_id: self.endpoint_id.clone(),
            on_demand_connect: self.on_demand_connect,
            connect_throttle_period: self.connect_throttle_period,
            static_kv_client_options: self.kv_client_options.clone(),
            state_change_handler: self.state_change_handler.clone(),
            on_client_connected_tx: self.on_client_connected_tx.clone(),
            fast_client: self.fast_client.clone(),
            slow_state: self.slow_state.clone(),
            shutdown_token: self.shutdown_token.clone(),
            tracing: self.tracing.clone(),
        }));

        if is_building {
            debug!("Client babysitter {} starting to rebuild client", &self.id);
        } else {
            debug!("Client babysitter {} already building client", &self.id);
        }

        loop {
            // A connect that has already failed is worth answering with. Waiting
            // is the right move while a connection is on its way back, but it
            // cannot tell that apart from a password the server will keep
            // refusing -- and that wait has no bound in this crate. When the
            // embedder asks, hand over the reason the last attempt failed and let
            // the caller decide, which is what gocbcorex does.
            //
            // Read before waiting, so a stale failure answers immediately rather
            // than after whatever the reconnect in flight does next. The lock is
            // released before the select: it is held by the connect thread too.
            if self.surface_connect_errors {
                let connect_err = {
                    let guard = self.slow_state.lock().unwrap();
                    guard.connect_err.as_ref().map(|e| e.connect_error.clone())
                };

                if let Some(err) = connect_err {
                    debug!(
                        "Client babysitter {} answering with stored connect error: {}",
                        &self.id, &err
                    );
                    // **Said, not merely handed back.** The attempt's own error
                    // does not record that it came from connecting, and a caller
                    // cannot reliably infer it: a refused dial is
                    // `ConnectionFailed`, but a password the server refuses is
                    // `Server(UnknownStatus { status: AuthError })`, which is
                    // shaped like a status returned to the operation itself. Left
                    // raw it is also *classified* as one -- `error_to_retry_reason`
                    // puts `UnknownStatus` to the error map, so a rotated password
                    // was retried or not depending on what the server said about
                    // `0x20`. This is the provenance the pool has and the caller
                    // cannot reconstruct.
                    return Err(Error::new_connect_failed_error(
                        self.endpoint_id.clone(),
                        err,
                    ));
                }
            }

            let changed = select! {
                () = self.shutdown_token.cancelled() => {
                    return Err(Error::new_message_error("client babysitter shutdown"))
                },
                (res) = rx.changed() => res
            };

            match changed {
                Ok(_) => {
                    if let Some(client) = rx.borrow_and_update().clone() {
                        return Ok(client);
                    }
                }
                Err(e) => {
                    debug!(
                        "Client babysitter {} failed to wait for client to become available: {}",
                        &self.id, e
                    );

                    return Err(Error::new_message_error(format!(
                        "client babysitter failed to wait for client to become available {e}"
                    )));
                }
            }
        }
    }

    fn endpoint_diagnostics(&self) -> EndpointDiagnostics {
        let state = self.slow_state.lock().unwrap();

        let connection_state = state.current_state;

        let (local_address, last_activity) = match &state.client {
            Some(cli) => (
                Some(cli.local_addr().to_string()),
                Some(
                    Utc::now()
                        .sub(cli.last_activity().to_utc())
                        .num_microseconds()
                        .unwrap_or_default(),
                ),
            ),
            None => (None, None),
        };

        EndpointDiagnostics {
            service_type: ServiceType::MEMD,
            id: self.id.to_string(),
            local_address,
            remote_address: state.desired_config.target.address.to_string(),
            last_activity,
            namespace: state.desired_config.selected_bucket.clone(),
            state: connection_state,
            last_connect_error: state
                .connect_err
                .as_ref()
                .map(|e| e.connect_error.to_string()),
        }
    }

    async fn update_auth(&self, authenticator: Authenticator) {
        {
            let mut guard = self.slow_state.lock().unwrap();
            guard.desired_config.auth = authenticator.clone();
        }

        if let Authenticator::JwtAuthenticator(jwt) = authenticator {
            // We will attempt to reauth the existing client, if we're bootstrapping already
            // then we don't know at what point that's already at so we'll always reauth that
            // new client.
            let fast_client = self.fast_client.clone();
            let slow_state_clone = self.slow_state.clone();
            let mut tx = self.on_client_connected_tx.clone();
            let shutdown = self.shutdown_token.clone();

            tokio::spawn(async move {
                if let Ok(client) = StdKvClientBabysitter::get_client_for_reauth(
                    fast_client,
                    slow_state_clone,
                    tx.clone(),
                    shutdown,
                )
                .await
                {
                    if let Err(e) = client
                        .reconfigure_authenticator(ReconfigureAuthenticatorRequest {
                            credentials: Credentials::JwtToken(jwt.token),
                        })
                        .await
                    {
                        warn!("Error during reauth in babysitter {}: {}", client.id(), e);
                        if let Err(e) = client.close().await {
                            warn!("Error during close after failed reauth in babysitter {}", e);
                        }
                    }
                }
            });
        }
    }

    async fn update_target(&self, target: KvTarget) {
        let mut guard = self.slow_state.lock().unwrap();
        guard.desired_config.target = target;
    }

    async fn close(&self) -> error::Result<()> {
        info!("Closing babysitter {}", self.id);
        self.shutdown_token.cancel();

        let client = {
            let mut guard = self.slow_state.lock().unwrap();

            self.fast_client
                .store(Arc::new(StdKvClientBabysitterClientState { client: None }));

            take(&mut guard.client)
        };

        if let Some(client) = client {
            client.close().await?;
        }

        self.state_change_handler.send(KvClientStateChange {
            babysitter_id: self.id.clone(),
            client: None,
            connect_err: None,
        });

        Ok(())
    }
}

impl<K: KvClient> Drop for StdKvClientBabysitter<K> {
    fn drop(&mut self) {
        self.shutdown_token.cancel();
        info!("Dropping StdKvClientBabysitter {}", self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::Address;
    use crate::authenticator::PasswordAuthenticator;
    use crate::kvclient::{KvClientBootstrapOptions, OnErrMapFetchedHandler, StdKvClient};
    use crate::memdx::client::Client;
    use crate::tracingcomponent::TracingComponentConfig;
    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio::time::timeout;

    type TestClient = StdKvClient<Client>;

    /// A port nothing listens on, so every connect attempt fails, and fails fast.
    ///
    /// Taken by binding and releasing rather than picked, because a hardcoded
    /// port that something else happens to hold would make this pass for the
    /// wrong reason.
    async fn closed_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    async fn babysitter_for(
        surface_connect_errors: bool,
    ) -> (
        StdKvClientBabysitter<TestClient>,
        UnboundedReceiver<KvClientStateChange<TestClient>>,
    ) {
        let address = Address {
            host: "127.0.0.1".to_string(),
            port: closed_port().await,
        };

        babysitter_against(
            address,
            "user".to_string(),
            "pass".to_string(),
            surface_connect_errors,
            None,
        )
        .await
    }

    async fn babysitter_against(
        address: Address,
        username: String,
        password: String,
        surface_connect_errors: bool,
        on_err_map_fetched: Option<OnErrMapFetchedHandler>,
    ) -> (
        StdKvClientBabysitter<TestClient>,
        UnboundedReceiver<KvClientStateChange<TestClient>>,
    ) {
        // Held and handed back: dropping the receiver would only lose a state
        // change this test never reads, but keeping it is closer to the real
        // arrangement.
        let (state_change_handler, state_change_rx) = tokio::sync::mpsc::unbounded_channel();

        let babysitter = KvClientBabysitter::new(KvClientBabysitterOptions {
            id: "test-babysitter".to_string(),
            // On demand, so that get_client is what drives the connect attempt.
            on_demand_connect: true,
            surface_connect_errors,
            connect_throttle_period: Duration::from_millis(50),
            disable_decompression: false,
            bootstrap_opts: KvClientBootstrapOptions {
                client_name: "test".to_string(),
                disable_error_map: false,
                disable_mutation_tokens: false,
                disable_server_durations: false,
                on_err_map_fetched,
                tcp_keep_alive_time: Duration::from_secs(60),
                auth_mechanisms: vec![],
                connect_timeout: Duration::from_millis(500),
            },
            endpoint_id: "test-endpoint".to_string(),
            state_change_handler,
            unsolicited_packet_tx: None,
            orphan_handler: None,
            target: KvTarget {
                address: address.clone(),
                canonical_address: address,
                tls_config: None,
            },
            auth: Authenticator::PasswordAuthenticator(PasswordAuthenticator {
                username,
                password,
            }),
            selected_bucket: None,
            tracing: Arc::new(TracingComponent::new(TracingComponentConfig {
                cluster_labels: None,
            })),
        });

        (babysitter, state_change_rx)
    }

    #[tokio::test]
    async fn surfaced_connect_errors_answer_the_caller() {
        let (babysitter, _state_change_rx) = babysitter_for(true).await;

        let res = timeout(Duration::from_secs(5), babysitter.get_client())
            .await
            .expect("get_client should answer with the connect error, not wait for a reconnect");

        let err = match res {
            Ok(_) => panic!("connecting to a closed port cannot have succeeded"),
            Err(e) => e,
        };

        // **The kind, not just that it failed.** Answering the caller is only
        // half of it: an error indistinguishable from a server's answer leaves
        // the caller doing kind archaeology to find out what it was told, and
        // gets classified as an answer by `error_to_retry_reason` on the way.
        assert!(
            matches!(err.kind(), ErrorKind::ConnectFailed { .. }),
            "a surfaced connect error has to say that is what it is: {err}"
        );
    }

    /// **What a password the server refuses actually looks like.**
    ///
    /// Against a real KV endpoint rather than a closed port, because a closed
    /// port can only produce a dial failure and the shape that matters here is
    /// the other one: SASL is refused by a server that *answered*, so the
    /// failure arrives carrying a memcached status and looking exactly like a
    /// status returned to an operation. That is the reason a caller cannot
    /// classify a surfaced connect error by reading kinds, and the reason this
    /// crate marks it instead.
    ///
    /// Skipped unless `RCBKVADDR` names a live endpoint as `host:port`.
    #[tokio::test]
    async fn a_refused_password_surfaces_as_a_connect_failure() {
        let Some(addr) = std::env::var("RCBKVADDR").ok().filter(|a| !a.is_empty()) else {
            eprintln!("RCBKVADDR is not set; skipping the live-cluster probe");
            return;
        };
        let (host, port) = addr.rsplit_once(':').expect("RCBKVADDR must be host:port");
        let address = Address {
            host: host.to_string(),
            port: port.parse().expect("RCBKVADDR port must be a number"),
        };

        let (babysitter, _rx) = babysitter_against(
            address,
            std::env::var("RCBUSER").unwrap_or_else(|_| "Administrator".to_string()),
            "definitely-not-the-password".to_string(),
            true,
            None,
        )
        .await;

        let res = timeout(Duration::from_secs(10), babysitter.get_client())
            .await
            .expect("a refused password must be answered, not waited on");

        let err = match res {
            Ok(_) => panic!("a wrong password cannot have authenticated"),
            Err(e) => e,
        };

        assert!(
            matches!(err.kind(), ErrorKind::ConnectFailed { .. }),
            "a refused password has to arrive as a connect failure: {err}"
        );

        // **And the cause is why the wrapper exists.** Measured against Couchbase
        // 8.x, a refused SASL step arrives as
        // `Server(UnknownStatus { status: AuthError })` -- a memcached status,
        // shaped exactly like one returned to a data operation, and put to the
        // server's own error map by `error_to_retry_reason` on the way past. The
        // status is asserted rather than the whole kind, because the *kind* is
        // only `UnknownStatus` for as long as `OpsCore::decode_error` has no arm
        // for `0x20`, and that is this crate's business to change.
        let ErrorKind::ConnectFailed { source, .. } = err.kind() else {
            unreachable!()
        };
        let rendered = source.cause().to_string();
        assert!(
            rendered.contains("0x20"),
            "a refused password should carry the auth status: {rendered}"
        );
    }

    #[tokio::test]
    async fn diagnostics_carry_the_last_connect_error() {
        // Built with the option off, to show that reporting the cause does not
        // depend on it: an operation still waits, a support ticket still says why.
        let (babysitter, _state_change_rx) = babysitter_for(false).await;

        assert!(
            babysitter
                .endpoint_diagnostics()
                .last_connect_error
                .is_none(),
            "nothing has been attempted yet, so there is nothing to report"
        );

        // Drives an attempt that cannot succeed. Waiting is what the default
        // does, so let the wait time out rather than expecting an answer.
        let _ = timeout(Duration::from_millis(600), babysitter.get_client()).await;

        assert!(
            babysitter
                .endpoint_diagnostics()
                .last_connect_error
                .is_some(),
            "a failed attempt should be reported against the endpoint"
        );
    }

    #[tokio::test]
    async fn the_default_waits_for_a_connection() {
        let (babysitter, _state_change_rx) = babysitter_for(false).await;

        let res = timeout(Duration::from_millis(600), babysitter.get_client()).await;

        assert!(
            res.is_err(),
            "the default must keep waiting for a connection rather than answering with the connect error"
        );
    }
}
