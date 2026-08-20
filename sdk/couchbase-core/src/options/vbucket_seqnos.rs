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

use crate::retry::{RetryStrategy, DEFAULT_RETRY_STRATEGY};

/// Read every active vbucket's high sequence number.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct VbucketSeqnosOptions {
    /// Report each vbucket's high seqno **for this collection only**.
    ///
    /// `None` is the bucket-wide high seqno, which makes one collection's
    /// reads wait for another's writes to be indexed. Safe — waiting for more
    /// always is — but a coupling a caller keeping a vector per collection
    /// should not pay for.
    pub collection_id: Option<u32>,
    pub retry_strategy: Arc<dyn RetryStrategy>,
}

impl VbucketSeqnosOptions {
    pub fn new() -> Self {
        Self {
            collection_id: None,
            retry_strategy: DEFAULT_RETRY_STRATEGY.clone(),
        }
    }

    pub fn collection_id(mut self, collection_id: impl Into<Option<u32>>) -> Self {
        self.collection_id = collection_id.into();
        self
    }

    pub fn retry_strategy(mut self, retry_strategy: Arc<dyn RetryStrategy>) -> Self {
        self.retry_strategy = retry_strategy;
        self
    }
}

impl Default for VbucketSeqnosOptions {
    fn default() -> Self {
        Self::new()
    }
}
