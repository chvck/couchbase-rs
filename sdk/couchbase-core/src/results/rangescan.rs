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

use std::sync::Arc;

use bytes::Bytes;

use crate::error::{Error, Result};
use crate::kvclient::StdKvClient;
use crate::kvclient_ops::KvClientOps;
use crate::memdx::client::Client;
use crate::memdx::ops_rangescan::{
    RangeScanCancelRequest, RangeScanContinueRequest, RangeScanContinueResponse,
};
use crate::options::rangescan::{RangeScanCancelOptions, RangeScanContinueOptions};

/// An open scan on one vbucket.
///
/// **The connection is part of the handle, not looked up again per call.** A scan
/// lives on the connection that created it: the server releases it when that
/// connection goes away, so continuing it anywhere else is not the same scan.
/// Holding the client here is also what settles which connection manager a scan
/// uses -- the choice is made once, at create, and the whole drain follows it.
pub struct RangeScanCreateResult {
    pub scan_uuid: Bytes,
    pub vbucket_id: u16,

    client: Arc<StdKvClient<Client>>,
}

/// Where the scan stands after one continue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RangeScanContinueResult {
    /// The server has more to give and this continue is finished: send another.
    pub more: bool,
    /// The scan is finished. The server has already released it, so there is
    /// nothing left to cancel.
    pub complete: bool,
}

impl RangeScanCreateResult {
    pub(crate) fn new(scan_uuid: Bytes, vbucket_id: u16, client: Arc<StdKvClient<Client>>) -> Self {
        Self {
            scan_uuid,
            vbucket_id,
            client,
        }
    }

    /// Send one continue and read every packet it answers with.
    ///
    /// `data_cb` is called once per response packet, and each packet's items are
    /// an iterator over that packet's own buffer -- so reading a scan costs
    /// nothing per document, and copying a key or a value out is the caller's
    /// decision to pay for.
    ///
    /// Loop until [`RangeScanContinueResult::complete`], not until `more` goes
    /// false: a continue that is still streaming reports neither, so a loop that
    /// stops on `!more` stops early.
    pub async fn continue_scan<F>(
        &self,
        opts: &RangeScanContinueOptions,
        data_cb: F,
    ) -> Result<RangeScanContinueResult>
    where
        F: FnMut(RangeScanContinueResponse) + Send,
    {
        let summary = self
            .client
            .range_scan_continue(
                RangeScanContinueRequest {
                    scan_uuid: &self.scan_uuid,
                    vbucket_id: self.vbucket_id,
                    max_count: opts.max_count,
                    max_bytes: opts.max_bytes,
                    timeout: opts.timeout,
                    on_behalf_of: None,
                },
                data_cb,
            )
            .await
            .map_err(Error::new_contextual_memdx_error)?;

        Ok(RangeScanContinueResult {
            more: summary.more,
            complete: summary.complete,
        })
    }

    /// Release a scan that will not be drained.
    ///
    /// A scan the server has reported `complete` for is already gone; cancelling
    /// it answers "not found".
    pub async fn cancel(&self, _opts: &RangeScanCancelOptions) -> Result<()> {
        self.client
            .range_scan_cancel(RangeScanCancelRequest {
                scan_uuid: &self.scan_uuid,
                vbucket_id: self.vbucket_id,
                on_behalf_of: None,
            })
            .await
            .map_err(Error::new_contextual_memdx_error)?;

        Ok(())
    }
}

impl std::fmt::Debug for RangeScanCreateResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RangeScanCreateResult")
            .field("scan_uuid", &self.scan_uuid)
            .field("vbucket_id", &self.vbucket_id)
            .finish_non_exhaustive()
    }
}
