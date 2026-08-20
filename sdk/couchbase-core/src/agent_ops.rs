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
use crate::agent::{Agent, AgentInner};
use crate::cbconfig::{CollectionManifest, FullBucketConfig, FullClusterConfig};
use crate::clusterlabels::ClusterLabels;
use crate::error::Result;
use crate::features::BucketFeature;
use crate::memdx::response::VbSeqno;
use crate::mgmtx::bucket_settings::BucketDef;
use crate::mgmtx::metakv2::MetaKv2Entry;
use crate::mgmtx::mgmt::AutoFailoverSettings;
use crate::mgmtx::mgmt_query::IndexStatus;
use crate::mgmtx::responses::{
    CreateCollectionResponse, CreateScopeResponse, DeleteCollectionResponse, DeleteScopeResponse,
    GetMetaKv2DirResponse, MetaKv2MutationResponse, UpdateCollectionResponse,
};
use crate::mgmtx::user::{Group, RoleAndDescription, UserAndMetadata};
use crate::options::analytics::{AnalyticsOptions, GetPendingMutationsOptions};
use crate::options::crud::{
    AddOptions, AppendOptions, DecrementOptions, DeleteOptions, GetAndLockOptions,
    GetAndTouchOptions, GetCollectionIdOptions, GetMetaOptions, GetOptions, IncrementOptions,
    LookupInOptions, MutateInOptions, PrependOptions, ReplaceOptions, TouchOptions, UnlockOptions,
    UpsertOptions,
};
use crate::options::diagnostics::DiagnosticsOptions;
use crate::options::index::IndexScanOptions;
use crate::options::management::{
    ChangePasswordOptions, CreateBucketOptions, CreateCollectionOptions, CreateScopeOptions,
    DeleteBucketOptions, DeleteCollectionOptions, DeleteGroupOptions, DeleteMetaKv2DirOptions,
    DeleteScopeOptions, DeleteUserOptions, EnsureBucketOptions, EnsureGroupOptions,
    EnsureManifestOptions, EnsureUserOptions, FlushBucketOptions, GetAllBucketsOptions,
    GetAllGroupsOptions, GetAllUsersOptions, GetAutoFailoverSettingsOptions, GetBucketOptions,
    GetBucketStatsOptions, GetCollectionManifestOptions, GetFullBucketConfigOptions,
    GetFullClusterConfigOptions, GetGroupOptions, GetMetaKv2DirOptions, GetMetaKv2Options,
    GetRolesOptions, GetUserOptions, IndexStatusOptions, LoadSampleBucketOptions,
    MayManageLocalUsersOptions, SetMetaKv2MultipleOptions, SetMetaKv2Options,
    SyncMetaKv2QuorumOptions, UpdateBucketOptions, UpdateCollectionOptions, UpsertGroupOptions,
    UpsertUserOptions,
};
use crate::options::ping::PingOptions;
use crate::options::query::{
    BuildDeferredIndexesOptions, CreateIndexOptions, CreatePrimaryIndexOptions, DropIndexOptions,
    DropPrimaryIndexOptions, EnsureIndexOptions, GetAllIndexesOptions, QueryOptions,
    WatchIndexesOptions,
};
use crate::options::rangescan::RangeScanCreateOptions;
use crate::options::search::SearchOptions;
use crate::options::search_management;
use crate::options::search_management::{
    AllowQueryingOptions, AnalyzeDocumentOptions, DeleteIndexOptions, DisallowQueryingOptions,
    FreezePlanOptions, GetIndexOptions, GetIndexedDocumentsCountOptions, PauseIngestOptions,
    ResumeIngestOptions, UnfreezePlanOptions, UpsertIndexOptions,
};
use crate::options::stats::{CollectionStatsOptions, StatsByVbucketOptions, StatsOptions};
use crate::options::vbucket_seqnos::VbucketSeqnosOptions;
use crate::options::waituntilready::WaitUntilReadyOptions;
use crate::queryx::index::Index;
use crate::results::analytics::AnalyticsResultStream;
use crate::results::diagnostics::DiagnosticsResult;
use crate::results::index_scan::IndexScanResults;
use crate::results::kv::{
    AddResult, AppendResult, DecrementResult, DeleteResult, GetAndLockResult, GetAndTouchResult,
    GetCollectionIdResult, GetMetaResult, GetResult, IncrementResult, LookupInResult,
    MutateInResult, PrependResult, ReplaceResult, TouchResult, UnlockResult, UpsertResult,
};
use crate::results::pingreport::PingReport;
use crate::results::query::QueryResultStream;
use crate::results::rangescan::RangeScanCreateResult;
use crate::results::search::SearchResultStream;
use crate::results::stats::{CollectionStats, StatsEntry, StatsResult};
use crate::searchx;
use crate::searchx::document_analysis::DocumentAnalysis;
use serde_json::value::RawValue;
use std::collections::HashMap;
use std::sync::Arc;

#[cfg(feature = "top-level-spans")]
use {
    crate::create_span,
    crate::tracingcomponent::{
        build_keyspace, record_metrics, Keyspace, SpanBuilder, SPAN_ATTRIB_OTEL_KIND_CLIENT_VALUE,
        SPAN_ATTRIB_OTEL_KIND_KEY,
    },
    futures::Future,
    std::time::Instant,
    tracing::Instrument,
};

impl Agent {
    #[cfg(feature = "top-level-spans")]
    async fn execute_observable_operation<'k, F, Fut, T>(
        &self,
        service: Option<&'static str>,
        keyspace: Keyspace<'k>,
        mut span: SpanBuilder,
        f: F,
    ) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let operation_name = span.name();
        let cluster_labels = self.inner.tracing.get_cluster_labels();

        let span = span
            .with_cluster_labels(&cluster_labels)
            .with_service(service)
            .with_keyspace(&keyspace)
            .build();

        let start = Instant::now();
        let result = (f)().instrument(span.clone()).await;

        span.record(
            "otel.status_code",
            if result.is_err() { "error" } else { "ok" },
        );
        drop(span);

        record_metrics(
            operation_name,
            service,
            &keyspace,
            &cluster_labels,
            start,
            result.as_ref().err(),
        );

        result
    }

    pub async fn bucket_features(&self) -> Result<Vec<BucketFeature>> {
        self.inner.bucket_features().await
    }

    pub fn cluster_labels(&self) -> Option<ClusterLabels> {
        self.inner.tracing.get_cluster_labels()
    }

    pub async fn upsert(&self, opts: UpsertOptions<'_>) -> Result<UpsertResult> {
        #[cfg(feature = "top-level-spans")]
        {
            let bucket_name = self.get_bucket_name();
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_KV),
                    build_keyspace(
                        bucket_name.as_deref(),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("upsert"),
                    || self.inner.crud.upsert(opts),
                )
                .await;
        }

        self.inner.crud.upsert(opts).await
    }

    pub async fn get(&self, opts: GetOptions<'_>) -> Result<GetResult> {
        #[cfg(feature = "top-level-spans")]
        {
            let bucket_name = self.get_bucket_name();
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_KV),
                    build_keyspace(
                        bucket_name.as_deref(),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("get"),
                    || self.inner.crud.get(opts),
                )
                .await;
        }
        self.inner.crud.get(opts).await
    }

    pub async fn get_meta(&self, opts: GetMetaOptions<'_>) -> Result<GetMetaResult> {
        #[cfg(feature = "top-level-spans")]
        {
            let bucket_name = self.get_bucket_name();
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_KV),
                    build_keyspace(
                        bucket_name.as_deref(),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("get_meta"),
                    || self.inner.crud.get_meta(opts),
                )
                .await;
        }
        self.inner.crud.get_meta(opts).await
    }

    pub async fn delete(&self, opts: DeleteOptions<'_>) -> Result<DeleteResult> {
        #[cfg(feature = "top-level-spans")]
        {
            let bucket_name = self.get_bucket_name();
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_KV),
                    build_keyspace(
                        bucket_name.as_deref(),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("delete").with_durability(opts.durability_level.as_ref()),
                    || self.inner.crud.delete(opts),
                )
                .await;
        }
        self.inner.crud.delete(opts).await
    }

    pub async fn get_and_lock(&self, opts: GetAndLockOptions<'_>) -> Result<GetAndLockResult> {
        #[cfg(feature = "top-level-spans")]
        {
            let bucket_name = self.get_bucket_name();
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_KV),
                    build_keyspace(
                        bucket_name.as_deref(),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("get_and_lock"),
                    || self.inner.crud.get_and_lock(opts),
                )
                .await;
        }
        self.inner.crud.get_and_lock(opts).await
    }

    pub async fn get_and_touch(&self, opts: GetAndTouchOptions<'_>) -> Result<GetAndTouchResult> {
        #[cfg(feature = "top-level-spans")]
        {
            let bucket_name = self.get_bucket_name();
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_KV),
                    build_keyspace(
                        bucket_name.as_deref(),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("get_and_touch"),
                    || self.inner.crud.get_and_touch(opts),
                )
                .await;
        }
        self.inner.crud.get_and_touch(opts).await
    }

    pub async fn unlock(&self, opts: UnlockOptions<'_>) -> Result<UnlockResult> {
        #[cfg(feature = "top-level-spans")]
        {
            let bucket_name = self.get_bucket_name();
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_KV),
                    build_keyspace(
                        bucket_name.as_deref(),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("unlock"),
                    || self.inner.crud.unlock(opts),
                )
                .await;
        }
        self.inner.crud.unlock(opts).await
    }

    pub async fn touch(&self, opts: TouchOptions<'_>) -> Result<TouchResult> {
        #[cfg(feature = "top-level-spans")]
        {
            let bucket_name = self.get_bucket_name();
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_KV),
                    build_keyspace(
                        bucket_name.as_deref(),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("touch"),
                    || self.inner.crud.touch(opts),
                )
                .await;
        }
        self.inner.crud.touch(opts).await
    }

    pub async fn add(&self, opts: AddOptions<'_>) -> Result<AddResult> {
        #[cfg(feature = "top-level-spans")]
        {
            let bucket_name = self.get_bucket_name();
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_KV),
                    build_keyspace(
                        bucket_name.as_deref(),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("add").with_durability(opts.durability_level.as_ref()),
                    || self.inner.crud.add(opts),
                )
                .await;
        }
        self.inner.crud.add(opts).await
    }

    pub async fn replace(&self, opts: ReplaceOptions<'_>) -> Result<ReplaceResult> {
        #[cfg(feature = "top-level-spans")]
        {
            let bucket_name = self.get_bucket_name();
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_KV),
                    build_keyspace(
                        bucket_name.as_deref(),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("replace").with_durability(opts.durability_level.as_ref()),
                    || self.inner.crud.replace(opts),
                )
                .await;
        }
        self.inner.crud.replace(opts).await
    }

    pub async fn append(&self, opts: AppendOptions<'_>) -> Result<AppendResult> {
        #[cfg(feature = "top-level-spans")]
        {
            let bucket_name = self.get_bucket_name();
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_KV),
                    build_keyspace(
                        bucket_name.as_deref(),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("append").with_durability(opts.durability_level.as_ref()),
                    || self.inner.crud.append(opts),
                )
                .await;
        }
        self.inner.crud.append(opts).await
    }

    pub async fn prepend(&self, opts: PrependOptions<'_>) -> Result<PrependResult> {
        #[cfg(feature = "top-level-spans")]
        {
            let bucket_name = self.get_bucket_name();
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_KV),
                    build_keyspace(
                        bucket_name.as_deref(),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("prepend").with_durability(opts.durability_level.as_ref()),
                    || self.inner.crud.prepend(opts),
                )
                .await;
        }
        self.inner.crud.prepend(opts).await
    }

    pub async fn increment(&self, opts: IncrementOptions<'_>) -> Result<IncrementResult> {
        #[cfg(feature = "top-level-spans")]
        {
            let bucket_name = self.get_bucket_name();
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_KV),
                    build_keyspace(
                        bucket_name.as_deref(),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("increment"),
                    || self.inner.crud.increment(opts),
                )
                .await;
        }
        self.inner.crud.increment(opts).await
    }

    pub async fn decrement(&self, opts: DecrementOptions<'_>) -> Result<DecrementResult> {
        #[cfg(feature = "top-level-spans")]
        {
            let bucket_name = self.get_bucket_name();
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_KV),
                    build_keyspace(
                        bucket_name.as_deref(),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("decrement"),
                    || self.inner.crud.decrement(opts),
                )
                .await;
        }
        self.inner.crud.decrement(opts).await
    }

    /// How many vbuckets the selected bucket has.
    ///
    /// A whole-collection range scan is one scan per vbucket, so this is the
    /// width of the fan-out a caller has to run.
    pub async fn num_vbuckets(&self) -> Result<usize> {
        self.inner.num_vbuckets()
    }

    /// Open a range scan on one vbucket.
    ///
    /// **Which connection manager the scan uses is decided here, not by the
    /// caller.** A scan holds its connection until it drains, so it must not
    /// share one with point operations -- and because the returned handle keeps
    /// the connection for its continues, naming it once at create names it for
    /// the whole scan.
    pub async fn range_scan_create(
        &self,
        opts: RangeScanCreateOptions<'_>,
    ) -> Result<RangeScanCreateResult> {
        let vbucket_id = opts.vbucket_id;

        self.run_with_bucket_feature_check(
            BucketFeature::RangeScan,
            || async {
                let (resp, client) = self.inner.crud.range_scan_create(opts).await?;

                Ok(RangeScanCreateResult::new(
                    resp.scan_uuid,
                    vbucket_id,
                    client,
                ))
            },
            "range scan is not supported by this bucket",
        )
        .await
    }

    /// Sweep `STAT` across every KV node, calling `data_cb` once per stat.
    ///
    /// **A multi-response operation, so it runs on the bulk connection manager**
    /// — the one range scans use. That is decided by how many responses `STAT`
    /// gets, not by what it is called: a sweep holds its connection until the
    /// node has finished listing, and a point operation queued behind it would
    /// wait for the whole listing.
    pub async fn stats<F>(&self, opts: StatsOptions<'_>, data_cb: F) -> Result<StatsResult>
    where
        F: FnMut(StatsEntry) + Send,
    {
        self.inner.crud.stats(opts, data_cb).await
    }

    /// Ask `STAT` of the node holding one vbucket. Also on the bulk manager.
    pub async fn stats_by_vbucket<F>(
        &self,
        opts: StatsByVbucketOptions<'_>,
        data_cb: F,
    ) -> Result<StatsResult>
    where
        F: FnMut(StatsEntry) + Send,
    {
        self.inner.crud.stats_by_vbucket(opts, data_cb).await
    }

    /// Ask KV what it measures about one collection.
    ///
    /// `Ok(None)` means the scope or collection does not exist -- distinct
    /// from `Ok(Some(CollectionStats { count: 0, .. }))`, an existing
    /// collection with nothing in it.
    pub async fn collection_stats(
        &self,
        opts: CollectionStatsOptions<'_>,
    ) -> Result<Option<CollectionStats>> {
        self.inner.crud.collection_stats(opts).await
    }

    /// Fan out `GET_ALL_VB_SEQNOS` across every KV node and concatenate the
    /// answers: every active vbucket's current high sequence number.
    ///
    /// **On the point-operation manager, not the bulk one** — the opposite of
    /// `stats` above, and for the same reason stated the opposite way: a single
    /// packet answers, so nothing here holds its connection. This sits on the
    /// critical path of every `at_plus` scan, where `stats` does not, which is
    /// also why the nodes are asked together rather than in turn.
    pub async fn vbucket_seqnos_active(&self, opts: VbucketSeqnosOptions) -> Result<Vec<VbSeqno>> {
        self.inner.crud.vbucket_seqnos_active(opts).await
    }

    pub async fn get_collection_id(
        &self,
        opts: GetCollectionIdOptions<'_>,
    ) -> Result<GetCollectionIdResult> {
        self.inner.crud.get_collection_id(opts).await
    }

    pub async fn lookup_in(&self, opts: LookupInOptions<'_>) -> Result<LookupInResult> {
        #[cfg(feature = "top-level-spans")]
        {
            let bucket_name = self.get_bucket_name();
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_KV),
                    build_keyspace(
                        bucket_name.as_deref(),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("lookup_in"),
                    || self.inner.crud.lookup_in(opts),
                )
                .await;
        }
        self.inner.crud.lookup_in(opts).await
    }

    pub async fn mutate_in(&self, opts: MutateInOptions<'_>) -> Result<MutateInResult> {
        #[cfg(feature = "top-level-spans")]
        {
            let bucket_name = self.get_bucket_name();
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_KV),
                    build_keyspace(
                        bucket_name.as_deref(),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("mutate_in").with_durability(opts.durability_level.as_ref()),
                    || self.inner.crud.mutate_in(opts),
                )
                .await;
        }
        self.inner.crud.mutate_in(opts).await
    }

    pub async fn query(&self, opts: QueryOptions) -> Result<QueryResultStream> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_QUERY),
                    Keyspace::Cluster,
                    create_span!("query").with_statement(opts.statement.as_deref().unwrap_or("")),
                    || self.inner.query.query(opts),
                )
                .await;
        }
        self.inner.query.query(opts).await
    }

    /// Read a secondary index directly, without going through the query
    /// service.
    ///
    /// The index is named, and the router turns that into a `defnId` and one
    /// connection per host holding a piece of the index — so a partitioned index
    /// is read whole rather than one node's share of it.
    ///
    /// **The entries are not globally ordered**, and
    /// [`crate::results::index_scan`] is the contract: sorted within each host's
    /// stream, unordered across them, and merging them is the caller's because
    /// the comparison needs the caller's collation. An index that is not
    /// partitioned has one stream and is therefore ordered;
    /// [`IndexScanResults::is_index_ordered`](crate::results::index_scan::IndexScanResults::is_index_ordered)
    /// is how to check rather than assume.
    pub async fn index_scan(&self, opts: &IndexScanOptions<'_>) -> Result<IndexScanResults> {
        let bucket_name = self
            .inner
            .get_bucket_name()
            .ok_or_else(|| crate::error::Error::from(crate::error::ErrorKind::NoBucket))?;

        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_INDEX),
                    build_keyspace(
                        Some(&bucket_name),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("index_scan"),
                    || self.inner.index.scan(&bucket_name, opts),
                )
                .await;
        }

        #[cfg(not(feature = "top-level-spans"))]
        self.inner.index.scan(&bucket_name, opts).await
    }

    pub async fn prepared_query(&self, opts: QueryOptions) -> Result<QueryResultStream> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_QUERY),
                    Keyspace::Cluster,
                    create_span!("query").with_statement(opts.statement.as_deref().unwrap_or("")),
                    || self.inner.query.prepared_query(opts),
                )
                .await;
        }
        self.inner.query.prepared_query(opts).await
    }

    pub async fn get_all_indexes(&self, opts: &GetAllIndexesOptions<'_>) -> Result<Vec<Index>> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_QUERY),
                    build_keyspace(
                        Some(opts.bucket_name),
                        opts.scope_name,
                        opts.collection_name,
                    ),
                    create_span!("manager_query_get_all_indexes"),
                    || self.inner.query.get_all_indexes(opts),
                )
                .await;
        }
        self.inner.query.get_all_indexes(opts).await
    }

    pub async fn create_primary_index(&self, opts: &CreatePrimaryIndexOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_QUERY),
                    build_keyspace(
                        Some(opts.bucket_name),
                        opts.scope_name,
                        opts.collection_name,
                    ),
                    create_span!("manager_query_create_primary_index"),
                    || self.inner.query.create_primary_index(opts),
                )
                .await;
        }
        self.inner.query.create_primary_index(opts).await
    }

    pub async fn create_index(&self, opts: &CreateIndexOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_QUERY),
                    build_keyspace(
                        Some(opts.bucket_name),
                        opts.scope_name,
                        opts.collection_name,
                    ),
                    create_span!("manager_query_create_index"),
                    || self.inner.query.create_index(opts),
                )
                .await;
        }
        self.inner.query.create_index(opts).await
    }

    pub async fn drop_primary_index(&self, opts: &DropPrimaryIndexOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_QUERY),
                    build_keyspace(
                        Some(opts.bucket_name),
                        opts.scope_name,
                        opts.collection_name,
                    ),
                    create_span!("manager_query_drop_primary_index"),
                    || self.inner.query.drop_primary_index(opts),
                )
                .await;
        }
        self.inner.query.drop_primary_index(opts).await
    }

    pub async fn drop_index(&self, opts: &DropIndexOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_QUERY),
                    build_keyspace(
                        Some(opts.bucket_name),
                        opts.scope_name,
                        opts.collection_name,
                    ),
                    create_span!("manager_query_drop_index"),
                    || self.inner.query.drop_index(opts),
                )
                .await;
        }
        self.inner.query.drop_index(opts).await
    }

    pub async fn build_deferred_indexes(
        &self,
        opts: &BuildDeferredIndexesOptions<'_>,
    ) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_QUERY),
                    build_keyspace(
                        Some(opts.bucket_name),
                        opts.scope_name,
                        opts.collection_name,
                    ),
                    create_span!("manager_query_build_deferred_indexes"),
                    || self.inner.query.build_deferred_indexes(opts),
                )
                .await;
        }
        self.inner.query.build_deferred_indexes(opts).await
    }

    pub async fn watch_indexes(&self, opts: &WatchIndexesOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_QUERY),
                    build_keyspace(
                        Some(opts.bucket_name),
                        opts.scope_name,
                        opts.collection_name,
                    ),
                    create_span!("manager_query_watch_indexes"),
                    || self.inner.query.watch_indexes(opts),
                )
                .await;
        }
        self.inner.query.watch_indexes(opts).await
    }

    pub async fn ensure_index(&self, opts: &EnsureIndexOptions<'_>) -> Result<()> {
        self.inner.query.ensure_index(opts).await
    }

    pub async fn search(&self, opts: SearchOptions) -> Result<SearchResultStream> {
        #[cfg(feature = "top-level-spans")]
        {
            let bucket_name = opts.bucket_name.clone();
            let scope_name = opts.scope_name.clone();
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_SEARCH),
                    build_keyspace(bucket_name.as_deref(), scope_name.as_deref(), None),
                    create_span!("search"),
                    || self.inner.search.query(opts),
                )
                .await;
        }
        self.inner.search.query(opts).await
    }

    pub async fn get_search_index(
        &self,
        opts: &GetIndexOptions<'_>,
    ) -> Result<searchx::index::Index> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_SEARCH),
                    build_keyspace(opts.bucket_name, opts.scope_name, None),
                    create_span!("manager_search_get_index"),
                    || self.inner.search.get_index(opts),
                )
                .await;
        }
        self.inner.search.get_index(opts).await
    }

    pub async fn upsert_search_index(&self, opts: &UpsertIndexOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_SEARCH),
                    build_keyspace(opts.bucket_name, opts.scope_name, None),
                    create_span!("manager_search_upsert_index"),
                    || self.inner.search.upsert_index(opts),
                )
                .await;
        }
        self.inner.search.upsert_index(opts).await
    }

    pub async fn delete_search_index(&self, opts: &DeleteIndexOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_SEARCH),
                    build_keyspace(opts.bucket_name, opts.scope_name, None),
                    create_span!("manager_search_drop_index"),
                    || self.inner.search.delete_index(opts),
                )
                .await;
        }
        self.inner.search.delete_index(opts).await
    }

    pub async fn get_all_search_indexes(
        &self,
        opts: &search_management::GetAllIndexesOptions<'_>,
    ) -> Result<Vec<searchx::index::Index>> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_SEARCH),
                    build_keyspace(opts.bucket_name, opts.scope_name, None),
                    create_span!("manager_search_get_all_indexes"),
                    || self.inner.search.get_all_indexes(opts),
                )
                .await;
        }
        self.inner.search.get_all_indexes(opts).await
    }

    pub async fn analyze_search_document(
        &self,
        opts: &AnalyzeDocumentOptions<'_>,
    ) -> Result<DocumentAnalysis> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_SEARCH),
                    build_keyspace(opts.bucket_name, opts.scope_name, None),
                    create_span!("manager_search_analyze_document"),
                    || self.inner.search.analyze_document(opts),
                )
                .await;
        }
        self.inner.search.analyze_document(opts).await
    }

    pub async fn get_search_indexed_documents_count(
        &self,
        opts: &GetIndexedDocumentsCountOptions<'_>,
    ) -> Result<u64> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_SEARCH),
                    build_keyspace(opts.bucket_name, opts.scope_name, None),
                    create_span!("manager_search_get_indexed_documents_count"),
                    || self.inner.search.get_indexed_documents_count(opts),
                )
                .await;
        }
        self.inner.search.get_indexed_documents_count(opts).await
    }

    pub async fn pause_search_index_ingest(&self, opts: &PauseIngestOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_SEARCH),
                    build_keyspace(opts.bucket_name, opts.scope_name, None),
                    create_span!("manager_search_pause_ingest"),
                    || self.inner.search.pause_ingest(opts),
                )
                .await;
        }
        self.inner.search.pause_ingest(opts).await
    }

    pub async fn resume_search_index_ingest(&self, opts: &ResumeIngestOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_SEARCH),
                    build_keyspace(opts.bucket_name, opts.scope_name, None),
                    create_span!("manager_search_resume_ingest"),
                    || self.inner.search.resume_ingest(opts),
                )
                .await;
        }
        self.inner.search.resume_ingest(opts).await
    }

    pub async fn allow_search_index_querying(&self, opts: &AllowQueryingOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_SEARCH),
                    build_keyspace(opts.bucket_name, opts.scope_name, None),
                    create_span!("manager_search_allow_querying"),
                    || self.inner.search.allow_querying(opts),
                )
                .await;
        }
        self.inner.search.allow_querying(opts).await
    }

    pub async fn disallow_search_index_querying(
        &self,
        opts: &DisallowQueryingOptions<'_>,
    ) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_SEARCH),
                    build_keyspace(opts.bucket_name, opts.scope_name, None),
                    create_span!("manager_search_disallow_querying"),
                    || self.inner.search.disallow_querying(opts),
                )
                .await;
        }
        self.inner.search.disallow_querying(opts).await
    }

    pub async fn freeze_search_index_plan(&self, opts: &FreezePlanOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_SEARCH),
                    build_keyspace(opts.bucket_name, opts.scope_name, None),
                    create_span!("manager_search_freeze_plan"),
                    || self.inner.search.freeze_plan(opts),
                )
                .await;
        }
        self.inner.search.freeze_plan(opts).await
    }

    pub async fn unfreeze_search_index_plan(&self, opts: &UnfreezePlanOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_SEARCH),
                    build_keyspace(opts.bucket_name, opts.scope_name, None),
                    create_span!("manager_search_unfreeze_plan"),
                    || self.inner.search.unfreeze_plan(opts),
                )
                .await;
        }
        self.inner.search.unfreeze_plan(opts).await
    }

    pub async fn get_collection_manifest(
        &self,
        opts: &GetCollectionManifestOptions<'_>,
    ) -> Result<CollectionManifest> {
        self.inner.mgmt.get_collection_manifest(opts).await
    }

    pub async fn create_scope(&self, opts: &CreateScopeOptions<'_>) -> Result<CreateScopeResponse> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    build_keyspace(Some(opts.bucket_name), Some(opts.scope_name), None),
                    create_span!("manager_collections_create_scope"),
                    || self.inner.mgmt.create_scope(opts),
                )
                .await;
        }
        self.inner.mgmt.create_scope(opts).await
    }

    pub async fn delete_scope(&self, opts: &DeleteScopeOptions<'_>) -> Result<DeleteScopeResponse> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    build_keyspace(Some(opts.bucket_name), Some(opts.scope_name), None),
                    create_span!("manager_collections_drop_scope"),
                    || self.inner.mgmt.delete_scope(opts),
                )
                .await;
        }
        self.inner.mgmt.delete_scope(opts).await
    }

    pub async fn create_collection(
        &self,
        opts: &CreateCollectionOptions<'_>,
    ) -> Result<CreateCollectionResponse> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    build_keyspace(
                        Some(opts.bucket_name),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("manager_collections_create_collection"),
                    || self.inner.mgmt.create_collection(opts),
                )
                .await;
        }
        self.inner.mgmt.create_collection(opts).await
    }

    pub async fn delete_collection(
        &self,
        opts: &DeleteCollectionOptions<'_>,
    ) -> Result<DeleteCollectionResponse> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    build_keyspace(
                        Some(opts.bucket_name),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("manager_collections_drop_collection"),
                    || self.inner.mgmt.delete_collection(opts),
                )
                .await;
        }
        self.inner.mgmt.delete_collection(opts).await
    }

    pub async fn update_collection(
        &self,
        opts: &UpdateCollectionOptions<'_>,
    ) -> Result<UpdateCollectionResponse> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    build_keyspace(
                        Some(opts.bucket_name),
                        Some(opts.scope_name),
                        Some(opts.collection_name),
                    ),
                    create_span!("manager_collections_update_collection"),
                    || self.inner.mgmt.update_collection(opts),
                )
                .await;
        }
        self.inner.mgmt.update_collection(opts).await
    }

    pub async fn ensure_manifest(&self, opts: &EnsureManifestOptions<'_>) -> Result<()> {
        self.inner.mgmt.ensure_manifest(opts).await
    }

    pub async fn get_all_buckets(&self, opts: &GetAllBucketsOptions<'_>) -> Result<Vec<BucketDef>> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("manager_buckets_get_all_buckets"),
                    || self.inner.mgmt.get_all_buckets(opts),
                )
                .await;
        }
        self.inner.mgmt.get_all_buckets(opts).await
    }

    pub async fn get_bucket(&self, opts: &GetBucketOptions<'_>) -> Result<BucketDef> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    build_keyspace(Some(opts.bucket_name), None, None),
                    create_span!("manager_buckets_get_bucket"),
                    || self.inner.mgmt.get_bucket(opts),
                )
                .await;
        }
        self.inner.mgmt.get_bucket(opts).await
    }

    pub async fn create_bucket(&self, opts: &CreateBucketOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    build_keyspace(Some(opts.bucket_name), None, None),
                    create_span!("manager_buckets_create_bucket"),
                    || self.inner.mgmt.create_bucket(opts),
                )
                .await;
        }
        self.inner.mgmt.create_bucket(opts).await
    }

    pub async fn update_bucket(&self, opts: &UpdateBucketOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    build_keyspace(Some(opts.bucket_name), None, None),
                    create_span!("manager_buckets_update_bucket"),
                    || self.inner.mgmt.update_bucket(opts),
                )
                .await;
        }
        self.inner.mgmt.update_bucket(opts).await
    }

    pub async fn delete_bucket(&self, opts: &DeleteBucketOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    build_keyspace(Some(opts.bucket_name), None, None),
                    create_span!("manager_buckets_drop_bucket"),
                    || self.inner.mgmt.delete_bucket(opts),
                )
                .await;
        }
        self.inner.mgmt.delete_bucket(opts).await
    }

    pub async fn flush_bucket(&self, opts: &FlushBucketOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    build_keyspace(Some(opts.bucket_name), None, None),
                    create_span!("manager_buckets_flush_bucket"),
                    || self.inner.mgmt.flush_bucket(opts),
                )
                .await;
        }
        self.inner.mgmt.flush_bucket(opts).await
    }

    pub async fn get_user(&self, opts: &GetUserOptions<'_>) -> Result<UserAndMetadata> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("manager_users_get_user"),
                    || self.inner.mgmt.get_user(opts),
                )
                .await;
        }
        self.inner.mgmt.get_user(opts).await
    }

    /// Whether the caller may manage local users.
    ///
    /// See [`crate::mgmtx::mgmt::Management::may_manage_local_users`] for why
    /// this is answered by asking about a user that cannot exist.
    pub async fn may_manage_local_users(
        &self,
        opts: &MayManageLocalUsersOptions<'_>,
    ) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("manager_users_may_manage_local_users"),
                    || self.inner.mgmt.may_manage_local_users(opts),
                )
                .await;
        }
        self.inner.mgmt.may_manage_local_users(opts).await
    }

    pub async fn get_all_users(
        &self,
        opts: &GetAllUsersOptions<'_>,
    ) -> Result<Vec<UserAndMetadata>> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("manager_users_get_all_users"),
                    || self.inner.mgmt.get_all_users(opts),
                )
                .await;
        }
        self.inner.mgmt.get_all_users(opts).await
    }

    pub async fn upsert_user(&self, opts: &UpsertUserOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("manager_users_upsert_user"),
                    || self.inner.mgmt.upsert_user(opts),
                )
                .await;
        }
        self.inner.mgmt.upsert_user(opts).await
    }

    pub async fn delete_user(&self, opts: &DeleteUserOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("manager_users_drop_user"),
                    || self.inner.mgmt.delete_user(opts),
                )
                .await;
        }
        self.inner.mgmt.delete_user(opts).await
    }

    pub async fn get_roles(&self, opts: &GetRolesOptions<'_>) -> Result<Vec<RoleAndDescription>> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("manager_users_get_roles"),
                    || self.inner.mgmt.get_roles(opts),
                )
                .await;
        }
        self.inner.mgmt.get_roles(opts).await
    }

    pub async fn get_group(&self, opts: &GetGroupOptions<'_>) -> Result<Group> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("manager_users_get_group"),
                    || self.inner.mgmt.get_group(opts),
                )
                .await;
        }
        self.inner.mgmt.get_group(opts).await
    }

    pub async fn get_all_groups(&self, opts: &GetAllGroupsOptions<'_>) -> Result<Vec<Group>> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("manager_users_get_all_groups"),
                    || self.inner.mgmt.get_all_groups(opts),
                )
                .await;
        }
        self.inner.mgmt.get_all_groups(opts).await
    }

    pub async fn upsert_group(&self, opts: &UpsertGroupOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("manager_users_upsert_group"),
                    || self.inner.mgmt.upsert_group(opts),
                )
                .await;
        }
        self.inner.mgmt.upsert_group(opts).await
    }

    pub async fn delete_group(&self, opts: &DeleteGroupOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("manager_users_drop_group"),
                    || self.inner.mgmt.delete_group(opts),
                )
                .await;
        }
        self.inner.mgmt.delete_group(opts).await
    }

    pub async fn change_password(&self, opts: &ChangePasswordOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("manager_users_change_password"),
                    || self.inner.mgmt.change_password(opts),
                )
                .await;
        }
        self.inner.mgmt.change_password(opts).await
    }

    pub async fn ping(&self, opts: &PingOptions) -> Result<PingReport> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("ping"),
                    || self.inner.diagnostics.ping(opts),
                )
                .await;
        }
        self.inner.diagnostics.ping(opts).await
    }

    pub async fn ensure_user(&self, opts: &EnsureUserOptions<'_>) -> Result<()> {
        self.inner.mgmt.ensure_user(opts).await
    }

    pub async fn ensure_group(&self, opts: &EnsureGroupOptions<'_>) -> Result<()> {
        self.inner.mgmt.ensure_group(opts).await
    }

    pub async fn ensure_bucket(&self, opts: &EnsureBucketOptions<'_>) -> Result<()> {
        self.inner.mgmt.ensure_bucket(opts).await
    }

    pub async fn ensure_search_index(
        &self,
        opts: &search_management::EnsureIndexOptions<'_>,
    ) -> Result<()> {
        self.inner.search.ensure_index(opts).await
    }

    pub async fn wait_until_ready(&self, opts: &WaitUntilReadyOptions) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("wait_until_ready"),
                    || self.inner.diagnostics.wait_until_ready(opts),
                )
                .await;
        }
        self.inner.diagnostics.wait_until_ready(opts).await
    }

    pub async fn diagnostics(&self, opts: &DiagnosticsOptions) -> Result<DiagnosticsResult> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("diagnostics"),
                    || self.inner.diagnostics.diagnostics(opts),
                )
                .await;
        }
        self.inner.diagnostics.diagnostics(opts).await
    }

    pub async fn analytics_query(&self, opts: AnalyticsOptions) -> Result<AnalyticsResultStream> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_ANALYTICS),
                    Keyspace::Cluster,
                    create_span!("analytics")
                        .with_statement(opts.statement.as_deref().unwrap_or("")),
                    || self.inner.analytics.query(opts),
                )
                .await;
        }
        self.inner.analytics.query(opts).await
    }

    pub async fn analytics_get_pending_mutations(
        &self,
        opts: &GetPendingMutationsOptions<'_>,
    ) -> Result<HashMap<String, HashMap<String, i64>>> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_ANALYTICS),
                    Keyspace::Cluster,
                    create_span!("manager_analytics_get_pending_mutations"),
                    || self.inner.analytics.get_pending_mutations(opts),
                )
                .await;
        }
        self.inner.analytics.get_pending_mutations(opts).await
    }

    pub async fn get_full_bucket_config(
        &self,
        opts: &GetFullBucketConfigOptions<'_>,
    ) -> Result<FullBucketConfig> {
        self.inner.mgmt.get_full_bucket_config(opts).await
    }

    pub async fn get_full_cluster_config(
        &self,
        opts: &GetFullClusterConfigOptions<'_>,
    ) -> Result<FullClusterConfig> {
        self.inner.mgmt.get_full_cluster_config(opts).await
    }

    pub async fn load_sample_bucket(&self, opts: &LoadSampleBucketOptions<'_>) -> Result<()> {
        self.inner.mgmt.load_sample_bucket(opts).await
    }

    pub async fn index_status(&self, opts: &IndexStatusOptions<'_>) -> Result<IndexStatus> {
        self.inner.mgmt.index_status(opts).await
    }

    pub async fn get_auto_failover_settings(
        &self,
        opts: &GetAutoFailoverSettingsOptions<'_>,
    ) -> Result<AutoFailoverSettings> {
        self.inner.mgmt.get_auto_failover_settings(opts).await
    }

    pub async fn get_bucket_stats(
        &self,
        opts: &GetBucketStatsOptions<'_>,
    ) -> Result<Box<RawValue>> {
        self.inner.mgmt.get_bucket_stats(opts).await
    }

    /// Read one leaf from the internal metakv2 store.
    ///
    /// See [`crate::mgmtx::metakv2`] for what this store guarantees and what it
    /// does not — notably that a successful read is a snapshot, not necessarily a
    /// fresh one.
    pub async fn get_metakv2(&self, opts: &GetMetaKv2Options<'_>) -> Result<MetaKv2Entry> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("manager_metakv2_get"),
                    || self.inner.mgmt.get_metakv2(opts),
                )
                .await;
        }
        self.inner.mgmt.get_metakv2(opts).await
    }

    /// Read every leaf under a metakv2 directory as one cross-key consistent
    /// snapshot.
    pub async fn get_metakv2_dir(
        &self,
        opts: &GetMetaKv2DirOptions<'_>,
    ) -> Result<GetMetaKv2DirResponse> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("manager_metakv2_get_dir"),
                    || self.inner.mgmt.get_metakv2_dir(opts),
                )
                .await;
        }
        self.inner.mgmt.get_metakv2_dir(opts).await
    }

    /// Write one metakv2 leaf, optionally conditional on its current revision.
    pub async fn set_metakv2(
        &self,
        opts: &SetMetaKv2Options<'_>,
    ) -> Result<MetaKv2MutationResponse> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("manager_metakv2_set"),
                    || self.inner.mgmt.set_metakv2(opts),
                )
                .await;
        }
        self.inner.mgmt.set_metakv2(opts).await
    }

    /// Commit a set of metakv2 writes atomically.
    pub async fn set_metakv2_multiple(
        &self,
        opts: &SetMetaKv2MultipleOptions<'_>,
    ) -> Result<MetaKv2MutationResponse> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("manager_metakv2_set_multiple"),
                    || self.inner.mgmt.set_metakv2_multiple(opts),
                )
                .await;
        }
        self.inner.mgmt.set_metakv2_multiple(opts).await
    }

    /// Remove a metakv2 subtree and everything under it. There is no conditional
    /// delete: a revision is accepted by the endpoint and ignored.
    pub async fn delete_metakv2_dir(
        &self,
        opts: &DeleteMetaKv2DirOptions<'_>,
    ) -> Result<MetaKv2MutationResponse> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("manager_metakv2_delete_dir"),
                    || self.inner.mgmt.delete_metakv2_dir(opts),
                )
                .await;
        }
        self.inner.mgmt.delete_metakv2_dir(opts).await
    }

    /// Ask the node this lands on to confirm it still has quorum, which is the
    /// only measured way to tell a current node from a stale one.
    pub async fn sync_metakv2_quorum(&self, opts: &SyncMetaKv2QuorumOptions<'_>) -> Result<()> {
        #[cfg(feature = "top-level-spans")]
        {
            return self
                .execute_observable_operation(
                    Some(crate::tracingcomponent::SERVICE_VALUE_MANAGEMENT),
                    Keyspace::Cluster,
                    create_span!("manager_metakv2_sync_quorum"),
                    || self.inner.mgmt.sync_metakv2_quorum(opts),
                )
                .await;
        }
        self.inner.mgmt.sync_metakv2_quorum(opts).await
    }

    /// Every active vbucket's UUID, **cached** — the call a scan vector should
    /// make.
    ///
    /// One map per agent, shared by every consumer that builds a vector, so the
    /// consumer that can detect staleness refreshes the map the one that cannot
    /// is about to use. [`Agent::invalidate_vbuuids`] is how a refusal gets back
    /// here; `crate::vbuuid_cache` documents what else drops it.
    ///
    /// [`Agent::vbucket_vbuuids`] is the uncached read underneath, kept public
    /// because a benchmark measuring the cost of the read itself has to be able
    /// to reach past the thing that avoids it.
    ///
    /// **Reads the revision before fetching, and stores under that reading** —
    /// not one taken afterward. `config_revision` is published only once the
    /// components it describes have finished reconfiguring, so a revision read
    /// after the fetch could already describe a *newer* topology than the map
    /// just built from the old one, and the map would be cached under a key
    /// that will never see it invalidated by a config change that already
    /// happened.
    pub async fn vbuuid_map(&self) -> Result<Arc<HashMap<u16, u64>>> {
        let revision = self.config_revision();
        if let Some(map) = self.inner.vbuuids.peek(revision) {
            return Ok(map);
        }
        let fresh = Arc::new(self.vbucket_vbuuids().await?);
        self.inner.vbuuids.store(revision, Arc::clone(&fresh));
        Ok(fresh)
    }

    /// Report that `stale` was refused by the server, so the next
    /// [`Agent::vbuuid_map`] reads afresh.
    ///
    /// **The map that was refused, not "the map"** — several scans share one
    /// and each will report it, so a report has to be able to be a no-op. See
    /// `crate::vbuuid_cache::VbUuidCache::invalidate`.
    pub fn invalidate_vbuuids(&self, stale: &Arc<HashMap<u16, u64>>) {
        self.inner.vbuuids.invalidate(stale);
    }

    /// The uncached read underneath [`Agent::vbuuid_map`], kept public because a
    /// benchmark measuring the cost of the read itself has to be able to reach
    /// past the thing that avoids it.
    ///
    /// **All vbuckets or an error, never a partial map.** A scan vector
    /// missing a vbucket's uuid is not a smaller answer -- it is no answer at
    /// all for that vbucket, and a caller reading `Ok` has no way to tell a
    /// complete map from one a rebalance thinned out from under it. So the
    /// completeness check at the end treats anything short of every vbucket
    /// the same as none, and fails loudly enough that a caller can retry
    /// rather than build a scan vector with silent holes in it.
    pub async fn vbucket_vbuuids(&self) -> Result<HashMap<u16, u64>> {
        let n = self.num_vbuckets().await?;
        if n == 0 {
            return Err(crate::error::Error::new_message_error(
                "no vbucket map yet, so no scan vector can be built",
            ));
        }

        // Pure and in-memory -- the router is an `ArcSwap` read -- so a
        // vbucket with no active node right now (mid-failover) is simply
        // absent here rather than treated as a hard failure. The
        // completeness check below is what turns that absence into an
        // error; this loop does not need to.
        let mut endpoint_id_by_vb: HashMap<u16, Arc<str>> = HashMap::with_capacity(n);
        for vb in 0..n as u16 {
            if let Ok(id) = self.inner.crud.dispatch_to_vbucket(vb) {
                endpoint_id_by_vb.insert(vb, id);
            }
        }

        // Resolve each *distinct* endpoint id once, not once per vbucket: `n`
        // vbuckets share a handful of nodes. Unlike the loop above, a
        // resolution failure here is propagated rather than swallowed -- an
        // id the router just returned but the connection manager cannot
        // resolve means the two disagree about the topology, which is not
        // the same kind of "try again later" as a vbucket briefly having no
        // owner, and must not be allowed to quietly shrink the map.
        let mut canonical_by_id: HashMap<Arc<str>, Arc<str>> = HashMap::new();
        for id in endpoint_id_by_vb.values() {
            if canonical_by_id.contains_key(id) {
                continue;
            }
            let addr = self.inner.crud.resolve_canonical_addr(id).await?;
            canonical_by_id.insert(id.clone(), addr);
        }

        let mut active: HashMap<u16, String> = HashMap::with_capacity(n);
        for (vb, id) in &endpoint_id_by_vb {
            if let Some(addr) = canonical_by_id.get(id) {
                active.insert(*vb, addr.to_string());
            }
        }

        let mut vbuuids: HashMap<u16, u64> = HashMap::with_capacity(n);
        let mut bad: Option<String> = None;
        self.stats(StatsOptions::new("vbucket-seqno"), |entry| {
            let key = entry.key_str();
            let value = entry.value_str();
            if !take_vbuuid_line(&mut vbuuids, &active, &entry.endpoint, &key, &value)
                && bad.is_none()
            {
                bad = Some(format!("{key} = {value:?}"));
            }
        })
        .await?;

        if let Some(bad) = bad {
            return Err(crate::error::Error::new_message_error(format!(
                "could not parse vbucket-seqno {bad}"
            )));
        }
        require_complete(n, vbuuids)
    }
}

/// `Ok(vbuuids)` only if it has exactly one entry per vbucket in `0..n`;
/// otherwise an error naming the shortfall and, cheaply, which vbuckets are
/// missing.
///
/// **Strict equality, not merely non-empty.** A gap during a rebalance is not
/// a smaller scan vector, it is a hole a caller cannot see -- so anything
/// short of every vbucket is refused the same way an empty map is, and the
/// caller finds out from the error rather than from a scan that silently
/// skips vbuckets.
fn require_complete(n: usize, vbuuids: HashMap<u16, u64>) -> Result<HashMap<u16, u64>> {
    if vbuuids.len() == n {
        return Ok(vbuuids);
    }
    Err(crate::error::Error::new_message_error(format!(
        "got {} of {n} vbucket uuids; missing {}",
        vbuuids.len(),
        missing_vbuckets(n as u16, &vbuuids)
    )))
}

/// The vbuckets in `0..n` absent from `have`, as a short, readable list for
/// an error message -- capped rather than exhaustive, because the list
/// exists to make a small gap actionable at a glance, not to enumerate a
/// four-figure outage a reader would skim past anyway.
fn missing_vbuckets(n: u16, have: &HashMap<u16, u64>) -> String {
    const MAX_NAMED: usize = 20;
    let mut missing: Vec<u16> = (0..n).filter(|vb| !have.contains_key(vb)).collect();
    missing.sort_unstable();

    if missing.len() > MAX_NAMED {
        let shown = missing[..MAX_NAMED]
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        format!("{shown}, and {} more", missing.len() - MAX_NAMED)
    } else {
        missing
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Take one `vb_N:uuid` line if it came from the node active for that
/// vbucket. Returns `false` only for a `uuid` line whose value will not parse
/// — every other shape is skipped rather than refused, because the group
/// gains fields between server versions.
fn take_vbuuid_line(
    out: &mut HashMap<u16, u64>,
    active: &HashMap<u16, String>,
    endpoint: &str,
    key: &str,
    value: &str,
) -> bool {
    let Some(rest) = key.strip_prefix("vb_") else {
        return true;
    };
    let Some((vb, field)) = rest.split_once(':') else {
        return true;
    };
    if field != "uuid" {
        return true;
    }
    let Ok(vb) = vb.parse::<u16>() else {
        return true;
    };
    if active.get(&vb).map(String::as_str) != Some(endpoint) {
        return true;
    }
    match value.parse::<u64>() {
        Ok(v) => {
            out.insert(vb, v);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_active_node_s_uuid_line_is_taken() {
        // A replica normally shares the active's failover-log lineage, so taking
        // whichever answered would *usually* agree. "Usually" is not a property to
        // build a consistency guarantee on.
        let mut out = std::collections::HashMap::new();
        let active: std::collections::HashMap<u16, String> =
            [(7u16, "10.0.0.1:11210".to_string())].into_iter().collect();

        take_vbuuid_line(&mut out, &active, "10.0.0.1:11210", "vb_7:uuid", "12345");
        take_vbuuid_line(&mut out, &active, "10.0.0.2:11210", "vb_7:uuid", "99999");
        take_vbuuid_line(&mut out, &active, "10.0.0.1:11210", "vb_7:high_seqno", "4");

        assert_eq!(
            out.get(&7),
            Some(&12345u64),
            "the replica's uuid and the non-uuid field are both skipped"
        );
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn a_key_without_the_vb_prefix_is_skipped() {
        let mut out = HashMap::new();
        let active = HashMap::new();
        assert!(take_vbuuid_line(
            &mut out,
            &active,
            "10.0.0.1:11210",
            "curr_connections",
            "5"
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn a_vb_key_without_a_colon_is_skipped() {
        let mut out = HashMap::new();
        let active = HashMap::new();
        assert!(take_vbuuid_line(
            &mut out,
            &active,
            "10.0.0.1:11210",
            "vb_7",
            "12345"
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn a_field_other_than_uuid_is_skipped() {
        let mut out = HashMap::new();
        let active: HashMap<u16, String> =
            [(7u16, "10.0.0.1:11210".to_string())].into_iter().collect();
        assert!(take_vbuuid_line(
            &mut out,
            &active,
            "10.0.0.1:11210",
            "vb_7:high_seqno",
            "4"
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn an_unparsable_vbucket_number_is_skipped() {
        let mut out = HashMap::new();
        let active = HashMap::new();
        assert!(take_vbuuid_line(
            &mut out,
            &active,
            "10.0.0.1:11210",
            "vb_not-a-number:uuid",
            "12345"
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn a_line_from_a_node_not_active_for_that_vbucket_is_skipped() {
        let mut out = HashMap::new();
        let active: HashMap<u16, String> =
            [(7u16, "10.0.0.1:11210".to_string())].into_iter().collect();
        assert!(take_vbuuid_line(
            &mut out,
            &active,
            "10.0.0.2:11210",
            "vb_7:uuid",
            "12345"
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn an_unparsable_uuid_value_is_refused_not_skipped() {
        // The one shape that returns `false`: a `uuid` line, from the right
        // node, whose value will not parse. Everything else above is a silent
        // skip; this is the one case `vbucket_vbuuids` turns into an error.
        let mut out = HashMap::new();
        let active: HashMap<u16, String> =
            [(7u16, "10.0.0.1:11210".to_string())].into_iter().collect();
        assert!(!take_vbuuid_line(
            &mut out,
            &active,
            "10.0.0.1:11210",
            "vb_7:uuid",
            "not-a-number"
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn a_complete_map_passes_through_unchanged() {
        let vbuuids: HashMap<u16, u64> = [(0u16, 1u64), (1, 2), (2, 3)].into_iter().collect();
        let out = require_complete(3, vbuuids.clone()).expect("complete map is accepted");
        assert_eq!(out, vbuuids);
    }

    #[test]
    fn an_empty_map_is_refused_not_returned_as_ok() {
        // The original bug wore exactly this shape: every line skipped, no
        // parse error, `Ok(HashMap::new())`. This is the check that turns it
        // into a loud failure instead of a map that looks merely quiet.
        let err = require_complete(3, HashMap::new()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("got 0 of 3"), "{msg}");
        assert!(msg.contains("0, 1, 2"), "{msg}");
    }

    #[test]
    fn a_partial_map_is_refused_not_returned_as_ok() {
        // A rebalance leaving one vbucket briefly unowned must not look like
        // success with a smaller vector -- it is a hole the caller cannot see
        // in an `Ok`.
        let vbuuids: HashMap<u16, u64> = [(0u16, 1u64), (2, 3)].into_iter().collect();
        let err = require_complete(3, vbuuids).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("got 2 of 3"), "{msg}");
        assert!(
            msg.contains('1'),
            "the one missing vbucket should be named: {msg}"
        );
    }

    #[test]
    fn missing_vbuckets_lists_the_gaps_in_order() {
        let have: HashMap<u16, u64> = [(0u16, 1u64), (2, 3), (4, 5)].into_iter().collect();
        assert_eq!(missing_vbuckets(5, &have), "1, 3");
    }

    #[test]
    fn missing_vbuckets_is_empty_when_nothing_is_missing() {
        let have: HashMap<u16, u64> = [(0u16, 1u64), (1, 2)].into_iter().collect();
        assert_eq!(missing_vbuckets(2, &have), "");
    }

    #[test]
    fn missing_vbuckets_caps_a_long_list_rather_than_enumerate_it() {
        let have: HashMap<u16, u64> = HashMap::new();
        let msg = missing_vbuckets(25, &have);
        assert!(msg.ends_with("and 5 more"), "{msg}");
        assert_eq!(
            msg.matches(", ").count(),
            20,
            "20 named vbuckets, comma-separated: {msg}"
        );
    }
}
