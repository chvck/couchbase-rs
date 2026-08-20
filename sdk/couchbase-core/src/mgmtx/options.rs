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

use crate::httpx::client::Client;
use crate::httpx::request::OnBehalfOfInfo;
use crate::mgmtx::bucket_settings::BucketSettings;
use crate::mgmtx::metakv2::{MetaKv2Revision, MetaKv2Write};
use crate::mgmtx::node_target::NodeTarget;
use crate::mgmtx::user::{Group, User};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct GetCollectionManifestOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub bucket_name: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct CreateScopeOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub bucket_name: &'a str,
    pub scope_name: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct DeleteScopeOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub bucket_name: &'a str,
    pub scope_name: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct CreateCollectionOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub bucket_name: &'a str,
    pub scope_name: &'a str,
    pub collection_name: &'a str,
    pub max_ttl: Option<i32>,
    pub history_enabled: Option<bool>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct UpdateCollectionOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub bucket_name: &'a str,
    pub scope_name: &'a str,
    pub collection_name: &'a str,
    pub max_ttl: Option<i32>,
    pub history_enabled: Option<bool>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct DeleteCollectionOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub bucket_name: &'a str,
    pub scope_name: &'a str,
    pub collection_name: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct GetTerseClusterConfigOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct GetFullClusterConfigOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct GetClusterInfoOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct GetTerseBucketConfigOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub bucket_name: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct GetFullBucketConfigOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub bucket_name: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct LoadSampleBucketOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub bucket_name: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct EnsureManifestPollOptions<C: Client> {
    pub client: Arc<C>,
    pub targets: Vec<NodeTarget>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct GetAllBucketsOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct GetBucketOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub bucket_name: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct CreateBucketOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub bucket_name: &'a str,
    pub bucket_settings: &'a BucketSettings,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct UpdateBucketOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub bucket_name: &'a str,
    pub bucket_settings: &'a BucketSettings,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct DeleteBucketOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub bucket_name: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct FlushBucketOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub bucket_name: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct EnsureBucketPollOptions<C: Client> {
    pub client: Arc<C>,
    pub targets: Vec<NodeTarget>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct GetUserOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub username: &'a str,
    pub auth_domain: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct GetAllUsersOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub auth_domain: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct UpsertUserOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub user: &'a User,
    pub auth_domain: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct DeleteUserOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub username: &'a str,
    pub auth_domain: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct GetRolesOptions<'a> {
    pub permission: Option<&'a str>,
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct GetGroupOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub group_name: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct GetAllGroupsOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct UpsertGroupOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub group: &'a Group,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct DeleteGroupOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub group_name: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct ChangePasswordOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub new_password: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct EnsureUserPollOptions<C: Client> {
    pub client: Arc<C>,
    pub targets: Vec<NodeTarget>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct EnsureGroupPollOptions<C: Client> {
    pub client: Arc<C>,
    pub targets: Vec<NodeTarget>,
}

#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct IndexStatusOptions<'a> {
    pub(crate) on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
}

impl<'a> IndexStatusOptions<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_behalf_of_info(
        mut self,
        on_behalf_of: impl Into<Option<&'a OnBehalfOfInfo>>,
    ) -> Self {
        self.on_behalf_of_info = on_behalf_of.into();
        self
    }
}

#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct GetAutoFailoverSettingsOptions<'a> {
    pub(crate) on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
}

impl<'a> GetAutoFailoverSettingsOptions<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_behalf_of_info(
        mut self,
        on_behalf_of: impl Into<Option<&'a OnBehalfOfInfo>>,
    ) -> Self {
        self.on_behalf_of_info = on_behalf_of.into();
        self
    }
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GetBucketStatsOptions<'a> {
    pub(crate) on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub(crate) bucket_name: &'a str,
}

impl<'a> GetBucketStatsOptions<'a> {
    pub fn new(bucket_name: &'a str) -> Self {
        Self {
            bucket_name,
            on_behalf_of_info: None,
        }
    }

    pub fn on_behalf_of_info(
        mut self,
        on_behalf_of: impl Into<Option<&'a OnBehalfOfInfo>>,
    ) -> Self {
        self.on_behalf_of_info = on_behalf_of.into();
        self
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct GetMetaKv2Options<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    /// The absolute path of one leaf. Must not carry a trailing slash.
    pub path: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct GetMetaKv2DirOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    /// The absolute path of a directory. Must carry its trailing slash.
    pub path: &'a str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct SetMetaKv2Options<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    pub path: &'a str,
    pub value: &'a str,
    /// When given, the write is conditional on the key standing at this
    /// revision. A stale revision is a conflict even when the write would
    /// change nothing.
    pub revision: Option<&'a MetaKv2Revision>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct SetMetaKv2MultipleOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    /// The writes to commit, keyed by absolute path. All-or-nothing.
    pub writes: &'a BTreeMap<String, MetaKv2Write>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct DeleteMetaKv2DirOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    /// The absolute path of a directory. Must carry its trailing slash.
    pub path: &'a str,
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct SyncMetaKv2QuorumOptions<'a> {
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
}
