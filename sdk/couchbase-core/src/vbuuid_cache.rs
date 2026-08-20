//! The vbucket-UUID map, held once per agent and shared by everything that
//! builds a scan vector.
//!
//! # Why it is cached at all
//!
//! A scan vector is a seqno and a UUID per vbucket. The seqnos move with every
//! write, so they are read per sweep; the UUIDs change only when a vbucket's
//! history forks. They are also the *expensive* half by an order of magnitude —
//! `stats vbucket-seqno` is the only place KV publishes them and it returns
//! every vbucket a node holds, replicas included, across eight text fields each.
//! Measured from the gateway's `benches/collscan.rs`: 2.05 ms against the seqno
//! sweep's 295 µs.
//!
//! # Why it lives here and not in a caller
//!
//! There were two of these, in one process, keyed differently: one for the
//! indexer's `request_plus` vector and one for the KV range-scan path. Two
//! caches of one cluster fact is two chances to be stale, two invalidation
//! rules, and no way for the consumer that *can* detect staleness to help the
//! one that cannot. This is the one, and it belongs beside the calls it wraps.
//!
//! # What may and may not invalidate it
//!
//! **A config revision that has not moved is not evidence the map is good.**
//! `Agent::config_revision` comes from the config watcher, which prefers the
//! HTTP source, while the UUIDs come over memcached — so an unmoved revision
//! says only that ns_server has not told us yet, over a link with its own
//! latency. Using "revision unchanged" as a *validity* proof serves a stale UUID
//! beside a fresh seqno for as long as the two channels disagree.
//!
//! Used the other way it is sound and kept: a revision that **has** moved
//! invalidates, because a fork moves the vbucket map. That direction only ever
//! discards a good map, which costs one read.
//!
//! So three things drop the map, and none of them is a claim that time passing
//! makes it good:
//!
//! 1. **A consumer reporting a refusal** — the exact signal, in band, on the
//!    channel the UUID came from. kv_engine checks the requirement's UUID
//!    against the failover table and fails a range-scan create with
//!    `vbuuid_not_equal`; [`Agent::invalidate_vbuuids`] is how that gets back
//!    here.
//! 2. **The config revision moving**, as above.
//! 3. **A TTL**, and it is a backstop rather than the mechanism.
//!
//! ## The TTL is not redundant, because the consumers are asymmetric
//!
//! Only one consumer can report. A range scan gets `vbuuid_not_equal` promptly
//! and says so. The indexer gets **nothing**: its `AsRecent` skips a vbucket
//! whose request UUID is zero and otherwise requires equality
//! (`common/timestamp.go`), so a stale non-zero UUID never matches, no snapshot
//! is ever consistent, and the scan waits out its budget — eleven minutes — with
//! no error to report until long after the query is lost. A signal that arrives
//! after the thing it would have saved is not a signal.
//!
//! So the TTL bounds how long the indexer can be wrong in a deployment doing no
//! range scans, and the refusal path makes it exact wherever a range scan runs.
//! Sharing the cache is what lets the second cover the first: a KV scan's
//! refusal refreshes the very map the indexer is about to use.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// How long a map may be served without any other reason to trust it.
///
/// Short because the cost of being wrong on the indexer path is a scan that
/// waits eleven minutes, and cheap to hold because what it guards is one stats
/// sweep rather than one per scan.
const TTL: Duration = Duration::from_secs(5);

pub type VbUuidMap = HashMap<u16, u64>;

struct Held {
    /// The config revision this was read under. `None` means the agent had no
    /// config yet, which is not a revision to trust anything against.
    revision: Option<(i64, i64)>,
    read: Instant,
    map: Arc<VbUuidMap>,
}

#[derive(Default)]
pub(crate) struct VbUuidCache {
    held: RwLock<Option<Held>>,
}

impl VbUuidCache {
    /// The map, if one is held that nothing has invalidated.
    pub(crate) fn peek(&self, revision: Option<(i64, i64)>) -> Option<Arc<VbUuidMap>> {
        let held = self.held.read().expect("vbuuid lock");
        let h = held.as_ref()?;
        if h.revision == revision && h.read.elapsed() < TTL {
            return Some(Arc::clone(&h.map));
        }
        None
    }

    pub(crate) fn store(&self, revision: Option<(i64, i64)>, map: Arc<VbUuidMap>) {
        *self.held.write().expect("vbuuid lock") = Some(Held {
            revision,
            read: Instant::now(),
            map,
        });
    }

    /// Drop `stale`, but only if it is still the map being served.
    ///
    /// **The identity check is what makes reporting safe under concurrency.**
    /// Several scans share one map, a fork refuses all of them, and each reports
    /// back. Without the check the first report clears the map, the second
    /// clears the *replacement* somebody already fetched, and one fork produces
    /// refetches for as long as scans keep arriving. With it, every report after
    /// the first is a no-op.
    ///
    /// By pointer rather than by value, because two maps with equal contents are
    /// still different maps: a refetch that happens to return the same UUIDs is
    /// a new map, and a report against the old one must not clear it.
    pub(crate) fn invalidate(&self, stale: &Arc<VbUuidMap>) {
        let mut held = self.held.write().expect("vbuuid lock");
        // Split from an `if let ... && ...` chain: this crate is edition 2021,
        // which does not have let-chains. Same one-pointer-comparison semantics.
        let is_current = held.as_ref().is_some_and(|h| Arc::ptr_eq(&h.map, stale));
        if is_current {
            *held = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(v: u64) -> Arc<VbUuidMap> {
        Arc::new(HashMap::from([(0u16, v)]))
    }

    #[test]
    fn a_moved_revision_is_a_miss() {
        let c = VbUuidCache::default();
        c.store(Some((1, 1)), map(7));
        assert!(c.peek(Some((1, 1))).is_some());
        // A fork moves the vbucket map, so a moved revision discards — the one
        // direction the revision is sound in.
        assert!(c.peek(Some((1, 2))).is_none());
    }

    #[test]
    fn a_late_report_does_not_discard_a_fresh_map() {
        let c = VbUuidCache::default();
        let stale = map(1);
        let fresh = map(2);
        c.store(Some((1, 1)), Arc::clone(&fresh));

        // A second consumer reports the map it was refused on, which is no
        // longer the one being served.
        c.invalidate(&stale);
        assert!(
            c.peek(Some((1, 1))).is_some(),
            "a report against a superseded map must not clear its replacement"
        );

        c.invalidate(&fresh);
        assert!(c.peek(Some((1, 1))).is_none());
    }

    #[test]
    fn identity_is_by_pointer_not_by_value() {
        let c = VbUuidCache::default();
        let first = map(1);
        let second = map(1);
        assert_eq!(first, second);

        c.store(Some((1, 1)), Arc::clone(&second));
        c.invalidate(&first);
        assert!(c.peek(Some((1, 1))).is_some());
    }
}
