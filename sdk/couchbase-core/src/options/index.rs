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

use crate::indexerx::proto::{DataEncoding, Projection};
use crate::indexerx::span::Scan;
use crate::mutationtoken::MutationToken;
use crate::on_behalf_of::OnBehalfOfInfo;

/// How fresh the index must be before it answers.
///
/// The crate-level spelling of [`indexerx::Consistency`](crate::indexerx::Consistency),
/// differing in one place: `AtPlus` takes the mutation tokens a caller already
/// holds instead of a wire vector, because that is what a caller has.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum IndexScanConsistency {
    /// Whatever the indexer has. Fastest, and stale by an unbounded amount.
    Any,
    /// At least as recent as the moment the request arrives, decided by the
    /// indexer, which asks the KV nodes itself. What N1QL calls `request_plus`,
    /// and the default here for the same reason it is `ScanOptions`'s: it is the
    /// end of the trade-off that cannot silently give a wrong answer.
    #[default]
    Session,
    /// At least as recent as these mutations, and no more.
    ///
    /// **Sparse on purpose.** The indexer skips any vbucket the request did not
    /// name, so this waits for exactly the vbuckets the tokens mention and no
    /// others — which is `at_plus` with a mutation state, and is the guarantee
    /// "my own writes are visible" actually needs. It is *not* a cheaper
    /// [`Session`](IndexScanConsistency::Session): a vector that stands in for
    /// `Session` has to name every vbucket in the bucket, and this crate has no
    /// sweep that can build one. See
    /// [`ScanVector`](crate::indexerx::proto::ScanVector).
    ///
    /// Duplicate vbuckets are allowed and the highest sequence number wins,
    /// which is what makes a list of tokens collected from several mutations
    /// usable as it stands.
    AtPlus(Vec<MutationToken>),
}

/// Read an index.
///
/// The bucket is the agent's, so this names the rest of the keyspace. The index
/// is named rather than identified by `defnId`, because the name is what a
/// caller has and the mapping is what the router is for.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct IndexScanOptions<'a> {
    pub scope_name: &'a str,
    pub collection_name: &'a str,
    pub index_name: &'a str,

    pub consistency: IndexScanConsistency,

    /// The ranges to read, as a disjunction — several runs of entries, read as
    /// one stream. Defaults to the whole index.
    pub scans: Vec<Scan>,
    /// Which key columns come back. Defaults to document keys only, which is
    /// what makes an index scan a cheap way to enumerate a keyspace.
    pub projection: Option<Projection>,
    /// Deduplicate on the **index key**, not the document key. An array index
    /// can still return one document several times with this set.
    pub distinct: bool,
    /// Entries to skip. Applied by the indexer **per host**, so on a partitioned
    /// index this skips `offset` entries of each share and not `offset` of the
    /// index — there is no consistent way to apply one offset across a scatter,
    /// because the streams are not ordered against each other.
    pub offset: i64,
    /// A cap on entries, or `None` for the whole index. Applied per host for the
    /// same reason `offset` is.
    pub limit: Option<i64>,
    pub data_encoding: DataEncoding,
    /// A wall-clock budget for the whole scan, or `None` for the indexer's own
    /// `scan_timeout` — 120 seconds by default, and a *total* duration rather
    /// than an idle timeout, so a cursor held open longer than that is killed
    /// whoever was slow.
    pub timeout: Option<Duration>,
    /// Echoed into the indexer's logs and shared by every host of one scattered
    /// scan, which is what makes the pieces correlatable there. Generated when
    /// absent.
    pub request_id: Option<String>,
    pub on_behalf_of: Option<&'a OnBehalfOfInfo>,
}

impl<'a> IndexScanOptions<'a> {
    pub fn new(
        scope_name: &'a str,
        collection_name: &'a str,
        index_name: &'a str,
    ) -> IndexScanOptions<'a> {
        IndexScanOptions {
            scope_name,
            collection_name,
            index_name,
            consistency: IndexScanConsistency::default(),
            scans: vec![Scan::all()],
            projection: Some(Projection::keys_only()),
            distinct: false,
            offset: 0,
            limit: None,
            data_encoding: DataEncoding::Json,
            timeout: None,
            request_id: None,
            on_behalf_of: None,
        }
    }

    pub fn consistency(mut self, consistency: IndexScanConsistency) -> Self {
        self.consistency = consistency;
        self
    }

    pub fn scans(mut self, scans: Vec<Scan>) -> Self {
        self.scans = scans;
        self
    }

    pub fn projection(mut self, projection: impl Into<Option<Projection>>) -> Self {
        self.projection = projection.into();
        self
    }

    pub fn distinct(mut self, distinct: bool) -> Self {
        self.distinct = distinct;
        self
    }

    pub fn offset(mut self, offset: i64) -> Self {
        self.offset = offset;
        self
    }

    pub fn limit(mut self, limit: impl Into<Option<i64>>) -> Self {
        self.limit = limit.into();
        self
    }

    pub fn data_encoding(mut self, data_encoding: DataEncoding) -> Self {
        self.data_encoding = data_encoding;
        self
    }

    pub fn timeout(mut self, timeout: impl Into<Option<Duration>>) -> Self {
        self.timeout = timeout.into();
        self
    }

    pub fn request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    pub fn on_behalf_of(mut self, on_behalf_of: impl Into<Option<&'a OnBehalfOfInfo>>) -> Self {
        self.on_behalf_of = on_behalf_of.into();
        self
    }
}
