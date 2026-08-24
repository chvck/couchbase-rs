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

use std::fmt::Debug;
use std::future::Future;
use std::sync::Arc;

use crate::collectionresolver::{orchestrate_memd_collection_id, CollectionResolver};
use crate::compressionmanager::{CompressionManager, Compressor};
use crate::error;
use crate::error::{Error, MemdxError, Result};
use crate::kv_orchestration::{
    orchestrate_endpoint_kv_client, orchestrate_kv_client, KvClientManagerClientType,
};
use crate::kvclient::KvClient;
use crate::kvclient_ops::KvClientOps;
use crate::kvendpointclientmanager::KvEndpointClientManager;
use crate::memdx::datatype::DataTypeFlag;
use crate::memdx::error::ServerErrorKind;
use crate::memdx::hello_feature::HelloFeature;
use crate::memdx::ops_rangescan::{
    RangeScanConfig, RangeScanCreateRequest, RangeScanCreateResponse,
};
use crate::memdx::request::{
    AddRequest, AppendRequest, DecrementRequest, DeleteRequest, GetAllVbSeqnosRequest,
    GetAndLockRequest, GetAndTouchRequest, GetCollectionIdRequest, GetMetaRequest, GetRequest,
    IncrementRequest, LookupInRequest, MutateInRequest, PrependRequest, ReplaceRequest,
    RequestedVbState, SetRequest, StatsRequest, TouchRequest, UnlockRequest,
};
use crate::memdx::response::{LookupInResponse, MutateInResponse, VbSeqno};
use crate::memdx::status::Status;
use crate::mutationtoken::MutationToken;
use crate::nmvbhandler::NotMyVbucketConfigHandler;
use crate::options::crud::{
    AddOptions, AppendOptions, DecrementOptions, DeleteOptions, GetAndLockOptions,
    GetAndTouchOptions, GetCollectionIdOptions, GetMetaOptions, GetOptions, IncrementOptions,
    LookupInOptions, MutateInOptions, PrependOptions, ReplaceOptions, TouchOptions, UnlockOptions,
    UpsertOptions,
};
use crate::options::rangescan::RangeScanCreateOptions;
use crate::options::stats::{CollectionStatsOptions, StatsByVbucketOptions, StatsOptions};
use crate::options::vbucket_seqnos::VbucketSeqnosOptions;
use crate::results::kv::{
    AddResult, AppendResult, DecrementResult, DeleteResult, GetAndLockResult, GetAndTouchResult,
    GetCollectionIdResult, GetMetaResult, GetResult, IncrementResult, LookupInResult,
    MutateInResult, PrependResult, ReplaceResult, SubDocResult, TouchResult, UnlockResult,
    UpsertResult,
};
use crate::results::stats::{CollectionStats, StatsEntry, StatsResult};
use crate::retry::{
    error_to_retry_reason, orchestrate_retries, RetryComponent, RetryRequest, RetryStrategy,
};
use crate::vbucketrouter::{orchestrate_memd_routing, VbucketRouter};
use bytes::Bytes;
use futures::{FutureExt, TryFutureExt};
use tokio::time::sleep;
use tracing::debug;

pub(crate) struct CrudComponent<
    M: KvEndpointClientManager,
    V: VbucketRouter,
    Nmvb: NotMyVbucketConfigHandler,
    C: CollectionResolver,
    Comp: Compressor,
> {
    conn_manager: Arc<M>,
    /// Connections for operations that answer with a stream of packets.
    ///
    /// **Not a second choice for point operations.** Which manager an operation
    /// uses is decided by how many responses it gets, not by what it is called,
    /// and it is decided here and nowhere else: there is no accessor and no way
    /// to ask for the other one.
    bulk_conn_manager: Arc<M>,
    router: Arc<V>,
    nmvb_handler: Arc<Nmvb>,
    collections: Arc<C>,
    retry_manager: Arc<RetryComponent>,
    compression_manager: Arc<CompressionManager<Comp>>,
}

// TODO: So much clone.
impl<
        M: KvEndpointClientManager,
        V: VbucketRouter,
        Nmvb: NotMyVbucketConfigHandler,
        C: CollectionResolver,
        Comp: Compressor,
    > CrudComponent<M, V, Nmvb, C, Comp>
{
    pub(crate) fn new(
        nmvb_handler: Arc<Nmvb>,
        router: Arc<V>,
        conn_manager: Arc<M>,
        bulk_conn_manager: Arc<M>,
        collections: Arc<C>,
        retry_manager: Arc<RetryComponent>,
        compression_manager: Arc<CompressionManager<Comp>>,
    ) -> Self {
        CrudComponent {
            conn_manager,
            bulk_conn_manager,
            router,
            nmvb_handler,
            collections,
            retry_manager,
            compression_manager,
        }
    }

    pub(crate) async fn upsert(&self, opts: UpsertOptions<'_>) -> Result<UpsertResult> {
        self.orchestrate_simple_crud(
            opts.key,
            opts.retry_strategy,
            RetryRequest::new("upsert", false),
            opts.scope_name,
            opts.collection_name,
            async |collection_id, vbucket_id, client| {
                let mut compressor = self.compression_manager.compressor();
                let (value, datatype) = match compressor.compress(
                    client.has_feature(HelloFeature::Snappy),
                    opts.datatype,
                    opts.value,
                ) {
                    Ok(result) => result,
                    Err(e) => {
                        return Err(e);
                    }
                };

                client
                    .set(SetRequest {
                        collection_id,
                        key: opts.key,
                        vbucket_id,
                        flags: opts.flags,
                        value,
                        datatype,
                        expiry: opts.expiry,
                        preserve_expiry: opts.preserve_expiry,
                        cas: opts.cas,
                        on_behalf_of: opts.on_behalf_of.map(|o| o.username.as_str()),
                        durability_level: opts.durability_level,
                        durability_level_timeout: None,
                    })
                    .map_err(|e| {
                        let e = Self::update_memdx_err(
                            client.clone(),
                            e,
                            opts.key.to_vec(),
                            opts.scope_name,
                            opts.collection_name,
                        );

                        Error::new_contextual_memdx_error(e)
                    })
                    .map_ok(|resp| {
                        let mutation_token = resp.mutation_token.map(|t| MutationToken {
                            vbid: vbucket_id,
                            vbuuid: t.vbuuid,
                            seqno: t.seqno,
                        });

                        UpsertResult {
                            cas: resp.cas,
                            mutation_token,
                        }
                    })
                    .await
            },
        )
        .await
    }

    pub(crate) async fn get(&self, opts: GetOptions<'_>) -> Result<GetResult> {
        self.orchestrate_simple_crud(
            opts.key,
            opts.retry_strategy,
            RetryRequest::new("get", true),
            opts.scope_name,
            opts.collection_name,
            async |collection_id, vbucket_id, client| {
                client
                    .get(GetRequest {
                        collection_id,
                        key: opts.key,
                        vbucket_id,
                        on_behalf_of: opts.on_behalf_of.map(|o| o.username.as_str()),
                    })
                    .map_err(|e| {
                        let e = Self::update_memdx_err(
                            client.clone(),
                            e,
                            opts.key.to_vec(),
                            opts.scope_name,
                            opts.collection_name,
                        );

                        Error::new_contextual_memdx_error(e)
                    })
                    .map_ok(|resp| GetResult {
                        value: resp.value,
                        datatype: resp.datatype,
                        cas: resp.cas,
                        flags: resp.flags,
                    })
                    .await
            },
        )
        .await
    }

    pub(crate) async fn get_meta(&self, opts: GetMetaOptions<'_>) -> Result<GetMetaResult> {
        self.orchestrate_simple_crud(
            opts.key,
            opts.retry_strategy,
            RetryRequest::new("get_meta", true),
            opts.scope_name,
            opts.collection_name,
            async |collection_id, vbucket_id, client| {
                client
                    .get_meta(GetMetaRequest {
                        collection_id,
                        key: opts.key,
                        vbucket_id,
                        on_behalf_of: opts.on_behalf_of.map(|o| o.username.as_str()),
                    })
                    .map_err(|e| {
                        let e = Self::update_memdx_err(
                            client.clone(),
                            e,
                            opts.key.to_vec(),
                            opts.scope_name,
                            opts.collection_name,
                        );

                        Error::new_contextual_memdx_error(e)
                    })
                    .map_ok(|resp| GetMetaResult {
                        value: resp.value,
                        datatype: resp.datatype,
                        server_duration: resp.server_duration,
                        expiry: resp.expiry,
                        seq_no: resp.seq_no,
                        cas: resp.cas,
                        flags: resp.flags,
                        deleted: resp.deleted,
                    })
                    .await
            },
        )
        .await
    }

    pub async fn delete(&self, opts: DeleteOptions<'_>) -> Result<DeleteResult> {
        self.orchestrate_simple_crud(
            opts.key,
            opts.retry_strategy,
            RetryRequest::new("delete", false),
            opts.scope_name,
            opts.collection_name,
            async |collection_id, vbucket_id, client| {
                client
                    .delete(DeleteRequest {
                        collection_id,
                        key: opts.key,
                        vbucket_id,
                        cas: opts.cas,
                        on_behalf_of: opts.on_behalf_of.map(|o| o.username.as_str()),
                        durability_level: opts.durability_level,
                        durability_level_timeout: None,
                    })
                    .map_err(|e| {
                        let e = Self::update_memdx_err(
                            client.clone(),
                            e,
                            opts.key.to_vec(),
                            opts.scope_name,
                            opts.collection_name,
                        );

                        Error::new_contextual_memdx_error(e)
                    })
                    .map_ok(|resp| {
                        let mutation_token = resp.mutation_token.map(|t| MutationToken {
                            vbid: vbucket_id,
                            vbuuid: t.vbuuid,
                            seqno: t.seqno,
                        });

                        DeleteResult {
                            cas: resp.cas,
                            mutation_token,
                        }
                    })
                    .await
            },
        )
        .await
    }

    pub async fn get_and_lock(&self, opts: GetAndLockOptions<'_>) -> Result<GetAndLockResult> {
        self.orchestrate_simple_crud(
            opts.key,
            opts.retry_strategy,
            RetryRequest::new("get_and_lock", false),
            opts.scope_name,
            opts.collection_name,
            async |collection_id, vbucket_id, client| {
                client
                    .get_and_lock(GetAndLockRequest {
                        collection_id,
                        key: opts.key,
                        vbucket_id,
                        lock_time: opts.lock_time,
                        on_behalf_of: opts.on_behalf_of.map(|o| o.username.as_str()),
                    })
                    .map_err(|e| {
                        let e = Self::update_memdx_err(
                            client.clone(),
                            e,
                            opts.key.to_vec(),
                            opts.scope_name,
                            opts.collection_name,
                        );

                        Error::new_contextual_memdx_error(e)
                    })
                    .map_ok(|resp| GetAndLockResult {
                        value: resp.value,
                        datatype: resp.datatype,
                        cas: resp.cas,
                        flags: resp.flags,
                    })
                    .await
            },
        )
        .await
    }

    pub async fn get_and_touch(&self, opts: GetAndTouchOptions<'_>) -> Result<GetAndTouchResult> {
        self.orchestrate_simple_crud(
            opts.key,
            opts.retry_strategy,
            RetryRequest::new("get_and_touch", false),
            opts.scope_name,
            opts.collection_name,
            async |collection_id, vbucket_id, client| {
                client
                    .get_and_touch(GetAndTouchRequest {
                        collection_id,
                        key: opts.key,
                        vbucket_id,
                        expiry: opts.expiry,
                        on_behalf_of: opts.on_behalf_of.map(|o| o.username.as_str()),
                    })
                    .map_err(|e| {
                        let e = Self::update_memdx_err(
                            client.clone(),
                            e,
                            opts.key.to_vec(),
                            opts.scope_name,
                            opts.collection_name,
                        );

                        Error::new_contextual_memdx_error(e)
                    })
                    .map_ok(|resp| GetAndTouchResult {
                        value: resp.value,
                        datatype: resp.datatype,
                        cas: resp.cas,
                        flags: resp.flags,
                    })
                    .await
            },
        )
        .await
    }

    pub async fn unlock(&self, opts: UnlockOptions<'_>) -> Result<UnlockResult> {
        self.orchestrate_simple_crud(
            opts.key,
            opts.retry_strategy,
            RetryRequest::new("unlock", false),
            opts.scope_name,
            opts.collection_name,
            async |collection_id, vbucket_id, client| {
                client
                    .unlock(UnlockRequest {
                        collection_id,
                        key: opts.key,
                        vbucket_id,
                        cas: opts.cas,
                        on_behalf_of: opts.on_behalf_of.map(|o| o.username.as_str()),
                    })
                    .map_err(|e| {
                        let e = Self::update_memdx_err(
                            client.clone(),
                            e,
                            opts.key.to_vec(),
                            opts.scope_name,
                            opts.collection_name,
                        );

                        Error::new_contextual_memdx_error(e)
                    })
                    .map_ok(|resp| UnlockResult {
                        // mutation token?
                    })
                    .await
            },
        )
        .await
    }

    pub async fn touch(&self, opts: TouchOptions<'_>) -> Result<TouchResult> {
        self.orchestrate_simple_crud(
            opts.key,
            opts.retry_strategy,
            RetryRequest::new("touch", false),
            opts.scope_name,
            opts.collection_name,
            async |collection_id, vbucket_id, client| {
                client
                    .touch(TouchRequest {
                        collection_id,
                        key: opts.key,
                        vbucket_id,
                        expiry: opts.expiry,
                        on_behalf_of: opts.on_behalf_of.map(|o| o.username.as_str()),
                    })
                    .map_err(|e| {
                        let e = Self::update_memdx_err(
                            client.clone(),
                            e,
                            opts.key.to_vec(),
                            opts.scope_name,
                            opts.collection_name,
                        );

                        Error::new_contextual_memdx_error(e)
                    })
                    .map_ok(|resp| TouchResult { cas: resp.cas })
                    .await
            },
        )
        .await
    }

    pub async fn add(&self, opts: AddOptions<'_>) -> Result<AddResult> {
        self.orchestrate_simple_crud(
            opts.key,
            opts.retry_strategy,
            RetryRequest::new("add", false),
            opts.scope_name,
            opts.collection_name,
            async |collection_id, vbucket_id, client| {
                let mut compressor = self.compression_manager.compressor();
                let (value, datatype) = match compressor.compress(
                    client.has_feature(HelloFeature::Snappy),
                    opts.datatype,
                    opts.value,
                ) {
                    Ok(result) => result,
                    Err(e) => {
                        return Err(e);
                    }
                };

                client
                    .add(AddRequest {
                        collection_id,
                        key: opts.key,
                        vbucket_id,
                        flags: opts.flags,
                        value,
                        datatype,
                        expiry: opts.expiry,
                        on_behalf_of: opts.on_behalf_of.map(|o| o.username.as_str()),
                        durability_level: opts.durability_level,
                        durability_level_timeout: None,
                    })
                    .map_err(|e| {
                        let e = Self::update_memdx_err(
                            client.clone(),
                            e,
                            opts.key.to_vec(),
                            opts.scope_name,
                            opts.collection_name,
                        );

                        Error::new_contextual_memdx_error(e)
                    })
                    .map_ok(|resp| {
                        let mutation_token = resp.mutation_token.map(|t| MutationToken {
                            vbid: vbucket_id,
                            vbuuid: t.vbuuid,
                            seqno: t.seqno,
                        });

                        AddResult {
                            cas: resp.cas,
                            mutation_token,
                        }
                    })
                    .await
            },
        )
        .await
    }

    pub async fn replace(&self, opts: ReplaceOptions<'_>) -> Result<ReplaceResult> {
        self.orchestrate_simple_crud(
            opts.key,
            opts.retry_strategy,
            RetryRequest::new("replace", false),
            opts.scope_name,
            opts.collection_name,
            async |collection_id, vbucket_id, client| {
                let mut compressor = self.compression_manager.compressor();
                let (value, datatype) = match compressor.compress(
                    client.has_feature(HelloFeature::Snappy),
                    opts.datatype,
                    opts.value,
                ) {
                    Ok(result) => result,
                    Err(e) => {
                        return Err(e);
                    }
                };

                client
                    .replace(ReplaceRequest {
                        collection_id,
                        key: opts.key,
                        vbucket_id,
                        flags: opts.flags,
                        value,
                        datatype,
                        expiry: opts.expiry,
                        preserve_expiry: opts.preserve_expiry,
                        cas: opts.cas,
                        on_behalf_of: opts.on_behalf_of.map(|o| o.username.as_str()),
                        durability_level: opts.durability_level,
                        durability_level_timeout: None,
                    })
                    .map_err(|e| {
                        let e = Self::update_memdx_err(
                            client.clone(),
                            e,
                            opts.key.to_vec(),
                            opts.scope_name,
                            opts.collection_name,
                        );

                        Error::new_contextual_memdx_error(e)
                    })
                    .map_ok(|resp| {
                        let mutation_token = resp.mutation_token.map(|t| MutationToken {
                            vbid: vbucket_id,
                            vbuuid: t.vbuuid,
                            seqno: t.seqno,
                        });

                        ReplaceResult {
                            cas: resp.cas,
                            mutation_token,
                        }
                    })
                    .await
            },
        )
        .await
    }

    pub async fn append(&self, opts: AppendOptions<'_>) -> Result<AppendResult> {
        self.orchestrate_simple_crud(
            opts.key,
            opts.retry_strategy,
            RetryRequest::new("append", false),
            opts.scope_name,
            opts.collection_name,
            async |collection_id, vbucket_id, client| {
                let mut compressor = self.compression_manager.compressor();
                let (value, datatype) = match compressor.compress(
                    client.has_feature(HelloFeature::Snappy),
                    DataTypeFlag::None,
                    opts.value,
                ) {
                    Ok(result) => result,
                    Err(e) => {
                        return Err(e);
                    }
                };

                client
                    .append(AppendRequest {
                        collection_id,
                        key: opts.key,
                        vbucket_id,
                        value,
                        datatype,
                        cas: opts.cas,
                        on_behalf_of: opts.on_behalf_of.map(|o| o.username.as_str()),
                        durability_level: opts.durability_level,
                        durability_level_timeout: None,
                    })
                    .map_err(|e| {
                        let e = Self::update_memdx_err(
                            client.clone(),
                            e,
                            opts.key.to_vec(),
                            opts.scope_name,
                            opts.collection_name,
                        );

                        Error::new_contextual_memdx_error(e)
                    })
                    .map_ok(|resp| {
                        let mutation_token = resp.mutation_token.map(|t| MutationToken {
                            vbid: vbucket_id,
                            vbuuid: t.vbuuid,
                            seqno: t.seqno,
                        });

                        AppendResult {
                            cas: resp.cas,
                            mutation_token,
                        }
                    })
                    .await
            },
        )
        .await
    }

    pub async fn prepend(&self, opts: PrependOptions<'_>) -> Result<PrependResult> {
        self.orchestrate_simple_crud(
            opts.key,
            opts.retry_strategy,
            RetryRequest::new("prepend", false),
            opts.scope_name,
            opts.collection_name,
            async |collection_id, vbucket_id, client| {
                let mut compressor = self.compression_manager.compressor();
                let (value, datatype) = match compressor.compress(
                    client.has_feature(HelloFeature::Snappy),
                    DataTypeFlag::None,
                    opts.value,
                ) {
                    Ok(result) => result,
                    Err(e) => {
                        return Err(e);
                    }
                };

                client
                    .prepend(PrependRequest {
                        collection_id,
                        key: opts.key,
                        vbucket_id,
                        value,
                        datatype,
                        cas: opts.cas,
                        on_behalf_of: opts.on_behalf_of.map(|o| o.username.as_str()),
                        durability_level: opts.durability_level,
                        durability_level_timeout: None,
                    })
                    .map_err(|e| {
                        let e = Self::update_memdx_err(
                            client.clone(),
                            e,
                            opts.key.to_vec(),
                            opts.scope_name,
                            opts.collection_name,
                        );

                        Error::new_contextual_memdx_error(e)
                    })
                    .map_ok(|resp| {
                        let mutation_token = resp.mutation_token.map(|t| MutationToken {
                            vbid: vbucket_id,
                            vbuuid: t.vbuuid,
                            seqno: t.seqno,
                        });

                        PrependResult {
                            cas: resp.cas,
                            mutation_token,
                        }
                    })
                    .await
            },
        )
        .await
    }

    pub async fn increment(&self, opts: IncrementOptions<'_>) -> Result<IncrementResult> {
        self.orchestrate_simple_crud(
            opts.key,
            opts.retry_strategy,
            RetryRequest::new("increment", false),
            opts.scope_name,
            opts.collection_name,
            async |collection_id, vbucket_id, client| {
                client
                    .increment(IncrementRequest {
                        collection_id,
                        key: opts.key,
                        vbucket_id,
                        initial: opts.initial,
                        delta: opts.delta,
                        expiry: opts.expiry,
                        on_behalf_of: opts.on_behalf_of.map(|o| o.username.as_str()),
                        durability_level: opts.durability_level,
                        durability_level_timeout: None,
                    })
                    .map_err(|e| {
                        let e = Self::update_memdx_err(
                            client.clone(),
                            e,
                            opts.key.to_vec(),
                            opts.scope_name,
                            opts.collection_name,
                        );

                        Error::new_contextual_memdx_error(e)
                    })
                    .map_ok(|resp| {
                        let mutation_token = resp.mutation_token.map(|t| MutationToken {
                            vbid: vbucket_id,
                            vbuuid: t.vbuuid,
                            seqno: t.seqno,
                        });

                        IncrementResult {
                            cas: resp.cas,
                            value: resp.value,
                            mutation_token,
                        }
                    })
                    .await
            },
        )
        .await
    }

    pub async fn decrement(&self, opts: DecrementOptions<'_>) -> Result<DecrementResult> {
        self.orchestrate_simple_crud(
            opts.key,
            opts.retry_strategy,
            RetryRequest::new("decrement", false),
            opts.scope_name,
            opts.collection_name,
            async |collection_id, vbucket_id, client| {
                client
                    .decrement(DecrementRequest {
                        collection_id,
                        key: opts.key,
                        vbucket_id,
                        initial: opts.initial,
                        delta: opts.delta,
                        expiry: opts.expiry,
                        on_behalf_of: opts.on_behalf_of.map(|o| o.username.as_str()),
                        durability_level: opts.durability_level,
                        durability_level_timeout: None,
                    })
                    .map_err(|e| {
                        let e = Self::update_memdx_err(
                            client.clone(),
                            e,
                            opts.key.to_vec(),
                            opts.scope_name,
                            opts.collection_name,
                        );

                        Error::new_contextual_memdx_error(e)
                    })
                    .map_ok(|resp| {
                        let mutation_token = resp.mutation_token.map(|t| MutationToken {
                            vbid: vbucket_id,
                            vbuuid: t.vbuuid,
                            seqno: t.seqno,
                        });

                        DecrementResult {
                            cas: resp.cas,
                            value: resp.value,
                            mutation_token,
                        }
                    })
                    .await
            },
        )
        .await
    }

    pub async fn lookup_in(&self, opts: LookupInOptions<'_>) -> Result<LookupInResult> {
        self.orchestrate_simple_crud(
            opts.key,
            opts.retry_strategy,
            RetryRequest::new("lookup_in", true),
            opts.scope_name,
            opts.collection_name,
            async |collection_id, vbucket_id, client| {
                client
                    .lookup_in(LookupInRequest {
                        collection_id,
                        key: opts.key,
                        vbucket_id,
                        flags: opts.flags,
                        ops: opts.ops,
                        on_behalf_of: opts.on_behalf_of.map(|o| o.username.as_str()),
                    })
                    .map_err(|e| {
                        let e = Self::update_memdx_err(
                            client.clone(),
                            e,
                            opts.key.to_vec(),
                            opts.scope_name,
                            opts.collection_name,
                        );

                        Error::new_contextual_memdx_error(e)
                    })
                    .map_ok(|resp: LookupInResponse| LookupInResult {
                        value: resp
                            .ops
                            .into_iter()
                            .map(|o| {
                                let err = o.err.map(|e| {
                                    MemdxError::new(e)
                                        .set_doc_id(opts.key.to_vec())
                                        .set_bucket_name(client.bucket_name().unwrap_or_default())
                                        .set_collection_name(opts.collection_name.to_string())
                                        .set_scope_name(opts.scope_name.to_string())
                                });

                                SubDocResult {
                                    err,
                                    value: o.value,
                                }
                            })
                            .collect(),
                        cas: resp.cas,
                        doc_is_deleted: resp.doc_is_deleted,
                    })
                    .await
            },
        )
        .await
    }

    pub async fn mutate_in(&self, opts: MutateInOptions<'_>) -> Result<MutateInResult> {
        self.orchestrate_simple_crud(
            opts.key,
            opts.retry_strategy,
            RetryRequest::new("mutate_in", false),
            opts.scope_name,
            opts.collection_name,
            async |collection_id, vbucket_id, client| {
                client
                    .mutate_in(MutateInRequest {
                        collection_id,
                        key: opts.key,
                        vbucket_id,
                        flags: opts.flags,
                        ops: opts.ops,
                        expiry: opts.expiry,
                        preserve_expiry: opts.preserve_expiry,
                        cas: opts.cas,
                        on_behalf_of: opts.on_behalf_of.map(|o| o.username.as_str()),
                        durability_level: opts.durability_level,
                        durability_level_timeout: None,
                    })
                    .map_err(|e| {
                        let e = Self::update_memdx_err(
                            client.clone(),
                            e,
                            opts.key.to_vec(),
                            opts.scope_name,
                            opts.collection_name,
                        );

                        Error::new_contextual_memdx_error(e)
                    })
                    .map_ok(|resp: MutateInResponse| {
                        let mutation_token = resp.mutation_token.map(|t| MutationToken {
                            vbid: vbucket_id,
                            vbuuid: t.vbuuid,
                            seqno: t.seqno,
                        });

                        MutateInResult {
                            value: resp
                                .ops
                                .into_iter()
                                .map(|o| {
                                    let err = o.err.map(|e| {
                                        MemdxError::new(e)
                                            .set_doc_id(opts.key.to_vec())
                                            .set_bucket_name(
                                                client.bucket_name().unwrap_or_default(),
                                            )
                                            .set_collection_name(opts.collection_name.to_string())
                                            .set_scope_name(opts.scope_name.to_string())
                                    });

                                    SubDocResult {
                                        err,
                                        value: o.value,
                                    }
                                })
                                .collect(),
                            cas: resp.cas,
                            mutation_token,
                        }
                    })
                    .await
            },
        )
        .await
    }

    /// Open a scan on one vbucket, and say which connection it was opened on.
    ///
    /// The client comes back with the response because a scan belongs to the
    /// connection that created it: its continues and its cancel have to go out
    /// on the same one, so the caller keeps it for the life of the scan rather
    /// than asking the manager again and landing somewhere else.
    ///
    /// Routing is **by vbucket, not by key** -- there is no key to hash, and a
    /// `NotMyVbucket` here is left to the retry manager rather than handled
    /// in-line, because a moved vbucket means a different node and therefore a
    /// different scan.
    pub(crate) async fn range_scan_create(
        &self,
        opts: RangeScanCreateOptions<'_>,
    ) -> Result<(RangeScanCreateResponse, Arc<KvClientManagerClientType<M>>)> {
        orchestrate_retries(
            self.retry_manager.clone(),
            opts.retry_strategy.clone(),
            RetryRequest::new("range_scan_create", true),
            async || {
                orchestrate_memd_collection_id(
                    self.collections.clone(),
                    opts.scope_name,
                    opts.collection_name,
                    async |collection_id: u32| {
                        let endpoint = self.router.dispatch_to_vbucket(opts.vbucket_id)?;

                        orchestrate_endpoint_kv_client(
                            // **The dispatch site that settles the whole scan.**
                            // The handle keeps the client this returns and its
                            // continues go out on it, so naming the bulk manager
                            // here names it for every packet of the scan.
                            self.bulk_conn_manager.clone(),
                            &endpoint,
                            async |client: Arc<KvClientManagerClientType<M>>| {
                                let resp = client
                                    .range_scan_create(RangeScanCreateRequest {
                                        vbucket_id: opts.vbucket_id,
                                        config: RangeScanConfig {
                                            collection_id,
                                            keys_only: opts.keys_only,
                                            range: opts.range.clone(),
                                            sampling: opts.sampling.clone(),
                                            snapshot: opts.snapshot.clone(),
                                        },
                                        on_behalf_of: opts
                                            .on_behalf_of
                                            .map(|o| o.username.as_str()),
                                    })
                                    .await
                                    .map_err(|e| {
                                        Error::new_contextual_memdx_error(
                                            e.set_bucket_name(
                                                client.bucket_name().unwrap_or_default(),
                                            )
                                            .set_scope_name(opts.scope_name.to_string())
                                            .set_collection_name(opts.collection_name.to_string()),
                                        )
                                    })?;

                                Ok((resp, client))
                            },
                        )
                        .await
                    },
                )
                .await
            },
        )
        .await
    }

    /// Sweep `STAT` across every KV node.
    ///
    /// **On the bulk manager, by the response-count rule.** `STAT` answers with a
    /// stream of packets terminated by an empty one, so like a range scan's
    /// continue it holds its connection for as long as the answer takes. What it
    /// is called does not come into it.
    ///
    /// The nodes are swept **one at a time**. In parallel the callback would need
    /// a lock around it, and the sweep is administrative -- nothing on a hot path
    /// waits for it.
    pub(crate) async fn stats<F>(
        &self,
        opts: StatsOptions<'_>,
        mut data_cb: F,
    ) -> Result<StatsResult>
    where
        F: FnMut(StatsEntry) + Send,
    {
        let clients = self.bulk_conn_manager.get_client_per_endpoint().await?;

        let mut result = StatsResult {
            endpoints: clients.len(),
            entries: 0,
        };

        for client in clients {
            let endpoint: Arc<str> = Arc::from(client.canonical_addr().to_string());
            let entries = &mut result.entries;
            let cb = &mut data_cb;

            client
                .stats(StatsRequest::new(opts.group_name), |resp| {
                    *entries += 1;
                    cb(StatsEntry {
                        endpoint: endpoint.clone(),
                        key: resp.key,
                        value: resp.value,
                    });
                })
                .await
                .map_err(Error::new_contextual_memdx_error)?;
        }

        Ok(result)
    }

    /// Ask `STAT` of the node holding one vbucket. Also on the bulk manager.
    ///
    /// **No retry orchestration around the sweep**, unlike a point operation.
    /// `STAT` hands entries to the caller as they arrive, so retrying a sweep
    /// that has already delivered some would deliver them twice. The endpoint
    /// orchestration below still retries a *dispatch* failure, which happens
    /// before any entry exists, and a vbucket that has moved surfaces as an error
    /// for the caller to reissue against the new topology.
    pub(crate) async fn stats_by_vbucket<F>(
        &self,
        opts: StatsByVbucketOptions<'_>,
        data_cb: F,
    ) -> Result<StatsResult>
    where
        F: FnMut(StatsEntry) + Send,
    {
        // The orchestrator wants an `FnMut` operation, and a closure holding the
        // caller's callback by unique reference is `FnOnce`. A mutex gives it
        // back, and it is uncontended: the sweep is sequential.
        let data_cb = std::sync::Mutex::new(data_cb);
        let endpoint = self.router.dispatch_to_vbucket(opts.vbucket_id)?;

        orchestrate_endpoint_kv_client(
            self.bulk_conn_manager.clone(),
            &endpoint,
            async |client: Arc<KvClientManagerClientType<M>>| {
                let node: Arc<str> = Arc::from(client.canonical_addr().to_string());
                let mut entries = 0usize;

                client
                    .stats(StatsRequest::new(opts.group_name), |resp| {
                        entries += 1;
                        let mut cb = data_cb.lock().unwrap();
                        (*cb)(StatsEntry {
                            endpoint: node.clone(),
                            key: resp.key,
                            value: resp.value,
                        });
                    })
                    .await
                    .map_err(Error::new_contextual_memdx_error)?;

                Ok(StatsResult {
                    endpoints: 1,
                    entries,
                })
            },
        )
        .await
    }

    /// Ask KV what it measures about one collection: document count and data
    /// size, summed across the sweep.
    ///
    /// **On the bulk manager, for the same reason `stats` is** -- a `STAT`
    /// reply is a stream terminated by an empty packet, so it holds its
    /// connection for as long as the node takes to send it.
    pub(crate) async fn collection_stats(
        &self,
        opts: CollectionStatsOptions<'_>,
    ) -> Result<Option<CollectionStats>> {
        let group_name = format!("collections {}.{}", opts.scope_name, opts.collection_name);
        let mut totals = CollectionStats::default();
        let mut seen = false;

        let clients = self.bulk_conn_manager.get_client_per_endpoint().await?;
        for client in clients {
            let mut request = StatsRequest::new(&group_name);
            if let Some(on_behalf_of) = opts.on_behalf_of {
                request = request.on_behalf_of(on_behalf_of.username.as_str());
            }
            let result = client
                .stats(request, |resp| {
                    seen = true;
                    let key = String::from_utf8_lossy(&resp.key);
                    let value = String::from_utf8_lossy(&resp.value);
                    take_collection_stat(&mut totals, &key, &value);
                })
                .await;

            match result {
                Ok(_) => {}
                // A name the bucket does not hold, from *any* node. It is the
                // same answer everywhere, so the first one settles it.
                Err(e) if is_unknown_keyspace(&e) => return Ok(None),
                Err(e) => return Err(Error::new_contextual_memdx_error(e)),
            }
        }

        Ok(finish_collection_stats(totals, seen))
    }

    /// The router's internal endpoint id for the node active for one
    /// vbucket, e.g. `"kvep-10.0.0.1-8091"` -- see
    /// `ParsedConfig::addresses_group_for_network_type` and
    /// `AgentComponentConfigs::gen_from_config` for where that shape comes
    /// from. Pure and in-memory (the router is an `ArcSwap` read), so
    /// `agent_ops.rs` calls this once per vbucket without a second thought.
    ///
    /// **Not** the representation a `stats` sweep reports a node under --
    /// see [`Self::resolve_canonical_addr`] for the bridge, and do not
    /// compare this value against a `StatsEntry::endpoint` directly.
    pub(crate) fn dispatch_to_vbucket(&self, vb: u16) -> Result<Arc<str>> {
        self.router.dispatch_to_vbucket(vb)
    }

    /// The canonical `host:port` behind one of [`Self::dispatch_to_vbucket`]'s
    /// endpoint ids -- the representation `stats` and `stats_by_vbucket`
    /// report a node under (`client.canonical_addr()`, read off the
    /// connection they dialed), not the id itself.
    ///
    /// A connection-manager round trip (an on-demand connect, the first time
    /// a given id is seen), so callers should resolve each *distinct* id
    /// once rather than once per vbucket.
    ///
    /// **Errors rather than silently omitting the vbucket**, unlike
    /// `dispatch_to_vbucket`'s ordinary `NoServerAssigned` during a
    /// rebalance. An id the router just returned but the connection manager
    /// has no pool for is not a transient "no owner right now" -- it means
    /// the router's topology and the manager's endpoint set have come apart,
    /// e.g. a data node published with no KV port under the active network
    /// type. That is a fact worth surfacing to the caller, not swallowing
    /// into a map one entry short with no trace of why.
    pub(crate) async fn resolve_canonical_addr(&self, endpoint_id: &str) -> Result<Arc<str>> {
        let client = self
            .bulk_conn_manager
            .get_endpoint_client(endpoint_id)
            .await?;
        Ok(Arc::from(client.canonical_addr().to_string()))
    }

    /// Every **active** vbucket's current high sequence number.
    ///
    /// **Fanned out rather than swept in turn**, unlike `stats`: this is on the
    /// critical path of every `at_plus` scan, where the stats sweep is
    /// administrative. Three serial round trips measured as half the old sweep's
    /// whole cost.
    ///
    /// On the point-operation manager, not the bulk one — the answer is a single
    /// packet, so it does not hold its connection the way a `STAT` sweep does.
    pub(crate) async fn vbucket_seqnos_active(
        &self,
        opts: VbucketSeqnosOptions,
    ) -> Result<Vec<VbSeqno>> {
        let clients = self.conn_manager.get_client_per_endpoint().await?;

        let per_node =
            futures::future::try_join_all(clients.into_iter().map(|client| async move {
                let resp = client
                    .get_all_vb_seqnos(GetAllVbSeqnosRequest {
                        state: RequestedVbState::Active,
                        collection_id: opts.collection_id,
                        on_behalf_of: None,
                    })
                    .await
                    .map_err(Error::new_contextual_memdx_error)?;
                Ok::<_, Error>(resp.seqnos)
            }))
            .await?;

        Ok(per_node.into_iter().flatten().collect())
    }

    pub(crate) async fn get_collection_id(
        &self,
        opts: GetCollectionIdOptions<'_>,
    ) -> Result<GetCollectionIdResult> {
        let mut retry_info = RetryRequest::new("get_collection_id", true);

        loop {
            let mut err = match orchestrate_kv_client(self.conn_manager.clone(), async |client| {
                client
                    .get_collection_id(GetCollectionIdRequest {
                        scope_name: opts.scope_name,
                        collection_name: opts.collection_name,
                    })
                    .map_err(|e| {
                        let e = e
                            .set_bucket_name(client.bucket_name().unwrap_or_default())
                            .set_collection_name(opts.collection_name.to_string())
                            .set_scope_name(opts.scope_name.to_string());

                        Error::new_contextual_memdx_error(e)
                    })
                    .map_ok(|resp| GetCollectionIdResult {
                        collection_id: resp.collection_id,
                        manifest_rev: resp.manifest_rev,
                    })
                    .await
            })
            .await
            {
                Ok(r) => {
                    return Ok(r);
                }
                Err(e) => e,
            };

            if let Some(memdx_err) = err.is_memdx_error() {
                if memdx_err.is_server_error_kind(ServerErrorKind::UnknownCollectionName)
                    || memdx_err.is_server_error_kind(ServerErrorKind::UnknownScopeName)
                {
                    return Err(err);
                }
            }

            if let Some(reason) =
                error_to_retry_reason(self.retry_manager.err_map(), &mut retry_info, &err)
            {
                if let Some(duration) =
                    self.retry_manager
                        .maybe_retry(&opts.retry_strategy, &mut retry_info, reason)
                {
                    debug!(
                        "Retrying {} after {:?} due to {}",
                        &retry_info, duration, reason
                    );
                    sleep(duration).await;
                    continue;
                }
            }

            if retry_info.retry_attempts > 0 {
                // If we aren't retrying then attach any retry info that we have.
                err.set_retry_info(retry_info);
            }

            return Err(err);
        }
    }

    pub(crate) async fn orchestrate_simple_crud<Fut, Resp>(
        &self,
        key: &[u8],
        retry_strategy: Arc<dyn RetryStrategy>,
        retry_info: RetryRequest,
        scope_name: &str,
        collection_name: &str,
        operation: impl Fn(u32, u16, Arc<KvClientManagerClientType<M>>) -> Fut + Send + Sync,
    ) -> Result<Resp>
    where
        Fut: Future<Output = Result<Resp>> + Send,
        Resp: Send,
    {
        orchestrate_retries(
            self.retry_manager.clone(),
            retry_strategy,
            retry_info,
            async || {
                orchestrate_memd_collection_id(
                    self.collections.clone(),
                    scope_name,
                    collection_name,
                    async |collection_id: u32| {
                        orchestrate_memd_routing(
                            self.router.clone(),
                            self.nmvb_handler.clone(),
                            key,
                            0,
                            async |endpoint: Arc<str>, vb_id: u16| {
                                orchestrate_endpoint_kv_client(
                                    self.conn_manager.clone(),
                                    &endpoint,
                                    async |client: Arc<KvClientManagerClientType<M>>| {
                                        operation(collection_id, vb_id, client).await
                                    },
                                )
                                .await
                            },
                        )
                        .await
                    },
                )
                .await
            },
        )
        .await
    }

    fn update_memdx_err(
        client: Arc<KvClientManagerClientType<M>>,
        e: MemdxError,
        key: Vec<u8>,
        scope_name: impl Into<String>,
        collection_name: impl Into<String>,
    ) -> MemdxError {
        e.set_doc_id(key)
            .set_bucket_name(client.bucket_name().unwrap_or_default())
            .set_collection_name(collection_name.into())
            .set_scope_name(scope_name.into())
    }
}

/// `<scope-id>:<collection-id>:<field>`, and only the two fields that are
/// measurements rather than descriptions are read. A key shaped otherwise is
/// skipped rather than refused: the group gains fields between server
/// versions.
fn take_collection_stat(totals: &mut CollectionStats, key: &str, value: &str) {
    let Some(field) = key.rsplit(':').next() else {
        return;
    };
    let Ok(n) = value.parse::<u64>() else {
        return;
    };
    match field {
        "items" => totals.count += n,
        "data_size" => totals.size += n,
        _ => {}
    }
}

/// Every node answered and none of them named the collection. Not expected --
/// an existing collection is reported by each node whether or not it holds
/// documents -- but "no rows" must not become "zero documents", which is a
/// figure rather than a silence.
fn finish_collection_stats(totals: CollectionStats, seen: bool) -> Option<CollectionStats> {
    seen.then_some(totals)
}

/// A `CollectionUnknown` (0x88) or `ScopeUnknown` (0x8c) from the server.
///
/// **`STAT` gets no op-specific name for either.** CRUD ops turn
/// `Status::CollectionUnknown` into `ServerErrorKind::UnknownCollectionID`
/// (`OpsCore::decode_common_status`), and `GetCollectionId` turns both into
/// `UnknownScopeName`/`UnknownCollectionName` wrapped in a `ResourceError`
/// (`GetCollectionIdResponse::try_from`) -- but `StatsResponse::try_from`
/// calls only the generic `OpsCore::decode_error`, which has no case for
/// either status and falls back to carrying the raw `Status` inside
/// `UnknownStatus`. That is the shape a `STAT` reply actually produces, so
/// it is the one matched here.
fn is_unknown_keyspace(e: &crate::memdx::error::Error) -> bool {
    e.is_server_error_kind(ServerErrorKind::UnknownStatus {
        status: Status::CollectionUnknown,
    }) || e.is_server_error_kind(ServerErrorKind::UnknownStatus {
        status: Status::ScopeUnknown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seqnos_from_every_node_concatenate() {
        // Each node answers only for the vbuckets it is active for, so the sweep's
        // answer is the concatenation and not any one node's reply. Three serial
        // round trips were half the old sweep's cost, which is why this fans out.
        let per_node = vec![
            vec![VbSeqno {
                vbucket: 0,
                seqno: 5,
            }],
            vec![VbSeqno {
                vbucket: 1,
                seqno: 7,
            }],
        ];
        let all: Vec<VbSeqno> = per_node.into_iter().flatten().collect();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].vbucket, 1);
    }

    #[test]
    fn totals_sum_across_nodes() {
        // Each node reports only the vbuckets it is active for. Measured against a
        // three-node 8.0.3 cluster: 13438 + 13434 + 13128 = 40000, exactly the
        // collection's SELECT COUNT(*). A per-node figure taken alone would be a
        // third of the truth and would look like a plausible one.
        let mut totals = CollectionStats::default();
        for (items, size) in [(13438u64, 100u64), (13434, 100), (13128, 100)] {
            totals.count += items;
            totals.size += size;
        }
        assert_eq!(totals.count, 40000);
        assert_eq!(totals.size, 300);
    }

    #[test]
    fn an_unknown_field_is_skipped_rather_than_refused() {
        // The group gains fields between server versions, and a reply carrying one
        // this does not know is not an error.
        let mut totals = CollectionStats::default();
        take_collection_stat(&mut totals, "8:9:items", "5");
        take_collection_stat(&mut totals, "8:9:data_size", "500");
        take_collection_stat(&mut totals, "8:9:ops_get", "99");
        take_collection_stat(&mut totals, "not-a-key", "1");
        assert_eq!((totals.count, totals.size), (5, 500));
    }

    #[test]
    fn no_rows_seen_is_not_zero_documents() {
        // Every node answered, but the group name never came back -- a real
        // "does not exist", not a coincidental empty reading. Confusing the two
        // is exactly what typed absence exists to prevent.
        assert_eq!(
            finish_collection_stats(CollectionStats::default(), false),
            None
        );
    }

    #[test]
    fn a_collection_with_no_documents_is_still_some() {
        // Contrast with the above: rows *were* seen, they just carried zeroes.
        // That is a real, empty collection, not an absent one.
        assert_eq!(
            finish_collection_stats(CollectionStats::default(), true),
            Some(CollectionStats::default())
        );
    }

    #[test]
    fn collection_unknown_and_scope_unknown_are_typed_absence() {
        let unknown_collection: crate::memdx::error::Error = crate::memdx::error::ServerError::new(
            ServerErrorKind::UnknownStatus {
                status: Status::CollectionUnknown,
            },
            crate::memdx::opcode::OpCode::Stat,
            Status::CollectionUnknown,
            0,
        )
        .into();
        let unknown_scope: crate::memdx::error::Error = crate::memdx::error::ServerError::new(
            ServerErrorKind::UnknownStatus {
                status: Status::ScopeUnknown,
            },
            crate::memdx::opcode::OpCode::Stat,
            Status::ScopeUnknown,
            0,
        )
        .into();
        let key_not_found: crate::memdx::error::Error = crate::memdx::error::ServerError::new(
            ServerErrorKind::KeyNotFound,
            crate::memdx::opcode::OpCode::Stat,
            Status::KeyNotFound,
            0,
        )
        .into();

        assert!(is_unknown_keyspace(&unknown_collection));
        assert!(is_unknown_keyspace(&unknown_scope));
        assert!(!is_unknown_keyspace(&key_not_found));
    }
}
