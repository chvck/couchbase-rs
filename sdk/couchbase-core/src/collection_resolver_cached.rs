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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use tokio::sync::Notify;

use crate::collectionresolver::CollectionResolver;
use crate::error::Error;
use crate::error::Result;

struct CollectionsFastCacheEntry {
    pub collection_id: u32,
    pub manifest_rev: u64,
}

/// The `scope.collection` key the caches are indexed by, built without touching
/// the heap for any name of ordinary length.
///
/// Every resolve needs this key, including the fast-cache hits that are the
/// point of the fast cache — so building it with `String` put allocations on
/// the hottest path in the crate to produce a string that is read once and
/// dropped. The server caps a scope or collection name at 251 bytes, so 256
/// covers most pairs; the `Owned` arm is there so a longer pair stays correct
/// rather than truncated.
#[allow(clippy::large_enum_variant)]
enum FormattedCollectionPath {
    Stack([u8; 256], usize),
    Owned(String),
}

impl FormattedCollectionPath {
    fn new(scope: &str, collection: &str) -> Self {
        let scope_bytes = scope.as_bytes();
        let coll_bytes = collection.as_bytes();
        let total_len = scope_bytes.len() + 1 + coll_bytes.len();

        if total_len <= 256 {
            let mut buf = [0u8; 256];
            buf[..scope_bytes.len()].copy_from_slice(scope_bytes);
            buf[scope_bytes.len()] = b'.';
            buf[scope_bytes.len() + 1..total_len].copy_from_slice(coll_bytes);
            Self::Stack(buf, total_len)
        } else {
            Self::Owned(format!("{scope}.{collection}"))
        }
    }

    fn as_str(&self) -> &str {
        match self {
            // Two valid UTF-8 strings joined by an ASCII '.', so the buffer is
            // valid UTF-8 by construction.
            Self::Stack(buf, len) => std::str::from_utf8(&buf[..*len]).unwrap(),
            Self::Owned(s) => s.as_str(),
        }
    }
}

#[derive(Default)]
struct CollectionsFastManifest {
    pub collections: HashMap<String, CollectionsFastCacheEntry>,
}

#[derive(Clone)]
struct CollectionCacheEntry {
    // TODO: Strongly suspect these should be Option.
    collection_id: u32,
    manifest_rev: u64,

    pending: Option<Arc<Notify>>,
}

type CollectionResolverSlowMap = Arc<Mutex<HashMap<String, Arc<Mutex<CollectionCacheEntry>>>>>;

pub(crate) struct CollectionResolverCached<Resolver: CollectionResolver> {
    resolver: Arc<Resolver>,

    fast_cache: Arc<ArcSwap<CollectionsFastManifest>>,

    slow_map: CollectionResolverSlowMap,
}

#[derive(Clone)]
pub(crate) struct CollectionResolverCachedOptions<Resolver: CollectionResolver> {
    pub resolver: Resolver,
}

impl<Resolver> CollectionResolverCached<Resolver>
where
    Resolver: CollectionResolver + 'static,
{
    pub fn new(opts: CollectionResolverCachedOptions<Resolver>) -> Self {
        Self {
            resolver: Arc::new(opts.resolver),
            fast_cache: Arc::new(ArcSwap::from_pointee(Default::default())),
            slow_map: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn resolve_collection_id_slow(
        &self,
        scope_name: &str,
        collection_name: &str,
        full_key_path: &str,
    ) -> Result<(u32, u64)> {
        loop {
            // This pending logic is a little convoluted but without it the compiler
            // thinks that we are holding the slow_map lock across an await, even
            // if we drop it manually.
            let pending = {
                let mut slow_map = self.slow_map.lock().unwrap();

                if let Some(entry) = slow_map.get(full_key_path) {
                    let entry_guard = entry.lock().unwrap();
                    if let Some(pending) = &entry_guard.pending {
                        Some(pending.clone())
                    } else {
                        return Ok((entry_guard.collection_id, entry_guard.manifest_rev));
                    }
                } else {
                    let entry = Arc::new(Mutex::new(CollectionCacheEntry {
                        collection_id: 0,
                        manifest_rev: 0,
                        pending: Some(Arc::new(Notify::new())),
                    }));

                    slow_map.insert(full_key_path.to_string(), entry);

                    None
                }
            };

            if let Some(p) = pending {
                p.notified().await;

                continue;
            }

            let resolve_resp = {
                let scope_name = scope_name.to_string();
                let collection_name = collection_name.to_string();
                let resolver = self.resolver.clone();
                let handle = tokio::spawn(async move {
                    resolver
                        .resolve_collection_id(&scope_name, &collection_name)
                        .await
                });

                handle.await
            }
            .map_err(|e| {
                Error::new_message_error(format!("failed to join resolve collection id task: {e}"))
            })?;

            return match resolve_resp {
                Ok((collection_id, manifest_rev)) => {
                    let slow_map = self.slow_map.lock().unwrap();
                    let entry = slow_map
                        .get(full_key_path)
                        .expect("slow map was missing collection id entry");

                    let pending = {
                        let mut guard = entry.lock().unwrap();
                        guard.collection_id = collection_id;
                        guard.manifest_rev = manifest_rev;

                        guard.pending.take()
                    };

                    Self::rebuild_fast_cache_locked(&slow_map, self.fast_cache.clone());

                    if let Some(p) = pending {
                        p.notify_waiters();
                    }

                    Ok((collection_id, manifest_rev))
                }
                Err(e) => {
                    let mut slow_map = self.slow_map.lock().unwrap();
                    let entry = slow_map
                        .remove(full_key_path)
                        .expect("slow map was missing collection id entry");

                    let mut guard = entry.lock().unwrap();
                    let pending = guard.pending.take();

                    // No need to rebuild the fast cache as we haven't added this entry to it.

                    if let Some(p) = pending {
                        p.notify_waiters();
                    }

                    Err(e)
                }
            };
        }
    }

    fn rebuild_fast_cache_locked(
        guard: &HashMap<String, Arc<Mutex<CollectionCacheEntry>>>,
        fast_cache: Arc<ArcSwap<CollectionsFastManifest>>,
    ) {
        let mut collections = HashMap::new();
        for (full_key_path, entry) in guard.iter() {
            let (collection_id, manifest_rev) = {
                let guard = entry.lock().unwrap();
                (guard.collection_id, guard.manifest_rev)
            };

            if collection_id > 0 {
                collections.insert(
                    full_key_path.clone(),
                    CollectionsFastCacheEntry {
                        collection_id,
                        manifest_rev,
                    },
                );
            }
        }

        fast_cache.store(Arc::new(CollectionsFastManifest { collections }));
    }
}

impl<Resolver> CollectionResolver for CollectionResolverCached<Resolver>
where
    Resolver: CollectionResolver + 'static,
{
    async fn resolve_collection_id(
        &self,
        scope_name: &str,
        collection_name: &str,
    ) -> Result<(u32, u64)> {
        let full_key_path = FormattedCollectionPath::new(scope_name, collection_name);

        {
            let fast_cache = self.fast_cache.load();
            if let Some(entry) = fast_cache.collections.get(full_key_path.as_str()) {
                return Ok((entry.collection_id, entry.manifest_rev));
            }
        }

        self.resolve_collection_id_slow(scope_name, collection_name, full_key_path.as_str())
            .await
    }

    async fn invalidate_collection_id(
        &self,
        scope_name: &str,
        collection_name: &str,
        manifest_rev: u64,
    ) {
        self.resolver
            .invalidate_collection_id(scope_name, collection_name, manifest_rev)
            .await;

        let full_key_path = FormattedCollectionPath::new(scope_name, collection_name);

        let mut slow_map = self.slow_map.lock().unwrap();

        // A refusal is evidence about the manifest the refusing node was on, not
        // about the manifest we resolved against. If ours is the newer of the
        // two then the id being refused is not the id we hold, and dropping the
        // entry would cost a round trip to re-learn the same id -- and would do
        // it again on the next refusal from the same lagging node. A revision of
        // 0 means the server did not say, so it is not evidence either way and
        // the entry goes.
        if manifest_rev > 0 {
            if let Some(entry) = slow_map.get(full_key_path.as_str()) {
                let entry = entry.lock().unwrap();
                if entry.manifest_rev > manifest_rev {
                    return;
                }
            }
        }

        slow_map.remove(full_key_path.as_str());

        Self::rebuild_fast_cache_locked(&slow_map, self.fast_cache.clone());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use crate::collection_resolver_cached::{
        CollectionResolverCached, CollectionResolverCachedOptions,
    };
    use crate::collectionresolver::CollectionResolver;
    use crate::error::Result;

    /// Answers with one fixed id and revision, and counts how often it is asked.
    /// The count is what says whether the cache kept its entry: a resolve that
    /// reaches here is a resolve the cache could not serve.
    struct CountingResolver {
        resolve_count: Arc<AtomicU32>,
        collection_id: u32,
        manifest_rev: u64,
    }

    impl CollectionResolver for CountingResolver {
        async fn resolve_collection_id(
            &self,
            _scope_name: &str,
            _collection_name: &str,
        ) -> Result<(u32, u64)> {
            self.resolve_count.fetch_add(1, Ordering::SeqCst);

            Ok((self.collection_id, self.manifest_rev))
        }

        async fn invalidate_collection_id(
            &self,
            _scope_name: &str,
            _collection_name: &str,
            _manifest_rev: u64,
        ) {
        }
    }

    /// A cache holding `scope.collection` -> id 9, resolved at manifest 4, and
    /// the counter recording how many times the underlying resolver was used.
    async fn warm_cache_at_rev_4() -> (CollectionResolverCached<CountingResolver>, Arc<AtomicU32>) {
        let resolve_count = Arc::new(AtomicU32::new(0));
        let resolver = CollectionResolverCached::new(CollectionResolverCachedOptions {
            resolver: CountingResolver {
                resolve_count: resolve_count.clone(),
                collection_id: 9,
                manifest_rev: 4,
            },
        });

        let resolved = resolver
            .resolve_collection_id("scope", "collection")
            .await
            .unwrap();
        assert_eq!(resolved, (9, 4));
        assert_eq!(resolve_count.load(Ordering::SeqCst), 1);

        // A second resolve is served from the fast cache, so the count stays at
        // one for as long as the entry survives.
        let resolved = resolver
            .resolve_collection_id("scope", "collection")
            .await
            .unwrap();
        assert_eq!(resolved, (9, 4));
        assert_eq!(resolve_count.load(Ordering::SeqCst), 1);

        (resolver, resolve_count)
    }

    #[tokio::test]
    async fn invalidation_older_than_the_cached_entry_is_declined() {
        let (resolver, resolve_count) = warm_cache_at_rev_4().await;

        // A node still on manifest 2 refused an id we resolved at manifest 4.
        // It cannot have been refusing the id we hold.
        resolver
            .invalidate_collection_id("scope", "collection", 2)
            .await;

        let resolved = resolver
            .resolve_collection_id("scope", "collection")
            .await
            .unwrap();
        assert_eq!(resolved, (9, 4));
        assert_eq!(
            resolve_count.load(Ordering::SeqCst),
            1,
            "an invalidation from an older manifest should have left the entry alone"
        );
    }

    #[tokio::test]
    async fn invalidation_at_the_cached_revision_drops_the_entry() {
        let (resolver, resolve_count) = warm_cache_at_rev_4().await;

        resolver
            .invalidate_collection_id("scope", "collection", 4)
            .await;

        resolver
            .resolve_collection_id("scope", "collection")
            .await
            .unwrap();
        assert_eq!(
            resolve_count.load(Ordering::SeqCst),
            2,
            "a refusal from the manifest we resolved against is about our id"
        );
    }

    #[tokio::test]
    async fn invalidation_newer_than_the_cached_entry_drops_the_entry() {
        let (resolver, resolve_count) = warm_cache_at_rev_4().await;

        resolver
            .invalidate_collection_id("scope", "collection", 5)
            .await;

        resolver
            .resolve_collection_id("scope", "collection")
            .await
            .unwrap();
        assert_eq!(resolve_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn invalidation_without_a_revision_drops_the_entry() {
        let (resolver, resolve_count) = warm_cache_at_rev_4().await;

        // 0 is what a server error carrying no manifest revision arrives as. It
        // is not evidence that our entry is good, so the entry goes.
        resolver
            .invalidate_collection_id("scope", "collection", 0)
            .await;

        resolver
            .resolve_collection_id("scope", "collection")
            .await
            .unwrap();
        assert_eq!(resolve_count.load(Ordering::SeqCst), 2);
    }
}
