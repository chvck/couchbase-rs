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

use crate::mgmtx::metakv2::{MetaKv2Entry, MetaKv2Revision};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CreateScopeResponse {
    pub manifest_uid: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DeleteScopeResponse {
    pub manifest_uid: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CreateCollectionResponse {
    pub manifest_uid: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UpdateCollectionResponse {
    pub manifest_uid: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DeleteCollectionResponse {
    pub manifest_uid: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct GetMetaKv2DirResponse {
    /// The position in the store's single global log that this listing reflects.
    /// One read is a consistent snapshot across keys, though not necessarily a
    /// fresh one.
    pub revision: MetaKv2Revision,
    /// Every leaf under the directory, keyed by absolute path. Directory nodes
    /// are not listed; a directory with no leaves under it yields an empty map,
    /// which is distinct from the directory never having existed (that is an
    /// error).
    pub entries: BTreeMap<String, MetaKv2Entry>,
}

/// How a metakv2 mutation ended.
#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct MetaKv2MutationResponse {
    /// The revision every key the call actually changed now carries. Keys whose
    /// value did not differ are not included and keep their old revisions.
    ///
    /// `None` means nothing differed: the store did nothing and issued no
    /// revision at all.
    pub revision: Option<MetaKv2Revision>,
}
