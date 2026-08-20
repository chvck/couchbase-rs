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
use crate::mgmtx::node_target::NodeTarget;
use crate::queryx::error;
use crate::queryx::index::Index;
use crate::queryx::query::{normalise_default_name, Query};
use crate::queryx::query_options::{EnsureIndexPollOptions, GetAllIndexesOptions};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct EnsureIndexHelper<'a> {
    pub user_agent: &'a str,
    pub on_behalf_of_info: Option<&'a OnBehalfOfInfo>,

    pub index_name: &'a str,
    pub bucket_name: &'a str,
    pub scope_name: Option<&'a str>,
    pub collection_name: Option<&'a str>,

    confirmed_endpoints: Vec<&'a str>,
}

#[derive(Copy, Debug, Clone, Ord, PartialOrd, Eq, PartialEq)]
#[non_exhaustive]
pub enum DesiredState {
    Created,
    Deleted,
}

impl<'a> EnsureIndexHelper<'a> {
    pub fn new(
        user_agent: &'a str,
        index_name: &'a str,
        bucket_name: &'a str,
        scope_name: Option<&'a str>,
        collection_name: Option<&'a str>,
        on_behalf_of_info: Option<&'a OnBehalfOfInfo>,
    ) -> Self {
        Self {
            user_agent,
            on_behalf_of_info,
            index_name,
            bucket_name,
            scope_name,
            collection_name,
            confirmed_endpoints: vec![],
        }
    }

    async fn poll_one<C: Client>(
        &self,
        client: Arc<C>,
        target: &NodeTarget,
    ) -> error::Result<bool> {
        let resp = Query {
            http_client: client,
            user_agent: self.user_agent.to_string(),
            endpoint: target.endpoint.to_string(),
            canonical_endpoint: target.canonical_endpoint.to_string(),
            auth: target.auth.clone(),
            tracing: Default::default(),
        }
        .get_all_indexes(&GetAllIndexesOptions {
            bucket_name: self.bucket_name,
            scope_name: self.scope_name,
            collection_name: self.collection_name,
            on_behalf_of: self.on_behalf_of_info,
        })
        .await?;

        for index in resp {
            if index.name == self.index_name && self.is_on_target_keyspace(&index) {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Whether `index` sits on the keyspace this helper was asked about.
    ///
    /// `get_all_indexes` is bucket-wide when no scope or collection is given --
    /// its where clause admits `bucket_id={bucket}` as well as the default
    /// collection -- so an index of the same name in another scope of the same
    /// bucket comes back too. Matching on the name alone let such an index stand
    /// in for the one being waited on, and a `Deleted` poll then never ends:
    /// the index it is watching is gone, but the namesake elsewhere is not.
    fn is_on_target_keyspace(&self, index: &Index) -> bool {
        let scope = normalise_default_name(self.scope_name.unwrap_or("_default"));
        let collection = normalise_default_name(self.collection_name.unwrap_or("_default"));

        match (&index.bucket_id, &index.scope_id, &index.keyspace_id) {
            // An index on a named collection names all three.
            (Some(bucket), Some(index_scope), Some(index_collection)) => {
                bucket == self.bucket_name
                    && *index_scope == scope
                    && *index_collection == collection
            }
            // An index on the default collection omits the bucket and the scope,
            // and carries the bucket name in keyspace_id.
            (None, None, Some(keyspace)) => {
                keyspace == self.bucket_name && scope == "_default" && collection == "_default"
            }
            _ => false,
        }
    }

    pub async fn poll<C: Client>(
        &mut self,
        opts: &'a EnsureIndexPollOptions<C>,
    ) -> error::Result<bool> {
        let mut filtered_targets = Vec::with_capacity(opts.targets.len());

        for target in &opts.targets {
            if !self.confirmed_endpoints.contains(&target.endpoint.as_str()) {
                filtered_targets.push(target);
            }
        }

        let mut success_endpoints = Vec::new();
        for target in &opts.targets {
            let exists = self.poll_one(opts.client.clone(), target).await?;

            match opts.desired_state {
                DesiredState::Created => {
                    if exists {
                        success_endpoints.push(target.endpoint.as_str());
                    }
                }
                DesiredState::Deleted => {
                    if !exists {
                        success_endpoints.push(target.endpoint.as_str());
                    }
                }
            }
        }

        self.confirmed_endpoints
            .extend_from_slice(success_endpoints.as_slice());

        Ok(success_endpoints.len() == filtered_targets.len())
    }
}

#[cfg(test)]
mod tests {
    use crate::queryx::ensure_index_helper::EnsureIndexHelper;
    use crate::queryx::index::Index;

    /// The shape `system:indexes` reports for an index on a named collection:
    /// all three of bucket, scope and collection are named.
    fn in_collection(name: &str, bucket: &str, scope: &str, collection: &str) -> Index {
        Index {
            name: name.to_string(),
            using: "gsi".to_string(),
            state: "online".to_string(),
            is_primary: None,
            keyspace_id: Some(collection.to_string()),
            namespace_id: None,
            index_key: None,
            condition: None,
            partition: None,
            scope_id: Some(scope.to_string()),
            bucket_id: Some(bucket.to_string()),
        }
    }

    /// The shape for an index on a bucket's default collection: no bucket_id, no
    /// scope_id, and the bucket name sitting in keyspace_id.
    fn in_default_collection(name: &str, bucket: &str) -> Index {
        Index {
            keyspace_id: Some(bucket.to_string()),
            scope_id: None,
            bucket_id: None,
            ..in_collection(name, bucket, "", "")
        }
    }

    fn helper<'a>(
        bucket: &'a str,
        scope: Option<&'a str>,
        collection: Option<&'a str>,
    ) -> EnsureIndexHelper<'a> {
        EnsureIndexHelper::new("test", "#primary", bucket, scope, collection, None)
    }

    #[test]
    fn an_index_on_the_default_collection_is_matched_when_no_scope_is_given() {
        assert!(helper("default", None, None)
            .is_on_target_keyspace(&in_default_collection("#primary", "default")));
    }

    /// The regression this guards: `get_all_indexes` is bucket-wide when no
    /// scope is given, so a namesake in another scope used to satisfy the poll.
    #[test]
    fn a_namesake_in_another_scope_is_not_matched() {
        assert!(!helper("default", None, None)
            .is_on_target_keyspace(&in_collection("#primary", "default", "bench", "orders")));
    }

    #[test]
    fn an_index_in_another_bucket_is_not_matched() {
        assert!(!helper("default", None, None)
            .is_on_target_keyspace(&in_default_collection("#primary", "travel-sample")));
    }

    #[test]
    fn a_named_collection_matches_only_its_own_keyspace() {
        let h = helper("default", Some("bench"), Some("orders"));

        assert!(h.is_on_target_keyspace(&in_collection("#primary", "default", "bench", "orders")));
        assert!(!h.is_on_target_keyspace(&in_collection("#primary", "default", "bench", "other")));
        assert!(!h.is_on_target_keyspace(&in_default_collection("#primary", "default")));
    }

    /// `system:indexes` reports either all three of bucket, scope and collection
    /// or keyspace alone; anything else is a shape we do not understand, and
    /// guessing that it is the index being waited on is how a `Deleted` poll
    /// ends early against an index that is still there.
    #[test]
    fn a_shape_we_do_not_recognise_is_not_matched() {
        let h = helper("default", None, None);

        let mut half_named = in_collection("#primary", "default", "bench", "orders");
        half_named.scope_id = None;
        assert!(!h.is_on_target_keyspace(&half_named));

        let mut no_keyspace = in_default_collection("#primary", "default");
        no_keyspace.keyspace_id = None;
        assert!(!h.is_on_target_keyspace(&no_keyspace));
    }

    /// An empty name means the default, per `normalise_default_name`.
    #[test]
    fn empty_scope_and_collection_names_mean_the_default_collection() {
        assert!(helper("default", Some(""), Some(""))
            .is_on_target_keyspace(&in_default_collection("#primary", "default")));
    }
}
