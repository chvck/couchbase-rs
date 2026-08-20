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

use std::time::Duration;

use crate::memdx::auth_mechanism::AuthMechanism;
use crate::memdx::durability_level::DurabilityLevel;
use crate::memdx::hello_feature::HelloFeature;
use crate::memdx::subdoc::{LookupInOp, MutateInOp, SubdocDocFlag};

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct HelloRequest {
    pub(crate) client_name: Vec<u8>,
    pub(crate) requested_features: Vec<HelloFeature>,
}

impl HelloRequest {
    pub fn new(client_name: Vec<u8>, requested_features: Vec<HelloFeature>) -> Self {
        Self {
            client_name,
            requested_features,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct GetErrorMapRequest {
    pub(crate) version: u16,
}

impl GetErrorMapRequest {
    pub fn new(version: u16) -> Self {
        Self { version }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SelectBucketRequest {
    pub(crate) bucket_name: String,
}

impl SelectBucketRequest {
    pub fn new(bucket_name: String) -> Self {
        Self { bucket_name }
    }
}

#[derive(Clone, Default, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct GetClusterConfigKnownVersion {
    pub rev_epoch: i64,
    pub rev_id: i64,
}

#[derive(Clone, Default, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct GetClusterConfigRequest {
    pub known_version: Option<GetClusterConfigKnownVersion>,
}

impl GetClusterConfigRequest {
    pub fn new() -> Self {
        Self {
            known_version: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct GetCollectionIdRequest<'a> {
    pub(crate) scope_name: &'a str,
    pub(crate) collection_name: &'a str,
}

impl<'a> GetCollectionIdRequest<'a> {
    pub fn new(scope_name: &'a str, collection_name: &'a str) -> Self {
        Self {
            scope_name,
            collection_name,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SASLAuthRequest {
    pub(crate) auth_mechanism: AuthMechanism,
    pub(crate) payload: Vec<u8>,
}

impl SASLAuthRequest {
    pub fn new(auth_mechanism: AuthMechanism, payload: Vec<u8>) -> Self {
        Self {
            auth_mechanism,
            payload,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SASLStepRequest {
    pub(crate) auth_mechanism: AuthMechanism,
    pub(crate) payload: Vec<u8>,
}

impl SASLStepRequest {
    pub fn new(auth_mechanism: AuthMechanism, payload: Vec<u8>) -> Self {
        Self {
            auth_mechanism,
            payload,
        }
    }
}

#[derive(Clone, Default, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SASLListMechsRequest {}

impl SASLListMechsRequest {
    pub fn new() -> Self {
        Self {}
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SetRequest<'a> {
    pub(crate) collection_id: u32,
    pub(crate) key: &'a [u8],
    pub(crate) vbucket_id: u16,
    pub(crate) flags: u32,
    pub(crate) value: &'a [u8],
    pub(crate) datatype: u8,
    pub(crate) expiry: Option<u32>,
    pub(crate) preserve_expiry: Option<bool>,
    pub(crate) cas: Option<u64>,
    pub(crate) on_behalf_of: Option<&'a str>,
    pub(crate) durability_level: Option<DurabilityLevel>,
    pub(crate) durability_level_timeout: Option<Duration>,
}

impl<'a> SetRequest<'a> {
    pub fn new(
        collection_id: u32,
        key: &'a [u8],
        vbucket_id: u16,
        flags: u32,
        value: &'a [u8],
        datatype: u8,
    ) -> Self {
        Self {
            collection_id,
            key,
            vbucket_id,
            flags,
            value,
            datatype,
            expiry: None,
            preserve_expiry: None,
            cas: None,
            on_behalf_of: None,
            durability_level: None,
            durability_level_timeout: None,
        }
    }

    pub fn expiry(mut self, expiry: u32) -> Self {
        self.expiry = Some(expiry);
        self
    }

    pub fn preserve_expiry(mut self, preserve_expiry: bool) -> Self {
        self.preserve_expiry = Some(preserve_expiry);
        self
    }

    pub fn cas(mut self, cas: u64) -> Self {
        self.cas = Some(cas);
        self
    }

    pub fn on_behalf_of(mut self, on_behalf_of: &'a str) -> Self {
        self.on_behalf_of = Some(on_behalf_of);
        self
    }

    pub fn durability_level(mut self, durability_level: DurabilityLevel) -> Self {
        self.durability_level = Some(durability_level);
        self
    }

    pub fn durability_level_timeout(mut self, durability_level_timeout: Duration) -> Self {
        self.durability_level_timeout = Some(durability_level_timeout);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct GetRequest<'a> {
    pub(crate) collection_id: u32,
    pub(crate) key: &'a [u8],
    pub(crate) vbucket_id: u16,
    pub(crate) on_behalf_of: Option<&'a str>,
}

impl<'a> GetRequest<'a> {
    pub fn new(collection_id: u32, key: &'a [u8], vbucket_id: u16) -> Self {
        Self {
            collection_id,
            key,
            vbucket_id,
            on_behalf_of: None,
        }
    }

    pub fn on_behalf_of(mut self, on_behalf_of: &'a str) -> Self {
        self.on_behalf_of = Some(on_behalf_of);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct GetMetaRequest<'a> {
    pub(crate) collection_id: u32,
    pub(crate) key: &'a [u8],
    pub(crate) vbucket_id: u16,
    pub(crate) on_behalf_of: Option<&'a str>,
}

impl<'a> GetMetaRequest<'a> {
    pub fn new(collection_id: u32, key: &'a [u8], vbucket_id: u16) -> Self {
        Self {
            collection_id,
            key,
            vbucket_id,
            on_behalf_of: None,
        }
    }

    pub fn on_behalf_of(mut self, on_behalf_of: &'a str) -> Self {
        self.on_behalf_of = Some(on_behalf_of);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DeleteRequest<'a> {
    pub(crate) collection_id: u32,
    pub(crate) key: &'a [u8],
    pub(crate) vbucket_id: u16,
    pub(crate) cas: Option<u64>,
    pub(crate) on_behalf_of: Option<&'a str>,
    pub(crate) durability_level: Option<DurabilityLevel>,
    pub(crate) durability_level_timeout: Option<Duration>,
}

impl<'a> DeleteRequest<'a> {
    pub fn new(collection_id: u32, key: &'a [u8], vbucket_id: u16) -> Self {
        Self {
            collection_id,
            key,
            vbucket_id,
            cas: None,
            on_behalf_of: None,
            durability_level: None,
            durability_level_timeout: None,
        }
    }

    pub fn cas(mut self, cas: u64) -> Self {
        self.cas = Some(cas);
        self
    }

    pub fn on_behalf_of(mut self, on_behalf_of: &'a str) -> Self {
        self.on_behalf_of = Some(on_behalf_of);
        self
    }

    pub fn durability_level(mut self, durability_level: DurabilityLevel) -> Self {
        self.durability_level = Some(durability_level);
        self
    }

    pub fn durability_level_timeout(mut self, durability_level_timeout: Duration) -> Self {
        self.durability_level_timeout = Some(durability_level_timeout);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct GetAndLockRequest<'a> {
    pub(crate) collection_id: u32,
    pub(crate) key: &'a [u8],
    pub(crate) vbucket_id: u16,
    pub(crate) lock_time: u32,
    pub(crate) on_behalf_of: Option<&'a str>,
}

impl<'a> GetAndLockRequest<'a> {
    pub fn new(collection_id: u32, key: &'a [u8], vbucket_id: u16, lock_time: u32) -> Self {
        Self {
            collection_id,
            key,
            vbucket_id,
            lock_time,
            on_behalf_of: None,
        }
    }

    pub fn on_behalf_of(mut self, on_behalf_of: &'a str) -> Self {
        self.on_behalf_of = Some(on_behalf_of);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct GetAndTouchRequest<'a> {
    pub(crate) collection_id: u32,
    pub(crate) key: &'a [u8],
    pub(crate) vbucket_id: u16,
    pub(crate) expiry: u32,
    pub(crate) on_behalf_of: Option<&'a str>,
}

impl<'a> GetAndTouchRequest<'a> {
    pub fn new(collection_id: u32, key: &'a [u8], vbucket_id: u16, expiry: u32) -> Self {
        Self {
            collection_id,
            key,
            vbucket_id,
            expiry,
            on_behalf_of: None,
        }
    }

    pub fn on_behalf_of(mut self, on_behalf_of: &'a str) -> Self {
        self.on_behalf_of = Some(on_behalf_of);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct UnlockRequest<'a> {
    pub(crate) collection_id: u32,
    pub(crate) key: &'a [u8],
    pub(crate) vbucket_id: u16,
    pub(crate) cas: u64,
    pub(crate) on_behalf_of: Option<&'a str>,
}

impl<'a> UnlockRequest<'a> {
    pub fn new(collection_id: u32, key: &'a [u8], vbucket_id: u16, cas: u64) -> Self {
        Self {
            collection_id,
            key,
            vbucket_id,
            cas,
            on_behalf_of: None,
        }
    }

    pub fn on_behalf_of(mut self, on_behalf_of: &'a str) -> Self {
        self.on_behalf_of = Some(on_behalf_of);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TouchRequest<'a> {
    pub(crate) collection_id: u32,
    pub(crate) key: &'a [u8],
    pub(crate) vbucket_id: u16,
    pub(crate) expiry: u32,
    pub(crate) on_behalf_of: Option<&'a str>,
}

impl<'a> TouchRequest<'a> {
    pub fn new(collection_id: u32, key: &'a [u8], vbucket_id: u16, expiry: u32) -> Self {
        Self {
            collection_id,
            key,
            vbucket_id,
            expiry,
            on_behalf_of: None,
        }
    }

    pub fn on_behalf_of(mut self, on_behalf_of: &'a str) -> Self {
        self.on_behalf_of = Some(on_behalf_of);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct AddRequest<'a> {
    pub(crate) collection_id: u32,
    pub(crate) key: &'a [u8],
    pub(crate) vbucket_id: u16,
    pub(crate) flags: u32,
    pub(crate) value: &'a [u8],
    pub(crate) datatype: u8,
    pub(crate) expiry: Option<u32>,
    pub(crate) on_behalf_of: Option<&'a str>,
    pub(crate) durability_level: Option<DurabilityLevel>,
    pub(crate) durability_level_timeout: Option<Duration>,
}

impl<'a> AddRequest<'a> {
    pub fn new(
        collection_id: u32,
        key: &'a [u8],
        vbucket_id: u16,
        flags: u32,
        value: &'a [u8],
        datatype: u8,
    ) -> Self {
        Self {
            collection_id,
            key,
            vbucket_id,
            flags,
            value,
            datatype,
            expiry: None,
            on_behalf_of: None,
            durability_level: None,
            durability_level_timeout: None,
        }
    }

    pub fn expiry(mut self, expiry: u32) -> Self {
        self.expiry = Some(expiry);
        self
    }

    pub fn on_behalf_of(mut self, on_behalf_of: &'a str) -> Self {
        self.on_behalf_of = Some(on_behalf_of);
        self
    }

    pub fn durability_level(mut self, durability_level: DurabilityLevel) -> Self {
        self.durability_level = Some(durability_level);
        self
    }

    pub fn durability_level_timeout(mut self, durability_level_timeout: Duration) -> Self {
        self.durability_level_timeout = Some(durability_level_timeout);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ReplaceRequest<'a> {
    pub(crate) collection_id: u32,
    pub(crate) key: &'a [u8],
    pub(crate) vbucket_id: u16,
    pub(crate) flags: u32,
    pub(crate) value: &'a [u8],
    pub(crate) datatype: u8,
    pub(crate) expiry: Option<u32>,
    pub(crate) preserve_expiry: Option<bool>,
    pub(crate) cas: Option<u64>,
    pub(crate) on_behalf_of: Option<&'a str>,
    pub(crate) durability_level: Option<DurabilityLevel>,
    pub(crate) durability_level_timeout: Option<Duration>,
}

impl<'a> ReplaceRequest<'a> {
    pub fn new(
        collection_id: u32,
        key: &'a [u8],
        vbucket_id: u16,
        flags: u32,
        value: &'a [u8],
        datatype: u8,
    ) -> Self {
        Self {
            collection_id,
            key,
            vbucket_id,
            flags,
            value,
            datatype,
            expiry: None,
            preserve_expiry: None,
            cas: None,
            on_behalf_of: None,
            durability_level: None,
            durability_level_timeout: None,
        }
    }

    pub fn expiry(mut self, expiry: u32) -> Self {
        self.expiry = Some(expiry);
        self
    }

    pub fn preserve_expiry(mut self, preserve_expiry: bool) -> Self {
        self.preserve_expiry = Some(preserve_expiry);
        self
    }

    pub fn cas(mut self, cas: u64) -> Self {
        self.cas = Some(cas);
        self
    }

    pub fn on_behalf_of(mut self, on_behalf_of: &'a str) -> Self {
        self.on_behalf_of = Some(on_behalf_of);
        self
    }

    pub fn durability_level(mut self, durability_level: DurabilityLevel) -> Self {
        self.durability_level = Some(durability_level);
        self
    }

    pub fn durability_level_timeout(mut self, durability_level_timeout: Duration) -> Self {
        self.durability_level_timeout = Some(durability_level_timeout);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct AppendRequest<'a> {
    pub(crate) collection_id: u32,
    pub(crate) key: &'a [u8],
    pub(crate) vbucket_id: u16,
    pub(crate) value: &'a [u8],
    pub(crate) datatype: u8,
    pub(crate) cas: Option<u64>,
    pub(crate) on_behalf_of: Option<&'a str>,
    pub(crate) durability_level: Option<DurabilityLevel>,
    pub(crate) durability_level_timeout: Option<Duration>,
}

impl<'a> AppendRequest<'a> {
    pub fn new(
        collection_id: u32,
        key: &'a [u8],
        vbucket_id: u16,
        value: &'a [u8],
        datatype: u8,
    ) -> Self {
        Self {
            collection_id,
            key,
            vbucket_id,
            value,
            datatype,
            cas: None,
            on_behalf_of: None,
            durability_level: None,
            durability_level_timeout: None,
        }
    }

    pub fn cas(mut self, cas: u64) -> Self {
        self.cas = Some(cas);
        self
    }

    pub fn on_behalf_of(mut self, on_behalf_of: &'a str) -> Self {
        self.on_behalf_of = Some(on_behalf_of);
        self
    }

    pub fn durability_level(mut self, durability_level: DurabilityLevel) -> Self {
        self.durability_level = Some(durability_level);
        self
    }

    pub fn durability_level_timeout(mut self, durability_level_timeout: Duration) -> Self {
        self.durability_level_timeout = Some(durability_level_timeout);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PrependRequest<'a> {
    pub(crate) collection_id: u32,
    pub(crate) key: &'a [u8],
    pub(crate) vbucket_id: u16,
    pub(crate) value: &'a [u8],
    pub(crate) datatype: u8,
    pub(crate) cas: Option<u64>,
    pub(crate) on_behalf_of: Option<&'a str>,
    pub(crate) durability_level: Option<DurabilityLevel>,
    pub(crate) durability_level_timeout: Option<Duration>,
}

impl<'a> PrependRequest<'a> {
    pub fn new(
        collection_id: u32,
        key: &'a [u8],
        vbucket_id: u16,
        value: &'a [u8],
        datatype: u8,
    ) -> Self {
        Self {
            collection_id,
            key,
            vbucket_id,
            value,
            datatype,
            cas: None,
            on_behalf_of: None,
            durability_level: None,
            durability_level_timeout: None,
        }
    }

    pub fn cas(mut self, cas: u64) -> Self {
        self.cas = Some(cas);
        self
    }

    pub fn on_behalf_of(mut self, on_behalf_of: &'a str) -> Self {
        self.on_behalf_of = Some(on_behalf_of);
        self
    }

    pub fn durability_level(mut self, durability_level: DurabilityLevel) -> Self {
        self.durability_level = Some(durability_level);
        self
    }

    pub fn durability_level_timeout(mut self, durability_level_timeout: Duration) -> Self {
        self.durability_level_timeout = Some(durability_level_timeout);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct IncrementRequest<'a> {
    pub(crate) collection_id: u32,
    pub(crate) key: &'a [u8],
    pub(crate) vbucket_id: u16,
    pub(crate) initial: Option<u64>,
    pub(crate) delta: u64,
    pub(crate) expiry: Option<u32>,
    pub(crate) on_behalf_of: Option<&'a str>,
    pub(crate) durability_level: Option<DurabilityLevel>,
    pub(crate) durability_level_timeout: Option<Duration>,
}

impl<'a> IncrementRequest<'a> {
    pub fn new(collection_id: u32, key: &'a [u8], delta: u64, vbucket_id: u16) -> Self {
        Self {
            collection_id,
            key,
            vbucket_id,
            initial: None,
            delta,
            expiry: None,
            on_behalf_of: None,
            durability_level: None,
            durability_level_timeout: None,
        }
    }

    pub fn initial(mut self, initial: u64) -> Self {
        self.initial = Some(initial);
        self
    }

    pub fn expiry(mut self, expiry: u32) -> Self {
        self.expiry = Some(expiry);
        self
    }

    pub fn on_behalf_of(mut self, on_behalf_of: &'a str) -> Self {
        self.on_behalf_of = Some(on_behalf_of);
        self
    }

    pub fn durability_level(mut self, durability_level: DurabilityLevel) -> Self {
        self.durability_level = Some(durability_level);
        self
    }

    pub fn durability_level_timeout(mut self, durability_level_timeout: Duration) -> Self {
        self.durability_level_timeout = Some(durability_level_timeout);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DecrementRequest<'a> {
    pub(crate) collection_id: u32,
    pub(crate) key: &'a [u8],
    pub(crate) vbucket_id: u16,
    pub(crate) initial: Option<u64>,
    pub(crate) delta: u64,
    pub(crate) expiry: Option<u32>,
    pub(crate) on_behalf_of: Option<&'a str>,
    pub(crate) durability_level: Option<DurabilityLevel>,
    pub(crate) durability_level_timeout: Option<Duration>,
}

impl<'a> DecrementRequest<'a> {
    pub fn new(collection_id: u32, key: &'a [u8], delta: u64, vbucket_id: u16) -> Self {
        Self {
            collection_id,
            key,
            vbucket_id,
            initial: None,
            delta,
            expiry: None,
            on_behalf_of: None,
            durability_level: None,
            durability_level_timeout: None,
        }
    }

    pub fn initial(mut self, initial: u64) -> Self {
        self.initial = Some(initial);
        self
    }

    pub fn expiry(mut self, expiry: u32) -> Self {
        self.expiry = Some(expiry);
        self
    }

    pub fn on_behalf_of(mut self, on_behalf_of: &'a str) -> Self {
        self.on_behalf_of = Some(on_behalf_of);
        self
    }

    pub fn durability_level(mut self, durability_level: DurabilityLevel) -> Self {
        self.durability_level = Some(durability_level);
        self
    }

    pub fn durability_level_timeout(mut self, durability_level_timeout: Duration) -> Self {
        self.durability_level_timeout = Some(durability_level_timeout);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LookupInRequest<'a> {
    pub(crate) collection_id: u32,
    pub(crate) key: &'a [u8],
    pub(crate) vbucket_id: u16,
    pub(crate) flags: SubdocDocFlag,
    pub(crate) ops: &'a [LookupInOp<'a>],
    pub(crate) on_behalf_of: Option<&'a str>,
}

impl<'a> LookupInRequest<'a> {
    pub fn new(
        collection_id: u32,
        key: &'a [u8],
        vbucket_id: u16,
        flags: SubdocDocFlag,
        ops: &'a [LookupInOp<'a>],
    ) -> Self {
        Self {
            collection_id,
            key,
            vbucket_id,
            flags,
            ops,
            on_behalf_of: None,
        }
    }

    pub fn on_behalf_of(mut self, on_behalf_of: &'a str) -> Self {
        self.on_behalf_of = Some(on_behalf_of);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutateInRequest<'a> {
    pub(crate) collection_id: u32,
    pub(crate) key: &'a [u8],
    pub(crate) vbucket_id: u16,
    pub(crate) flags: SubdocDocFlag,
    pub(crate) ops: &'a [MutateInOp<'a>],
    pub(crate) expiry: Option<u32>,
    pub(crate) preserve_expiry: Option<bool>,
    pub(crate) cas: Option<u64>,
    pub(crate) on_behalf_of: Option<&'a str>,
    pub(crate) durability_level: Option<DurabilityLevel>,
    pub(crate) durability_level_timeout: Option<Duration>,
}

impl<'a> MutateInRequest<'a> {
    pub fn new(
        collection_id: u32,
        key: &'a [u8],
        vbucket_id: u16,
        flags: SubdocDocFlag,
        ops: &'a [MutateInOp<'a>],
    ) -> Self {
        Self {
            collection_id,
            key,
            vbucket_id,
            flags,
            ops,
            expiry: None,
            preserve_expiry: None,
            cas: None,
            on_behalf_of: None,
            durability_level: None,
            durability_level_timeout: None,
        }
    }

    pub fn expiry(mut self, expiry: u32) -> Self {
        self.expiry = Some(expiry);
        self
    }

    pub fn preserve_expiry(mut self, preserve_expiry: bool) -> Self {
        self.preserve_expiry = Some(preserve_expiry);
        self
    }

    pub fn cas(mut self, cas: u64) -> Self {
        self.cas = Some(cas);
        self
    }

    pub fn on_behalf_of(mut self, on_behalf_of: &'a str) -> Self {
        self.on_behalf_of = Some(on_behalf_of);
        self
    }

    pub fn durability_level(mut self, durability_level: DurabilityLevel) -> Self {
        self.durability_level = Some(durability_level);
        self
    }

    pub fn durability_level_timeout(mut self, durability_level_timeout: Duration) -> Self {
        self.durability_level_timeout = Some(durability_level_timeout);
        self
    }
}

/// A `STAT` sweep of one node.
///
/// Answers with a **stream** of key/value packets terminated by an empty one, so
/// it holds its connection for as long as the node takes to produce them --
/// which is why it runs on the bulk connection manager.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct StatsRequest<'a> {
    /// The stat group, or empty for the default set.
    pub group_name: &'a str,
    pub on_behalf_of: Option<&'a str>,
}

impl<'a> StatsRequest<'a> {
    pub fn new(group_name: &'a str) -> Self {
        Self {
            group_name,
            on_behalf_of: None,
        }
    }

    pub fn on_behalf_of(mut self, on_behalf_of: &'a str) -> Self {
        self.on_behalf_of = Some(on_behalf_of);
        self
    }
}

/// Which vbucket states to report.
///
/// **`Active` rather than `Alive`, and it matters twice.** A replica reports
/// its checkpoint snapshot *range end*, which can name a sequence number the
/// vbucket has not received — ask an index to wait for one of those and the
/// scan never finishes. Filtering here also means the caller does not have to
/// work out which node is active for which vbucket, which is 1024 routing
/// lookups it would otherwise do per sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum RequestedVbState {
    /// Every state that is not dead.
    Alive = 0,
    Active = 1,
    Replica = 2,
    Pending = 3,
}

/// The binary alternative to a `stats vbucket-seqno` sweep: every active
/// vbucket's high sequence number in one packet, ten bytes each, rather than
/// eight text fields per vbucket a node holds -- replicas included.
#[derive(Debug, Clone)]
pub struct GetAllVbSeqnosRequest<'a> {
    pub state: RequestedVbState,
    /// Report each vbucket's high seqno **for this collection only**.
    ///
    /// `None` is the bucket-wide high seqno. The indexer's own
    /// `SessionConsistency` scopes to the collection being scanned, so a
    /// bucket-wide vector makes one collection's reads wait for another's
    /// writes to be indexed -- safe, since waiting for more is always safe,
    /// but a coupling worth removing once a caller keeps a vector per
    /// collection.
    pub collection_id: Option<u32>,
    pub on_behalf_of: Option<&'a str>,
}

impl GetAllVbSeqnosRequest<'_> {
    /// 4 bytes of state, then optionally 4 of collection id. The server's
    /// validator accepts 0, 4 or 8 and refuses anything else.
    pub(crate) fn extras(&self) -> Vec<u8> {
        let mut extras = Vec::with_capacity(8);
        extras.extend_from_slice(&(self.state as u32).to_be_bytes());
        if let Some(cid) = self.collection_id {
            extras.extend_from_slice(&cid.to_be_bytes());
        }
        extras
    }
}

#[derive(Default, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PingRequest<'a> {
    pub(crate) on_behalf_of: Option<&'a str>,
}

impl<'a> PingRequest<'a> {
    pub fn new() -> Self {
        Self { on_behalf_of: None }
    }

    pub fn on_behalf_of(mut self, on_behalf_of: &'a str) -> Self {
        self.on_behalf_of = Some(on_behalf_of);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_filter_is_the_first_four_bytes() {
        let req = GetAllVbSeqnosRequest {
            state: RequestedVbState::Active,
            collection_id: None,
            on_behalf_of: None,
        };
        assert_eq!(req.extras(), vec![0, 0, 0, 1]);
    }

    #[test]
    fn a_collection_id_appends_four_more() {
        // The validator accepts 0, 4 or 8 extras bytes and refuses anything else.
        let req = GetAllVbSeqnosRequest {
            state: RequestedVbState::Active,
            collection_id: Some(0x0a0b0c0d),
            on_behalf_of: None,
        };
        assert_eq!(req.extras(), vec![0, 0, 0, 1, 0x0a, 0x0b, 0x0c, 0x0d]);
    }
}
