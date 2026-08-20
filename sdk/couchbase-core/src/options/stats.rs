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

use crate::httpx::request::OnBehalfOfInfo;
use crate::retry::{RetryStrategy, DEFAULT_RETRY_STRATEGY};

/// Sweep `STAT` across every KV node.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct StatsOptions<'a> {
    /// The stat group, or empty for the default set.
    pub group_name: &'a str,
    pub retry_strategy: Arc<dyn RetryStrategy>,
}

impl<'a> StatsOptions<'a> {
    pub fn new(group_name: &'a str) -> Self {
        Self {
            group_name,
            retry_strategy: DEFAULT_RETRY_STRATEGY.clone(),
        }
    }

    pub fn retry_strategy(mut self, retry_strategy: Arc<dyn RetryStrategy>) -> Self {
        self.retry_strategy = retry_strategy;
        self
    }
}

/// Ask KV what it measures about one collection.
///
/// **The group is asked for one collection by name**, `collections
/// <scope>.<collection>`, rather than for all of them and filtered here. It
/// costs one small reply instead of one per collection in the bucket, and it
/// is what makes absence *typed*: a name the bucket does not hold answers
/// `CollectionUnknown` (0x88) or `ScopeUnknown` (0x8c) rather than simply
/// never appearing in a listing.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct CollectionStatsOptions<'a> {
    pub scope_name: &'a str,
    pub collection_name: &'a str,
    pub retry_strategy: Arc<dyn RetryStrategy>,
    /// Ask as this user rather than as the cluster credentials.
    ///
    /// **A stats read is a read of the collection**, so a caller that may not
    /// see it must not learn its document count either — and for a client whose
    /// own map already holds the keyspace name, this is the *only* thing that
    /// consults the server about permission. Without it a gateway answering
    /// `collStats` on a caller's behalf answers as itself.
    ///
    /// KV takes the username alone; the domain travels only on the HTTP form.
    /// [`StatsOptions`] deliberately has no counterpart: the group sweep is a
    /// client's own cluster-wide read, never one made for somebody else.
    pub on_behalf_of: Option<&'a OnBehalfOfInfo>,
}

impl<'a> CollectionStatsOptions<'a> {
    pub fn new(scope_name: &'a str, collection_name: &'a str) -> Self {
        Self {
            scope_name,
            collection_name,
            retry_strategy: DEFAULT_RETRY_STRATEGY.clone(),
            on_behalf_of: None,
        }
    }

    pub fn on_behalf_of(mut self, on_behalf_of: impl Into<Option<&'a OnBehalfOfInfo>>) -> Self {
        self.on_behalf_of = on_behalf_of.into();
        self
    }
}

/// Ask `STAT` of the node holding one vbucket.
///
/// The vbucket picks the node; it is not sent to the server, because `STAT` is a
/// per-node command. Groups like `vbucket-seqno` answer for every vbucket the
/// node holds, so this narrows the sweep to a node rather than to a vbucket.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct StatsByVbucketOptions<'a> {
    pub group_name: &'a str,
    pub vbucket_id: u16,
    pub retry_strategy: Arc<dyn RetryStrategy>,
}

impl<'a> StatsByVbucketOptions<'a> {
    pub fn new(group_name: &'a str, vbucket_id: u16) -> Self {
        Self {
            group_name,
            vbucket_id,
            retry_strategy: DEFAULT_RETRY_STRATEGY.clone(),
        }
    }

    pub fn retry_strategy(mut self, retry_strategy: Arc<dyn RetryStrategy>) -> Self {
        self.retry_strategy = retry_strategy;
        self
    }
}
