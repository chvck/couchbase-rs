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

use tokio::time::Instant;
use tracing::warn;

use crate::memdx::dispatcher::Dispatcher;
use crate::memdx::error::Result;
use crate::memdx::op_auth_saslauto::{
    OpSASLAutoEncoder, OpsSASLAuthAuto, SASLAuthAutoOptions, SASLAuthAutoProgress,
};
use crate::memdx::pendingop::{
    discard_op_with_deadline, dispatch_op_with_deadline, recv_op_with_deadline, StandardPendingOp,
};
use crate::memdx::request::{
    GetClusterConfigRequest, GetErrorMapRequest, HelloRequest, SelectBucketRequest,
};
use crate::memdx::response::{
    BootstrapResult, GetClusterConfigResponse, GetErrorMapResponse, HelloResponse,
    SelectBucketResponse,
};

pub trait OpBootstrapEncoder {
    fn hello<D>(
        &self,
        dispatcher: &D,
        request: HelloRequest,
    ) -> impl std::future::Future<Output = Result<StandardPendingOp<HelloResponse>>>
    where
        D: Dispatcher;

    fn get_error_map<D>(
        &self,
        dispatcher: &D,
        request: GetErrorMapRequest,
    ) -> impl std::future::Future<Output = Result<StandardPendingOp<GetErrorMapResponse>>>
    where
        D: Dispatcher;

    fn select_bucket<D>(
        &self,
        dispatcher: &D,
        request: SelectBucketRequest,
    ) -> impl std::future::Future<Output = Result<StandardPendingOp<SelectBucketResponse>>>
    where
        D: Dispatcher;

    fn get_cluster_config<D>(
        &self,
        dispatcher: &D,
        request: GetClusterConfigRequest,
    ) -> impl std::future::Future<Output = Result<StandardPendingOp<GetClusterConfigResponse>>>
    where
        D: Dispatcher;
}

pub struct OpBootstrap {}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct BootstrapOptions {
    pub hello: Option<HelloRequest>,
    pub get_error_map: Option<GetErrorMapRequest>,
    pub auth: Option<SASLAuthAutoOptions>,
    pub select_bucket: Option<SelectBucketRequest>,
    pub deadline: Instant,
    pub get_cluster_config: Option<GetClusterConfigRequest>,
}

/// The bootstrap requests that have been written but not yet read.
///
/// Held together so that a path which gives up part way through can read the rest away.
/// Dropping a dispatched op unregisters its opaque, and the reply that is already coming
/// back then has nowhere to go and is reported as an orphan.
#[derive(Default)]
struct PendingBootstrap {
    hello: Option<StandardPendingOp<HelloResponse>>,
    error_map: Option<StandardPendingOp<GetErrorMapResponse>>,
    select_bucket: Option<StandardPendingOp<SelectBucketResponse>>,
    cluster_config: Option<StandardPendingOp<GetClusterConfigResponse>>,
}

impl PendingBootstrap {
    async fn discard(mut self, deadline: Instant) {
        if let Some(op) = self.hello.take() {
            discard_op_with_deadline(deadline, op).await;
        }
        if let Some(op) = self.error_map.take() {
            discard_op_with_deadline(deadline, op).await;
        }
        self.discard_post_auth(deadline).await;
    }

    async fn discard_post_auth(&mut self, deadline: Instant) {
        if let Some(op) = self.select_bucket.take() {
            discard_op_with_deadline(deadline, op).await;
        }
        if let Some(op) = self.cluster_config.take() {
            discard_op_with_deadline(deadline, op).await;
        }
    }
}

impl OpBootstrap {
    /// Runs the bootstrap sequence, in as few round trips as the authentication mechanism
    /// allows.
    ///
    /// Every request in a batch is written before any of the batch's responses is read, so
    /// a batch costs one round trip instead of one per request. `Hello`, `GetErrorMap` and
    /// the first authentication attempt (with `SASLListMechs` beside it) do not read each
    /// other's replies, so they always share the first batch.
    ///
    /// `SelectBucket` and `GetClusterConfig` do depend on being authenticated, but only in
    /// the sense that the server handles a connection's requests in the order they arrive:
    /// they can ride in the same batch as the *last* authentication request. Which batch
    /// that is depends on the mechanism -- the first for PLAIN and OAUTHBEARER, the second
    /// for SCRAM, which cannot compute its final message until the server has answered.
    pub async fn bootstrap<E, D>(
        encoder: E,
        dispatcher: &D,
        opts: BootstrapOptions,
    ) -> Result<BootstrapResult>
    where
        E: OpBootstrapEncoder + OpSASLAutoEncoder,
        D: Dispatcher,
    {
        let deadline = opts.deadline;
        let mut result = BootstrapResult {
            hello: None,
            error_map: None,
            cluster_config: None,
        };

        let mut pending = PendingBootstrap::default();

        if let Some(req) = opts.hello {
            match dispatch_op_with_deadline(deadline, encoder.hello(dispatcher, req)).await {
                Ok(op) => pending.hello = Some(op),
                Err(e) => warn!("Hello failed {e}"),
            }
        };

        if let Some(req) = opts.get_error_map {
            match dispatch_op_with_deadline(deadline, encoder.get_error_map(dispatcher, req)).await
            {
                Ok(op) => pending.error_map = Some(op),
                Err(e) => warn!("Get error map failed {e}"),
            }
        };

        let mut auth = match opts.auth {
            Some(req) => {
                match (OpsSASLAuthAuto {})
                    .dispatch(&encoder, dispatcher, deadline, req)
                    .await
                {
                    Ok(auth) => Some(auth),
                    Err(e) => {
                        warn!("Auth failed {e}");
                        pending.discard(deadline).await;
                        return Err(e);
                    }
                }
            }
            None => None,
        };

        // Without authentication there is nothing for the dependent requests to wait for,
        // so they belong in this batch too.
        let auth_finishes_in_this_batch = match &auth {
            Some(auth) => !auth.needs_another_round_trip(),
            None => true,
        };

        let mut post_auth_dispatched = false;
        if auth_finishes_in_this_batch {
            if let Err(e) = Self::dispatch_post_auth(
                &encoder,
                dispatcher,
                deadline,
                &opts.select_bucket,
                &opts.get_cluster_config,
                &mut pending,
            )
            .await
            {
                if let Some(auth) = auth {
                    auth.discard(deadline).await;
                }
                pending.discard(deadline).await;
                return Err(e);
            }
            post_auth_dispatched = true;
        }

        if let Some(op) = pending.hello.take() {
            match recv_op_with_deadline(deadline, op).await {
                Ok(r) => result.hello = Some(r),
                Err(e) => warn!("Hello failed {e}"),
            }
        };

        if let Some(op) = pending.error_map.take() {
            match recv_op_with_deadline(deadline, op).await {
                Ok(r) => result.error_map = Some(r),
                Err(e) => warn!("Get error map failed {e}"),
            }
        };

        if let Some(mut auth) = auth.take() {
            loop {
                let progress = match auth.resolve(&encoder, dispatcher, deadline).await {
                    Ok(progress) => progress,
                    Err(e) => {
                        warn!("Auth failed {e}");
                        pending.discard(deadline).await;
                        return Err(e);
                    }
                };

                let next = match progress {
                    SASLAuthAutoProgress::Done => break,
                    SASLAuthAutoProgress::Continue(next) => next,
                };

                if post_auth_dispatched {
                    // The attempt those requests rode behind failed, so the server refused
                    // them too. Read the refusals away and send them again with the retry.
                    pending.discard_post_auth(deadline).await;
                    post_auth_dispatched = false;
                }

                if !next.needs_another_round_trip() {
                    if let Err(e) = Self::dispatch_post_auth(
                        &encoder,
                        dispatcher,
                        deadline,
                        &opts.select_bucket,
                        &opts.get_cluster_config,
                        &mut pending,
                    )
                    .await
                    {
                        next.discard(deadline).await;
                        pending.discard(deadline).await;
                        return Err(e);
                    }
                    post_auth_dispatched = true;
                }

                auth = next;
            }
        }

        // A mechanism that finished sooner than its shape suggested -- a SCRAM server that
        // accepts the client-first message outright -- leaves the dependent requests unsent.
        if !post_auth_dispatched {
            Self::dispatch_post_auth(
                &encoder,
                dispatcher,
                deadline,
                &opts.select_bucket,
                &opts.get_cluster_config,
                &mut pending,
            )
            .await?;
        }

        if let Some(op) = pending.select_bucket.take() {
            if let Err(e) = recv_op_with_deadline(deadline, op).await {
                warn!("Select bucket failed {e}");
                pending.discard(deadline).await;
                return Err(e);
            }
        };

        if let Some(op) = pending.cluster_config.take() {
            match recv_op_with_deadline(deadline, op).await {
                Ok(r) => result.cluster_config = Some(r),
                Err(e) => warn!("Get cluster config failed {e}"),
            }
        };

        Ok(result)
    }

    /// Writes the requests that only make sense on an authenticated connection.
    ///
    /// A `SelectBucket` that cannot even be written is fatal, the same way a rejected one
    /// is: everything after it would be addressed to the wrong bucket.
    async fn dispatch_post_auth<E, D>(
        encoder: &E,
        dispatcher: &D,
        deadline: Instant,
        select_bucket: &Option<SelectBucketRequest>,
        get_cluster_config: &Option<GetClusterConfigRequest>,
        pending: &mut PendingBootstrap,
    ) -> Result<()>
    where
        E: OpBootstrapEncoder,
        D: Dispatcher,
    {
        if let Some(req) = select_bucket {
            match dispatch_op_with_deadline(
                deadline,
                encoder.select_bucket(dispatcher, req.clone()),
            )
            .await
            {
                Ok(op) => pending.select_bucket = Some(op),
                Err(e) => {
                    warn!("Select bucket failed {e}");
                    return Err(e);
                }
            }
        }

        if let Some(req) = get_cluster_config {
            match dispatch_op_with_deadline(
                deadline,
                encoder.get_cluster_config(dispatcher, req.clone()),
            )
            .await
            {
                Ok(op) => pending.cluster_config = Some(op),
                Err(e) => warn!("Get cluster config failed {e}"),
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use bytes::Bytes;
    use tokio::sync::mpsc;

    use super::*;
    use crate::memdx::auth_mechanism::AuthMechanism;
    use crate::memdx::client::{OpaqueMap, ResponseContext, ResponseSender};
    use crate::memdx::client_response::ClientResponse;
    use crate::memdx::connection::ConnectionType;
    use crate::memdx::dispatcher::DispatcherOptions;
    use crate::memdx::hello_feature::HelloFeature;
    use crate::memdx::magic::Magic;
    use crate::memdx::op_auth_saslauto::Credentials;
    use crate::memdx::opcode::OpCode;
    use crate::memdx::ops_core::OpsCore;
    use crate::memdx::packet::{RequestPacket, ResponsePacket};
    use crate::memdx::pendingop::ClientPendingOp;
    use crate::memdx::status::Status;

    /// A dispatcher that refuses to answer while the bootstrap is still writing.
    ///
    /// The order of the opcodes on its own proves nothing: a strictly sequential bootstrap
    /// writes the same packets in the same order. What separates the two is *when* the
    /// writes happen relative to the reads, so this mock holds every reply until the
    /// caller has stopped writing and has blocked, then answers everything outstanding at
    /// once and counts that as a round trip.
    ///
    /// The blocking is detected by the clock: with `start_paused`, tokio only advances time
    /// when every task is parked, which in this test means the bootstrap is waiting on a
    /// response it cannot have yet.
    struct MockDispatcher {
        state: Mutex<MockState>,
        next_opaque: AtomicU32,
        supported_mechs: String,
        accepted_mech: String,
    }

    #[derive(Default)]
    struct MockState {
        op_codes: Vec<OpCode>,
        /// Written, and not yet answered.
        in_flight: Vec<(ResponseSender, ResponsePacket)>,
        round_trips: usize,
    }

    impl MockDispatcher {
        fn new(supported_mechs: &str, accepted_mech: &str) -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(MockState::default()),
                next_opaque: AtomicU32::new(1),
                supported_mechs: supported_mechs.to_string(),
                accepted_mech: accepted_mech.to_string(),
            })
        }

        fn reply_to(&self, packet: &RequestPacket, opaque: u32) -> ResponsePacket {
            let (status, value) = match packet.op_code {
                OpCode::Hello => (Status::Success, packet.value.map(Bytes::copy_from_slice)),
                OpCode::GetErrorMap => (Status::Success, Some(Bytes::from_static(b"{}"))),
                OpCode::SASLListMechs => (
                    Status::Success,
                    Some(Bytes::copy_from_slice(self.supported_mechs.as_bytes())),
                ),
                OpCode::SASLAuth => {
                    let mech = String::from_utf8_lossy(packet.key.unwrap_or_default());
                    if mech != self.accepted_mech {
                        (Status::AuthError, None)
                    } else if mech.starts_with("SCRAM") {
                        (
                            Status::AuthContinue,
                            Some(server_first(packet.value.unwrap_or_default())),
                        )
                    } else {
                        (Status::Success, None)
                    }
                }
                OpCode::SASLStep => (
                    Status::Success,
                    Some(Bytes::from_static(b"v=c2VydmVycHJvb2Y=")),
                ),
                OpCode::GetClusterConfig => (Status::Success, Some(Bytes::from_static(b"{}"))),
                _ => (Status::Success, None),
            };

            let mut response = ResponsePacket::new(Magic::Res, packet.op_code, 0, status, opaque);
            response.value = value;

            response
        }

        /// Answers everything outstanding. Returns false when there was nothing to answer,
        /// which is not a round trip.
        fn serve_one_round_trip(&self) -> bool {
            let mut state = self.state.lock().unwrap();
            if state.in_flight.is_empty() {
                return false;
            }

            state.round_trips += 1;
            for (sender, packet) in state.in_flight.drain(..) {
                assert!(
                    sender
                        .try_send(Ok(ClientResponse::new(packet, None)))
                        .is_none(),
                    "the caller's receiver should be waiting for exactly one response",
                );
            }

            true
        }
    }

    impl Dispatcher for MockDispatcher {
        fn new(_conn: ConnectionType, _opts: DispatcherOptions) -> Self {
            unimplemented!("the mock is built directly, not from a connection")
        }

        async fn dispatch<'a>(
            &self,
            packet: RequestPacket<'a>,
            is_persistent: bool,
            _response_context: Option<ResponseContext>,
        ) -> Result<ClientPendingOp> {
            let opaque = self.next_opaque.fetch_add(1, Ordering::SeqCst);
            let response = self.reply_to(&packet, opaque);
            let (sender, receiver) = ResponseSender::new_pair(is_persistent);

            {
                let mut state = self.state.lock().unwrap();
                state.op_codes.push(packet.op_code);
                state.in_flight.push((sender, response));
            }

            Ok(ClientPendingOp::new(
                opaque,
                Arc::new(Mutex::new(OpaqueMap::default())),
                receiver,
            ))
        }

        async fn close(&self) -> Result<()> {
            Ok(())
        }
    }

    /// The server-first message a SCRAM server would answer with. The client checks that
    /// the nonce is prefixed with its own, that the salt is base64 and that the iteration
    /// count parses, and nothing else, so echoing the nonce back is enough.
    fn server_first(client_first: &[u8]) -> Bytes {
        let client_first = String::from_utf8_lossy(client_first);
        let client_nonce = client_first
            .split(',')
            .find_map(|field| field.strip_prefix("r="))
            .expect("the client-first message should carry a nonce");

        Bytes::from(format!(
            "r={client_nonce}ServerNonce,s=dGVzdHNhbHQxMjM0NTY3OA==,i=4096"
        ))
    }

    fn bootstrap_opts(enabled_mechs: &[AuthMechanism]) -> BootstrapOptions {
        let auth = if enabled_mechs.is_empty() {
            None
        } else {
            Some(SASLAuthAutoOptions {
                credentials: Credentials::UserPass {
                    username: "user".to_string(),
                    password: "pass".to_string(),
                },
                enabled_mechs: enabled_mechs.to_vec(),
            })
        };

        BootstrapOptions {
            hello: Some(HelloRequest::new(
                b"test".to_vec(),
                vec![HelloFeature::Snappy],
            )),
            get_error_map: Some(GetErrorMapRequest::new(2)),
            auth,
            select_bucket: Some(SelectBucketRequest::new("test".to_string())),
            deadline: Instant::now() + Duration::from_secs(30),
            get_cluster_config: Some(GetClusterConfigRequest::new()),
        }
    }

    /// Runs a bootstrap against the mock and reports the packets it wrote, in order, and
    /// the number of round trips they took.
    async fn run_bootstrap(
        dispatcher: &Arc<MockDispatcher>,
        opts: BootstrapOptions,
    ) -> (Result<BootstrapResult>, Vec<OpCode>, usize) {
        let server = tokio::spawn({
            let dispatcher = dispatcher.clone();
            async move {
                loop {
                    // Only elapses once every task is parked, i.e. once the bootstrap has
                    // stopped writing and is waiting to be answered.
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    dispatcher.serve_one_round_trip();
                }
            }
        });

        let result = OpBootstrap::bootstrap(OpsCore {}, dispatcher.as_ref(), opts).await;
        server.abort();

        let state = dispatcher.state.lock().unwrap();
        (result, state.op_codes.clone(), state.round_trips)
    }

    #[tokio::test(start_paused = true)]
    async fn bootstrap_without_auth_takes_one_round_trip() {
        let dispatcher = MockDispatcher::new("PLAIN", "PLAIN");
        let (result, op_codes, round_trips) = run_bootstrap(&dispatcher, bootstrap_opts(&[])).await;

        assert!(result.is_ok(), "bootstrap failed: {:?}", result.err());
        assert_eq!(
            op_codes,
            [
                OpCode::Hello,
                OpCode::GetErrorMap,
                OpCode::SelectBucket,
                OpCode::GetClusterConfig,
            ]
        );
        assert_eq!(round_trips, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn bootstrap_with_plain_auth_takes_one_round_trip() {
        let dispatcher = MockDispatcher::new("PLAIN", "PLAIN");
        let (result, op_codes, round_trips) =
            run_bootstrap(&dispatcher, bootstrap_opts(&[AuthMechanism::Plain])).await;

        assert!(result.is_ok(), "bootstrap failed: {:?}", result.err());
        // PLAIN finishes in one exchange, so the requests that need an authenticated
        // connection can ride behind it in the same batch.
        assert_eq!(
            op_codes,
            [
                OpCode::Hello,
                OpCode::GetErrorMap,
                OpCode::SASLListMechs,
                OpCode::SASLAuth,
                OpCode::SelectBucket,
                OpCode::GetClusterConfig,
            ]
        );
        assert_eq!(round_trips, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn bootstrap_with_scram_auth_takes_two_round_trips() {
        let dispatcher = MockDispatcher::new("SCRAM-SHA512", "SCRAM-SHA512");
        let (result, op_codes, round_trips) =
            run_bootstrap(&dispatcher, bootstrap_opts(&[AuthMechanism::ScramSha512])).await;

        assert!(result.is_ok(), "bootstrap failed: {:?}", result.err());
        // SCRAM cannot compute its final message until the server has answered, so the
        // dependent requests wait and go out with the second exchange instead.
        assert_eq!(
            op_codes,
            [
                OpCode::Hello,
                OpCode::GetErrorMap,
                OpCode::SASLListMechs,
                OpCode::SASLAuth,
                OpCode::SASLStep,
                OpCode::SelectBucket,
                OpCode::GetClusterConfig,
            ]
        );
        assert_eq!(round_trips, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn bootstrap_falling_back_to_plain_writes_select_bucket_once() {
        let dispatcher = MockDispatcher::new("PLAIN", "PLAIN");
        let (result, op_codes, round_trips) = run_bootstrap(
            &dispatcher,
            bootstrap_opts(&[AuthMechanism::ScramSha512, AuthMechanism::Plain]),
        )
        .await;

        assert!(result.is_ok(), "bootstrap failed: {:?}", result.err());
        // The preferred mechanism needs two exchanges, so nothing rides behind it and the
        // fallback attempt is the first one the dependent requests can join. Writing them
        // optimistically in the first batch instead would leave a select-bucket reply with
        // no reader, and the orphan reporter would log it.
        assert_eq!(
            op_codes,
            [
                OpCode::Hello,
                OpCode::GetErrorMap,
                OpCode::SASLListMechs,
                OpCode::SASLAuth,
                OpCode::SASLAuth,
                OpCode::SelectBucket,
                OpCode::GetClusterConfig,
            ]
        );
        assert_eq!(round_trips, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn bootstrap_falling_back_to_scram_reissues_the_dependent_requests() {
        let dispatcher = MockDispatcher::new("SCRAM-SHA512", "SCRAM-SHA512");
        let (result, op_codes, round_trips) = run_bootstrap(
            &dispatcher,
            bootstrap_opts(&[AuthMechanism::Plain, AuthMechanism::ScramSha512]),
        )
        .await;

        assert!(result.is_ok(), "bootstrap failed: {:?}", result.err());
        // PLAIN looked like it would finish in one exchange, so select-bucket went out
        // behind it. When it failed the server refused those too, so they are read away
        // and written again behind the last exchange of the mechanism that worked.
        assert_eq!(
            op_codes,
            [
                OpCode::Hello,
                OpCode::GetErrorMap,
                OpCode::SASLListMechs,
                OpCode::SASLAuth,
                OpCode::SelectBucket,
                OpCode::GetClusterConfig,
                OpCode::SASLAuth,
                OpCode::SASLStep,
                OpCode::SelectBucket,
                OpCode::GetClusterConfig,
            ]
        );
        assert_eq!(round_trips, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn bootstrap_fails_when_no_mechanism_is_shared_with_the_server() {
        let dispatcher = MockDispatcher::new("SCRAM-SHA1", "SCRAM-SHA1");
        let (result, _op_codes, _round_trips) =
            run_bootstrap(&dispatcher, bootstrap_opts(&[AuthMechanism::Plain])).await;

        assert!(result.is_err());
    }
}
