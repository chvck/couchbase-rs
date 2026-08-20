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

/// One stat, and the node that reported it.
///
/// **Bytes rather than `String`s, and a shared endpoint name.** A sweep of the
/// default group is hundreds of entries per node; the key and value are slices of
/// the packet they arrived in and the endpoint is one allocation per node, so
/// reading a sweep costs nothing per stat. Turning either into a `String` is the
/// caller's decision to pay for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatsEntry {
    /// The node that reported it, as `host:port`.
    pub endpoint: Arc<str>,
    pub key: Bytes,
    pub value: Bytes,
}

impl StatsEntry {
    /// The stat's name, with invalid UTF-8 replaced.
    pub fn key_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.key)
    }

    /// The stat's value, with invalid UTF-8 replaced.
    pub fn value_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.value)
    }
}

/// What a completed sweep covered.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StatsResult {
    /// How many nodes answered.
    pub endpoints: usize,
    /// How many entries were delivered to the callback.
    pub entries: usize,
}

/// What the KV service measures about one collection.
///
/// **Two numbers are all it publishes**, and everything a caller reports
/// beyond them is derived from them, constant, or absent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CollectionStats {
    /// Documents, summed across every node.
    pub count: u64,
    /// Bytes those documents occupy, summed across every node.
    pub size: u64,
}
