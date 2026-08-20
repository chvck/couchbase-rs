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
use crate::memdx::request::{GetCollectionIdRequest, PingRequest, StatsRequest};
use crate::memdx::response::{GetCollectionIdResponse, PingResponse, StatsResponse};

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

    pub async fn ping<D>(
        &self,
        dispatcher: &D,
        _request: PingRequest<'_>,
    ) -> Result<StandardPendingOp<PingResponse>>
    where
        D: Dispatcher,
    {
        let op = dispatcher
            .dispatch(
                RequestPacket {
                    magic: Magic::Req,
                    op_code: OpCode::Noop,
                    datatype: 0,
                    vbucket_id: None,
                    cas: None,
                    extras: None,
                    key: None,
                    value: None,
                    framing_extras: None,
                    opaque: None,
                },
                false,
                None,
            )
            .await?;

        Ok(StandardPendingOp::new(op))
    }
}
