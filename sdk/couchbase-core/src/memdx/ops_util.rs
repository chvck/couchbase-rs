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

use crate::memdx::client::ResponseContext;
use crate::memdx::dispatcher::Dispatcher;
use crate::memdx::error::{Error, Result};
use crate::memdx::ext_frame_code::ExtReqFrameCode;
use crate::memdx::extframe;
use crate::memdx::magic::Magic;
use crate::memdx::opcode::OpCode;
use crate::memdx::packet::RequestPacket;
use crate::memdx::pendingop::StandardPendingOp;
use crate::memdx::request::{
    GetAllVbSeqnosRequest, GetCollectionIdRequest, PingRequest, StatsRequest,
};
use crate::memdx::response::{
    GetAllVbSeqnosResponse, GetCollectionIdResponse, PingResponse, StatsResponse,
};

#[derive(Debug, Default, Clone, Copy)]
pub struct OpsUtil {
    pub ext_frames_enabled: bool,
}

impl OpsUtil {
    fn encode_req_ext_frames(
        &self,
        on_behalf_of: Option<&str>,
        buf: &mut [u8],
    ) -> Result<(Magic, usize)> {
        let mut offset = 0;

        if let Some(obo) = on_behalf_of {
            if !self.ext_frames_enabled {
                return Err(Error::new_invalid_argument_error(
                    "cannot use framing extras when its not enabled",
                    "on_behalf_of".to_string(),
                ));
            }
            extframe::append_ext_frame(
                ExtReqFrameCode::OnBehalfOf,
                obo.as_bytes(),
                buf,
                &mut offset,
            )?;
        }

        let magic = if offset > 0 {
            Magic::ReqExt
        } else {
            Magic::Req
        };

        Ok((magic, offset))
    }

    /// Dispatch a `STAT`, which answers with a **stream** of packets.
    ///
    /// `is_persistent` is `true`: the opaque stays registered after the first
    /// reply, and the caller reads until a response reports
    /// [`StatsResponse::is_end`]. Dropping the returned op unregisters the
    /// opaque, so it must not be dropped while the server still has entries to
    /// send -- see `KvClientOps::stats`.
    pub async fn stats<D>(
        &self,
        dispatcher: &D,
        request: StatsRequest<'_>,
    ) -> Result<StandardPendingOp<StatsResponse>>
    where
        D: Dispatcher,
    {
        let mut ext_frame_buf = [0; 128];
        let (magic, used) = self.encode_req_ext_frames(request.on_behalf_of, &mut ext_frame_buf)?;
        let framing_extras = if used > 0 {
            Some(&ext_frame_buf[..used])
        } else {
            None
        };

        let key = if request.group_name.is_empty() {
            None
        } else {
            Some(request.group_name.as_bytes())
        };

        let op = dispatcher
            .dispatch(
                RequestPacket {
                    magic,
                    op_code: OpCode::Stat,
                    datatype: 0,
                    vbucket_id: None,
                    cas: None,
                    extras: None,
                    key,
                    value: None,
                    framing_extras,
                    opaque: None,
                },
                true,
                None,
            )
            .await?;

        Ok(StandardPendingOp::new(op))
    }

    /// Dispatch `GET_ALL_VB_SEQNOS`, the binary alternative to a `stats
    /// vbucket-seqno` sweep: ten bytes per vbucket rather than eight text
    /// fields per vbucket a node holds -- replicas included.
    pub async fn get_all_vb_seqnos<D>(
        &self,
        dispatcher: &D,
        request: GetAllVbSeqnosRequest<'_>,
    ) -> Result<StandardPendingOp<GetAllVbSeqnosResponse>>
    where
        D: Dispatcher,
    {
        let mut ext_frame_buf = [0; 128];
        let (magic, used) = self.encode_req_ext_frames(request.on_behalf_of, &mut ext_frame_buf)?;
        let framing_extras = if used > 0 {
            Some(&ext_frame_buf[..used])
        } else {
            None
        };

        let extras = request.extras();

        let op = dispatcher
            .dispatch(
                RequestPacket {
                    magic,
                    op_code: OpCode::GetAllVBSeqnos,
                    datatype: 0,
                    vbucket_id: None,
                    cas: None,
                    extras: Some(&extras),
                    key: None,
                    value: None,
                    framing_extras,
                    opaque: None,
                },
                false,
                None,
            )
            .await?;

        Ok(StandardPendingOp::new(op))
    }

    pub async fn get_collection_id<D>(
        &self,
        dispatcher: &D,
        request: GetCollectionIdRequest<'_>,
    ) -> Result<StandardPendingOp<GetCollectionIdResponse>>
    where
        D: Dispatcher,
    {
        let full_name = format!("{}.{}", request.scope_name, request.collection_name);

        let op = dispatcher
            .dispatch(
                RequestPacket {
                    magic: Magic::Req,
                    op_code: OpCode::GetCollectionId,
                    datatype: 0,
                    vbucket_id: None,
                    cas: None,
                    extras: None,
                    key: None,
                    value: Some(full_name.as_bytes()),
                    framing_extras: None,
                    opaque: None,
                },
                false,
                Some(ResponseContext {
                    cas: None,
                    subdoc_info: None,
                    scope_name: Some(request.scope_name.to_string()),
                    collection_name: Some(request.collection_name.to_string()),
                }),
            )
            .await?;

        Ok(StandardPendingOp::new(op))
    }

    /// Dispatch a `NOOP`, which is what a KV ping is.
    ///
    /// The on-behalf-of frame is the only thing the request carries. It used to
    /// be dropped here -- the parameter was named `_request` -- so a ping "as
    /// another user" ran as the connection's own user and reported success,
    /// which is the one answer a permission probe must never invent.
    pub async fn ping<D>(
        &self,
        dispatcher: &D,
        request: PingRequest<'_>,
    ) -> Result<StandardPendingOp<PingResponse>>
    where
        D: Dispatcher,
    {
        let mut ext_frame_buf = [0; 128];
        let (magic, used) = self.encode_req_ext_frames(request.on_behalf_of, &mut ext_frame_buf)?;
        let framing_extras = if used > 0 {
            Some(&ext_frame_buf[..used])
        } else {
            None
        };

        let op = dispatcher
            .dispatch(
                RequestPacket {
                    magic,
                    op_code: OpCode::Noop,
                    datatype: 0,
                    vbucket_id: None,
                    cas: None,
                    extras: None,
                    key: None,
                    value: None,
                    framing_extras,
                    opaque: None,
                },
                false,
                None,
            )
            .await?;

        Ok(StandardPendingOp::new(op))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::Address;
    use crate::memdx::client::Client;
    use crate::memdx::codec::HEADER_SIZE;
    use crate::memdx::connection::{ConnectOptions, ConnectionType, TcpConnection};
    use crate::memdx::dispatcher::DispatcherOptions;
    use crate::memdx::error::ErrorKind;
    use futures::future::BoxFuture;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::time::Instant;

    /// The bytes a ping actually puts on the wire.
    ///
    /// A real `Client` over a loopback socket, and the request read back off it
    /// raw. The only claim worth pinning here is what the server would read:
    /// asserting on the `RequestPacket` this file builds would restate the code
    /// rather than test it, and the encode of the framing block -- its length
    /// byte, and the header field that says how long it is -- is where a frame
    /// that is "present" can still not arrive.
    async fn ping_on_the_wire(ops_util: OpsUtil, request: PingRequest<'_>) -> Result<Vec<u8>> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            let mut frame = vec![0u8; HEADER_SIZE];
            socket.read_exact(&mut frame).await.unwrap();

            let body_len = u32::from_be_bytes(frame[8..12].try_into().unwrap()) as usize;
            frame.resize(HEADER_SIZE + body_len, 0);
            socket.read_exact(&mut frame[HEADER_SIZE..]).await.unwrap();

            frame
        });

        let conn = TcpConnection::connect(
            Address {
                host: addr.ip().to_string(),
                port: addr.port(),
            },
            ConnectOptions {
                deadline: Instant::now() + Duration::from_secs(5),
                tcp_keep_alive_time: Duration::from_secs(30),
            },
        )
        .await
        .unwrap();

        let (on_read_close_tx, _on_read_close_rx) = oneshot::channel();
        let client = Client::new(
            ConnectionType::Tcp(conn),
            DispatcherOptions {
                unsolicited_packet_handler: Arc::new(|_packet| {
                    Box::pin(async {}) as BoxFuture<'static, ()>
                }),
                orphan_handler: None,
                on_read_close_tx,
                disable_decompression: false,
                id: "ops-util-test".to_string(),
            },
        );

        // Held until the request has been read: dropping the operation
        // unregisters its opaque, and the reply never comes anyway.
        let _op = ops_util.ping(&client, request).await?;

        Ok(tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("the ping never reached the wire")
            .unwrap())
    }

    /// **The on-behalf-of frame reaches the server.**
    ///
    /// `ping` used to name its parameter `_request` and drop it, so a ping "as
    /// another user" ran as the connection's own user and reported success --
    /// the one answer a permission probe must never invent. The bytes below are
    /// the protocol's, not this code's: an alternative-magic request, a framing
    /// block whose length the header declares, and inside it frame code `0x4`
    /// with a seven byte body.
    #[tokio::test]
    async fn a_ping_on_behalf_of_a_user_carries_that_user_to_the_server() {
        let frame = ping_on_the_wire(
            OpsUtil {
                ext_frames_enabled: true,
            },
            PingRequest::new().on_behalf_of("someone"),
        )
        .await
        .expect("a ping with framing extras enabled should dispatch");

        let mut expected_framing = vec![0x47];
        expected_framing.extend_from_slice(b"someone");

        assert_eq!(
            frame[0],
            u8::from(Magic::ReqExt),
            "a request carrying framing extras must use the alternative magic"
        );
        assert_eq!(frame[1], u8::from(OpCode::Noop));
        assert_eq!(
            frame[2] as usize,
            expected_framing.len(),
            "the header must declare the framing block's length"
        );
        assert_eq!(frame[3], 0, "a ping has no key");
        assert_eq!(
            u32::from_be_bytes(frame[8..12].try_into().unwrap()) as usize,
            expected_framing.len(),
            "the framing block is the whole body"
        );
        assert_eq!(&frame[HEADER_SIZE..], expected_framing.as_slice());
    }

    /// A ping with no identity is unchanged: the plain magic, and no body at
    /// all. The two key-length bytes of a plain request occupy what is the
    /// framing length in the alternative form, so this is what says the frame
    /// is absent rather than empty.
    #[tokio::test]
    async fn a_plain_ping_carries_no_framing_extras() {
        let frame = ping_on_the_wire(
            OpsUtil {
                ext_frames_enabled: true,
            },
            PingRequest::new(),
        )
        .await
        .expect("a plain ping should dispatch");

        assert_eq!(frame[0], u8::from(Magic::Req));
        assert_eq!(frame[1], u8::from(OpCode::Noop));
        assert_eq!(&frame[2..4], &[0, 0], "a ping has no key");
        assert_eq!(frame.len(), HEADER_SIZE, "a plain ping has no body");
    }

    /// The connection has to have negotiated alternative requests, and says so
    /// rather than sending a request the server will read as malformed -- the
    /// same refusal `stats` gives, and the reason `ping` cannot simply always
    /// set the frame.
    #[tokio::test]
    async fn a_ping_on_behalf_of_a_user_needs_framing_extras_negotiated() {
        let err = ping_on_the_wire(
            OpsUtil {
                ext_frames_enabled: false,
            },
            PingRequest::new().on_behalf_of("someone"),
        )
        .await
        .expect_err("a ping should refuse an identity it cannot encode");

        assert!(
            matches!(err.kind(), ErrorKind::InvalidArgument { arg, .. } if arg.as_deref() == Some("on_behalf_of")),
            "unexpected error: {err}"
        );
    }
}
