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
use std::time::Duration;

use crate::memdx::ops_rangescan::{
    RangeScanCreateRandomSamplingConfig, RangeScanCreateRangeScanConfig,
    RangeScanCreateSnapshotRequirements,
};
use crate::retry::{RetryStrategy, DEFAULT_RETRY_STRATEGY};

/// Open a scan on **one vbucket**.
///
/// A whole-collection scan is a fan-out of one of these per vbucket -- documents
/// are placed by hash of the key rather than by key order, so a key range prunes
/// nothing and every vbucket has to be asked. [`crate::agent::Agent::num_vbuckets`]
/// is how many that is.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RangeScanCreateOptions<'a> {
    pub scope_name: &'a str,
    pub collection_name: &'a str,
    pub vbucket_id: u16,

    /// Return keys without their values.
    pub keys_only: bool,

    /// Exactly one of `range` and `sampling` must be set.
    pub range: Option<RangeScanCreateRangeScanConfig<'a>>,
    pub sampling: Option<RangeScanCreateRandomSamplingConfig>,

    /// Pin the scan to a point in the vbucket's history, so a fan-out reads one
    /// consistent snapshot rather than each vbucket's latest state.
    pub snapshot: Option<RangeScanCreateSnapshotRequirements>,

    pub retry_strategy: Arc<dyn RetryStrategy>,
}

impl<'a> RangeScanCreateOptions<'a> {
    pub fn new(scope_name: &'a str, collection_name: &'a str, vbucket_id: u16) -> Self {
        Self {
            scope_name,
            collection_name,
            vbucket_id,
            keys_only: false,
            range: None,
            sampling: None,
            snapshot: None,
            retry_strategy: DEFAULT_RETRY_STRATEGY.clone(),
        }
    }

    pub fn keys_only(mut self, keys_only: bool) -> Self {
        self.keys_only = keys_only;
        self
    }

    pub fn range(mut self, range: RangeScanCreateRangeScanConfig<'a>) -> Self {
        self.range = Some(range);
        self
    }

    pub fn sampling(mut self, sampling: RangeScanCreateRandomSamplingConfig) -> Self {
        self.sampling = Some(sampling);
        self
    }

    pub fn snapshot(mut self, snapshot: RangeScanCreateSnapshotRequirements) -> Self {
        self.snapshot = Some(snapshot);
        self
    }

    pub fn retry_strategy(mut self, retry_strategy: Arc<dyn RetryStrategy>) -> Self {
        self.retry_strategy = retry_strategy;
        self
    }
}

/// Read the next batch from an open scan.
///
/// Zero means "no limit, the server decides", which is the usual choice: the
/// server chunks its answer either way, and a limit here only adds round trips.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct RangeScanContinueOptions {
    pub max_count: u32,
    pub max_bytes: u32,
    /// A server-side deadline for this continue, distinct from the client's.
    pub timeout: Option<Duration>,
}

impl RangeScanContinueOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn max_count(mut self, max_count: u32) -> Self {
        self.max_count = max_count;
        self
    }

    pub fn max_bytes(mut self, max_bytes: u32) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct RangeScanCancelOptions {}

impl RangeScanCancelOptions {
    pub fn new() -> Self {
        Self::default()
    }
}
