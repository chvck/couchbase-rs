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

//! Which indexer serves a scan, and whether the answer is the whole index.
//!
//! The counterpart of `vbucketrouter` for the indexing
//! service, and the same kind of thing: a routing table with no I/O in it. It
//! decides, `indexcomponent` acts, and
//! [`indexerx`](crate::indexerx) speaks the protocol — the split `memdx`,
//! `vbucketrouter` and `crudcomponent` already have.
//!
//! ### The two translations it exists for
//!
//! **Names to definition ids.** A scan addresses a `defnId`; the queryport wire
//! format has no index names in it at all. The indexing service's
//! `/getIndexStatus` — [`indexerx::status`](crate::indexerx::status), and **not**
//! the mgmt service's `/indexStatus`, which is a different endpoint reached
//! through [`mgmtx`](crate::mgmtx) and says nothing about placement — is where
//! the mapping comes from.
//!
//! **Mgmt ports to scan ports.** `/getIndexStatus` reports `hosts` and
//! `partitionMap` keyed by the node's *mgmt* port — `:8091`, measured on 8.0.3,
//! and not the index HTTP port the Go type's comment claims. Scanning needs the
//! queryport, so the router joins the two against the cluster config, which
//! knows both.
//!
//! ### Scope, and where the scatter stops
//!
//! `IndexRouter::routes` resolves a scan to **one route per host** holding a
//! piece of the index. A non-partitioned index is the degenerate case of one.
//!
//! **Merging those streams back into index order is deliberately not here, and
//! not anywhere in this crate.** Entry keys arrive as JSON under
//! [`DataEncoding::Json`](crate::indexerx::DataEncoding), and JSON text does not
//! compare in index order — `[10]` sorts before `[9]` as text. Ordering them
//! means decoding each key to a value and comparing it under the caller's
//! collation, which is the caller's knowledge and not this crate's. See
//! `crate::results::index_scan` for what that costs a caller and what it buys.
//!
//! What is here is what only routing can answer: which hosts, which partitions
//! each holds, whether they add up to the whole index, replica choice, and
//! whether a failure is one a fresher topology could fix.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::address::Address;
use crate::indexerx::status::IndexStatus;
use crate::parsedconfig::ParsedConfig;

/// Where each node's queryport is, keyed the way `/getIndexStatus` names nodes.
///
/// A plain map rather than a handle on the config watcher, so the router is
/// testable without a cluster and so one refresh cannot mix a node map from one
/// moment with an index list from another.
///
/// **Keyed by `host:mgmtPort`**, because that is how `/getIndexStatus` names a
/// node, and the host is the one the *cluster* knows — the canonical address,
/// not the alternate-network one. The indexer answers out of its own view of the
/// cluster, so an external-network client still has to join on the internal
/// name; the value is the address to dial, which is the network-specific one.
/// (Measured on a cluster with no alternate addresses, where the two are the
/// same. The canonical key is reasoning, not measurement.)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeMap {
    scan_addresses: HashMap<String, Address>,
}

impl NodeMap {
    /// Build from `(cluster_name, mgmt_port, host_to_dial, scan_port)` — which
    /// is what a [`NetworkConfigNode`](crate::parsedconfig::NetworkConfigNode)
    /// yields, taking the first two from its canonical info.
    pub(crate) fn from_nodes(
        nodes: impl IntoIterator<Item = (String, Option<u16>, String, Option<u16>)>,
    ) -> NodeMap {
        let mut scan_addresses = HashMap::new();
        for (cluster_name, mgmt, host, scan) in nodes {
            // A node with no index service has no queryport. Recording one
            // would produce an address that fails to connect rather than a node
            // that is absent, and "absent" is the truth.
            if let (Some(mgmt), Some(port)) = (mgmt, scan) {
                scan_addresses.insert(format!("{cluster_name}:{mgmt}"), Address { host, port });
            }
        }
        NodeMap { scan_addresses }
    }

    /// The join `/getIndexStatus` forces, done once against one config snapshot.
    ///
    /// **The transport is not decided here.** `index_scan` is the same number in
    /// both port sets — the server advertises `indexScan` and no `indexScanSSL`,
    /// so a TLS scan is that port with TLS on top — which is why this reads
    /// either and whether to wrap the socket is settled by whether the agent
    /// holds a [`TlsConfig`](crate::tls_config::TlsConfig).
    pub(crate) fn from_config(config: &ParsedConfig, network_type: &str) -> NodeMap {
        let network = config.addresses_group_for_network_type(network_type);
        NodeMap::from_nodes(network.nodes.into_iter().map(|node| {
            (
                node.canonical_node_info.hostname,
                node.canonical_node_info.non_ssl_ports.mgmt,
                node.hostname,
                node.non_ssl_ports.index_scan.or(node.ssl_ports.index_scan),
            )
        }))
    }

    /// The queryport of a node named the way `/getIndexStatus` names it.
    pub fn scan_address(&self, mgmt_host: &str) -> Option<&Address> {
        self.scan_addresses.get(mgmt_host)
    }

    pub fn is_empty(&self) -> bool {
        self.scan_addresses.is_empty()
    }

    pub fn len(&self) -> usize {
        self.scan_addresses.len()
    }
}

/// A snapshot of the cluster's indexes, and the node map they are joined
/// against.
#[derive(Debug, Default)]
struct Topology {
    indexes: Vec<IndexStatus>,
    nodes: NodeMap,
    /// Whether the index list can be trusted. False until the first
    /// `/getIndexStatus`, and false again when the cluster config moved the
    /// nodes under it — see [`IndexRouter::set_nodes`].
    current: bool,
}

impl Topology {
    /// Every instance of a named index that could serve a scan right now.
    ///
    /// Instances that are not `Ready` are dropped here rather than at the
    /// server, which turns a round trip and an "index not ready" into a filter.
    fn ready_instances<'a>(&'a self, index: &IndexRef) -> Vec<&'a IndexStatus> {
        self.indexes
            .iter()
            .filter(|s| {
                s.is_ready()
                    && s.index_name == index.name
                    && s.keyspace()
                        == (
                            index.bucket.as_str(),
                            index.scope.as_str(),
                            index.collection.as_str(),
                        )
            })
            .collect()
    }
}

/// What a scan needs to be routed: the index by name, in its keyspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRef {
    pub bucket: String,
    pub scope: String,
    pub collection: String,
    pub name: String,
}

impl IndexRef {
    pub fn new(
        bucket: impl Into<String>,
        scope: impl Into<String>,
        collection: impl Into<String>,
        name: impl Into<String>,
    ) -> IndexRef {
        IndexRef {
            bucket: bucket.into(),
            scope: scope.into(),
            collection: collection.into(),
            name: name.into(),
        }
    }
}

impl std::fmt::Display for IndexRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} on {}.{}.{}",
            self.name, self.bucket, self.scope, self.collection
        )
    }
}

/// A routing decision: which instance, on which queryport, holding which
/// partitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// What the scan request addresses. The wire has no index names.
    pub defn_id: u64,
    /// Which copy of the index this is a piece of. Every route of one scan
    /// shares it — that is what makes them one copy rather than a mixture.
    pub inst_id: u64,
    pub replica_id: u32,
    pub scan_address: Address,
    /// Whether the index is partitioned at all.
    ///
    /// Carried because it changes how a scan request is *addressed*, not only
    /// what it covers: a partitioned index is asked for partitions by number,
    /// and an unpartitioned one is asked for nothing in particular. Its single
    /// partition is numbered 0, which is not a number the server expects to be
    /// given.
    pub partitioned: bool,
    /// The partitions **this host** holds. `[0]` for an index that is not
    /// partitioned, which is the server's numbering and is preserved rather than
    /// normalised.
    pub partitions: Vec<u64>,
}

/// Why a scan could not be routed.
///
/// Every variant is a statement about the *topology snapshot*, which is what
/// [`RouteError::worth_refreshing`] is able to answer from the type alone.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RouteError {
    /// No instance of that name in that keyspace, ready or otherwise.
    IndexNotFound(IndexRef),
    /// Instances exist but none is `Ready`.
    IndexNotReady(IndexRef),
    /// The partitions the chosen replica reports do not add up to the index.
    ///
    /// A scan addresses partitions by number, so a partition no host claims is
    /// one nobody reads — and the answer comes back short by whatever it held,
    /// with no error anywhere. Refusing is the only honest response, and a
    /// rebalance in flight is the ordinary cause, which is why it is worth a
    /// refresh.
    PartitionsMissing {
        index: IndexRef,
        held: usize,
        want: usize,
    },
    /// Two hosts claim the same partition.
    ///
    /// Not a short read, and not a coherent copy either: reading both would
    /// return every entry in the shared partition twice. Its own variant rather
    /// than a `PartitionsMissing` with an invented total, because the two are
    /// caught by different arithmetic and a reader deserves to know which.
    PartitionClaimedTwice { index: IndexRef, partition: u64 },
    /// `/getIndexStatus` named a node the cluster config does not know about,
    /// which means the two are out of step.
    UnknownNode(String),
    /// A scan was routed before any `/getIndexStatus` succeeded, or the config
    /// moved the nodes and the refresh that follows has not happened yet.
    NoTopology,
}

impl RouteError {
    /// Whether a fresher topology could plausibly turn this into a route.
    ///
    /// **Typed, and that is the point.** Every variant here is a disagreement
    /// between what this client last read and what the cluster now is, so the
    /// honest answer is yes for all of them — including `IndexNotFound`, which
    /// from a *stale snapshot* means "not in the list I hold", where the same
    /// words from the *server* ([`ServerError::IndexNotFound`]) mean "I looked
    /// up your defnId and it is gone". The two are opposite answers to
    /// "should I retry", which is why they must not share a spelling.
    ///
    /// [`ServerError::IndexNotFound`]: crate::indexerx::error::ServerError::IndexNotFound
    pub fn worth_refreshing(&self) -> bool {
        match self {
            RouteError::IndexNotFound(_)
            | RouteError::IndexNotReady(_)
            | RouteError::PartitionsMissing { .. }
            | RouteError::PartitionClaimedTwice { .. }
            | RouteError::UnknownNode(_)
            | RouteError::NoTopology => true,
        }
    }
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteError::IndexNotFound(r) => write!(f, "no index {r}"),
            RouteError::IndexNotReady(r) => {
                write!(f, "index {r} exists but no instance is ready")
            }
            RouteError::PartitionsMissing { index, held, want } => write!(
                f,
                "index {index} reports {held} of its {want} partitions; the rest are on no host this scan could reach"
            ),
            RouteError::PartitionClaimedTwice { index, partition } => write!(
                f,
                "index {index} has partition {partition} on two hosts, so a scan of it would read that partition twice"
            ),
            RouteError::UnknownNode(host) => {
                write!(f, "node {host} is not in the cluster config")
            }
            RouteError::NoTopology => {
                write!(f, "no index topology has been read yet")
            }
        }
    }
}

impl std::error::Error for RouteError {}

impl From<RouteError> for crate::error::Error {
    fn from(e: RouteError) -> Self {
        crate::error::ErrorKind::IndexRouting(e).into()
    }
}

/// The index topology, and the routing decisions that can be taken from it.
///
/// No I/O: the snapshot is pushed in by `indexcomponent`, which owns the HTTP
/// call and the endpoint selection, exactly as `vbucketrouter` is pushed a
/// vbucket map by the agent rather than fetching one.
pub(crate) struct IndexRouter {
    topology: ArcSwap<Topology>,
    /// Bumped by every [`IndexRouter::set_topology`]. What a caller compares
    /// across an await to ask "did somebody else already do this refresh?"
    /// without holding a lock over the fetch.
    generation: AtomicUsize,
    /// Round-robin cursor over replicas. Load-aware selection — the Go client's
    /// pending-item counts and response timings — is a later optimization; this
    /// is the version whose behaviour can be stated in one sentence.
    next_replica: AtomicUsize,
}

impl IndexRouter {
    pub fn new(nodes: NodeMap) -> IndexRouter {
        IndexRouter {
            topology: ArcSwap::from_pointee(Topology {
                indexes: Vec::new(),
                nodes,
                current: false,
            }),
            generation: AtomicUsize::new(0),
            next_replica: AtomicUsize::new(0),
        }
    }

    /// Install a fresh `/getIndexStatus` result against the node map it was
    /// read beside.
    pub fn set_topology(&self, indexes: Vec<IndexStatus>, nodes: NodeMap) {
        self.topology.store(Arc::new(Topology {
            indexes,
            nodes,
            current: true,
        }));
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// How many times the index list has been replaced. Only useful compared
    /// against itself.
    pub fn generation(&self) -> usize {
        self.generation.load(Ordering::Relaxed)
    }

    /// Take a new node map from a cluster config, and say whether the index
    /// list survived it.
    ///
    /// **The invalidation is what wires this to the config watcher.** A config
    /// whose node set changed is a rebalance, a failover or a node added — the
    /// three things that move partitions — so the index list read before it
    /// cannot be trusted. Marking it stale rather than fetching here keeps HTTP
    /// off `apply_config`'s path, which every component's `reconfigure` shares;
    /// the next scan pays for the refresh, and a config change that touched no
    /// index node costs nothing at all.
    pub fn set_nodes(&self, nodes: NodeMap) {
        let previous = self.topology.load();
        if previous.nodes == nodes {
            return;
        }

        self.topology.store(Arc::new(Topology {
            indexes: previous.indexes.clone(),
            nodes,
            current: false,
        }));
    }

    /// Whether the next [`IndexRouter::routes`] needs a `/getIndexStatus`
    /// first.
    pub fn needs_refresh(&self) -> bool {
        !self.topology.load().current
    }

    /// Every route one scan of this index needs — **one per host holding a
    /// piece of it**, and exactly one for an index that is not partitioned.
    ///
    /// **A partitioned index is several status rows, not one**, and this is the
    /// measurement the whole function is shaped by. On 8.0 a four-partition
    /// index over three nodes comes back as *three* rows: same `defnId`, same
    /// `instId`, each naming one host and only that host's partitions, each
    /// reporting its own share in the field called `numPartition`. So a route is
    /// a row, an instance is the rows that share an `instId`, and taking one row
    /// for the index would read a fraction of it and call it whole.
    ///
    /// A replica is chosen first and the scatter is over *its* rows, because a
    /// replica is a whole copy: choosing per partition across replicas would
    /// spread load further but can also assemble a set of partitions that no
    /// single copy ever held.
    ///
    /// The routes come back in partition order rather than in map order, so that
    /// two scans of one index scatter the same way — which is what makes an
    /// unordered read of a partitioned index reproducible enough to assert on.
    pub fn routes(&self, index: &IndexRef) -> Result<Vec<Route>, RouteError> {
        let topology = self.topology.load();
        let ready = topology.ready_instances(index);

        if ready.is_empty() {
            if !topology.current {
                return Err(RouteError::NoTopology);
            }
            // Distinguishing these two costs one more pass and is the difference
            // between "your index is building" and "you named something that
            // does not exist".
            let exists = topology.indexes.iter().any(|s| {
                s.index_name == index.name
                    && s.keyspace()
                        == (
                            index.bucket.as_str(),
                            index.scope.as_str(),
                            index.collection.as_str(),
                        )
            });
            return Err(if exists {
                RouteError::IndexNotReady(index.clone())
            } else {
                RouteError::IndexNotFound(index.clone())
            });
        }

        // One copy of the index is the rows that share an instance id, in the
        // order the endpoint listed them so that the rotation below is stable.
        let mut instances: Vec<Vec<&IndexStatus>> = Vec::new();
        for row in ready {
            match instances
                .iter_mut()
                .find(|rows| rows[0].inst_id == row.inst_id)
            {
                Some(rows) => rows.push(row),
                None => instances.push(vec![row]),
            }
        }

        // Round-robin across replicas. `fetch_add` wrapping is fine — the
        // modulus is what matters and the counter is only a rotation.
        let choice = self.next_replica.fetch_add(1, Ordering::Relaxed) % instances.len();
        let instance = &instances[choice];

        let mut routes = Vec::new();
        for row in instance {
            for (mgmt_host, partitions) in &row.partition_map {
                let scan_address = topology
                    .nodes
                    .scan_address(mgmt_host)
                    .ok_or_else(|| RouteError::UnknownNode(mgmt_host.clone()))?
                    .clone();

                routes.push(Route {
                    defn_id: row.defn_id,
                    inst_id: row.inst_id,
                    replica_id: row.replica_id,
                    scan_address,
                    partitioned: row.partitioned,
                    partitions: partitions.clone(),
                });
            }
        }

        if routes.is_empty() {
            // A `Ready` instance with an empty `partitionMap`. The server does
            // that between placing an index and publishing where it went.
            return Err(RouteError::IndexNotReady(index.clone()));
        }

        check_coverage(index, instance, &routes)?;

        routes.sort_by(|a, b| a.partitions.cmp(&b.partitions));
        Ok(routes)
    }
}

/// Whether these routes are the whole index, exactly once.
///
/// **The guard a scatter needs and a single host did not.** Partitions are
/// addressed by number, so one that no route claims is one nobody asks for, and
/// the scan comes back short by whatever it held with nothing anywhere to say
/// so.
///
/// It cannot be checked by summing `num_partition`, because that field is per
/// row: the sum over the rows that *survived* is the number a scan that lost a
/// host would also compute, so it agrees with itself no matter what is missing.
/// The three independent facts are:
///
/// - **No partition claimed twice**, which is arithmetic on the routes alone.
/// - **What the DDL asked for**, which [`IndexStatus::declared_partitions`]
///   reads out of the echoed `WITH` clause. Authoritative when present, and
///   absent when the index was created without naming a count.
/// - **The numbering.** A partitioned index numbers its partitions from 1, so a
///   complete set is exactly `1..=n` — distinct, and with no gaps. That catches
///   any missing partition but the highest, which is why it is the fallback and
///   not the rule.
fn check_coverage(
    index: &IndexRef,
    instance: &[&IndexStatus],
    routes: &[Route],
) -> Result<(), RouteError> {
    let mut partitions: Vec<u64> = routes.iter().flat_map(|r| r.partitions.clone()).collect();
    partitions.sort_unstable();

    if let Some(dup) = partitions.windows(2).find(|w| w[0] == w[1]) {
        return Err(RouteError::PartitionClaimedTwice {
            index: index.clone(),
            partition: dup[0],
        });
    }

    let held = partitions.len();
    let short = |want: usize| {
        Err(RouteError::PartitionsMissing {
            index: index.clone(),
            held,
            want,
        })
    };

    if !instance.iter().any(|row| row.partitioned) {
        // The unpartitioned case, and it is not a special case of the numbering
        // rule: the single partition is numbered 0 rather than 1.
        return if partitions.as_slice() == [0] {
            Ok(())
        } else {
            short(1)
        };
    }

    let want = instance
        .iter()
        .find_map(|row| row.declared_partitions())
        .map_or_else(
            || partitions.last().copied().unwrap_or(0) as usize,
            |n| n as usize,
        );

    if partitions == (1..=want as u64).collect::<Vec<_>>() {
        Ok(())
    } else {
        short(want)
    }
}

/// Whether a failed scan is worth one topology refresh and one more attempt.
///
/// **Typed all the way down, and that is the change from cbcore-rs**, where this
/// substring-matched its own `Display` output — so a reworded error message
/// silently turned a retry into a failure, and the two meanings of "index not
/// found" could not be told apart at all. Here each taxonomy answers for itself:
/// [`RouteError::worth_refreshing`] for a disagreement with the topology
/// snapshot, [`ServerError::is_retryable_after_refresh`] for what the indexer
/// said. Nothing else is retried.
///
/// [`ServerError::is_retryable_after_refresh`]: crate::indexerx::error::ServerError::is_retryable_after_refresh
pub(crate) fn worth_refreshing(error: &crate::error::Error) -> bool {
    match error.kind() {
        crate::error::ErrorKind::IndexRouting(e) => e.worth_refreshing(),
        crate::error::ErrorKind::Indexer(e) => match e.kind() {
            crate::indexerx::error::ErrorKind::Server(e) => e.is_retryable_after_refresh(),
            _ => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexerx::error::{Error as IndexerError, ServerError};

    /// One status row: one host, and only the partitions it holds.
    ///
    /// **That is the shape the endpoint really returns**, measured on 8.0 — a
    /// partitioned index is several of these sharing a `defnId` and an `instId`,
    /// not one row with a map of every host. `numPartition` is this row's own
    /// share, which is what makes it useless as a total.
    fn row(
        name: &str,
        defn_id: u64,
        replica: u32,
        host: &str,
        partitions: &[u64],
        partitioned: bool,
    ) -> IndexStatus {
        let json = serde_json::json!({
            "defnId": defn_id,
            "instId": defn_id + u64::from(replica),
            "indexName": name,
            "bucket": "default",
            "scope": "_default",
            "collection": "things",
            "status": "Ready",
            "replicaId": replica,
            "partitioned": partitioned,
            "numPartition": partitions.len(),
            "partitionMap": { host: partitions.to_vec() },
        });
        serde_json::from_value(json).expect("a status row")
    }

    /// A whole index on one host, which is every index that is not partitioned.
    fn status(name: &str, defn_id: u64, replica: u32, host: &str) -> IndexStatus {
        row(name, defn_id, replica, host, &[0], false)
    }

    /// A partitioned index, as its rows: `(host, its partitions)`, plus what the
    /// DDL declared.
    fn partitioned(
        name: &str,
        defn_id: u64,
        declared: Option<u32>,
        hosts: &[(&str, &[u64])],
    ) -> Vec<IndexStatus> {
        hosts
            .iter()
            .map(|(host, parts)| {
                let mut s = row(name, defn_id, 0, host, parts, true);
                s.definition = match declared {
                    Some(n) => format!(
                        "CREATE INDEX `{name}` ON `default`.`_default`.`things`(`n`) \
                         PARTITION BY hash((meta().`id`)) WITH {{  \"num_partition\":{n} }}"
                    ),
                    None => format!(
                        "CREATE INDEX `{name}` ON `default`.`_default`.`things`(`n`) \
                         PARTITION BY hash((meta().`id`))"
                    ),
                };
                s
            })
            .collect()
    }

    fn building(name: &str, defn_id: u64) -> IndexStatus {
        let mut s = status(name, defn_id, 0, "10.0.0.1:8091");
        s.status = "Building".to_string();
        s
    }

    fn nodes() -> NodeMap {
        NodeMap::from_nodes([
            (
                "10.0.0.1".to_string(),
                Some(8091),
                "10.0.0.1".to_string(),
                Some(9101),
            ),
            (
                "10.0.0.2".to_string(),
                Some(8091),
                "10.0.0.2".to_string(),
                Some(9101),
            ),
        ])
    }

    fn scan_at(host: &str) -> Address {
        Address {
            host: host.to_string(),
            port: 9101,
        }
    }

    fn router(indexes: Vec<IndexStatus>) -> IndexRouter {
        let router = IndexRouter::new(nodes());
        router.set_topology(indexes, nodes());
        router
    }

    fn a_ref(name: &str) -> IndexRef {
        IndexRef::new("default", "_default", "things", name)
    }

    #[test]
    fn a_node_map_translates_mgmt_to_the_scan_port() {
        // The join `/getIndexStatus` forces: it names nodes by mgmt port and
        // scanning needs the queryport.
        assert_eq!(
            nodes().scan_address("10.0.0.1:8091"),
            Some(&scan_at("10.0.0.1"))
        );
        assert_eq!(nodes().scan_address("10.0.0.9:8091"), None);
    }

    #[test]
    fn a_node_without_the_index_service_is_absent_rather_than_dialled() {
        // `Option` rather than a zero sentinel, so this is a node that is not
        // there rather than one at port zero.
        let map = NodeMap::from_nodes([(
            "10.0.0.3".to_string(),
            Some(8091),
            "10.0.0.3".to_string(),
            None,
        )]);
        assert!(map.is_empty());
        assert_eq!(map.scan_address("10.0.0.3:8091"), None);
    }

    #[test]
    fn a_node_map_is_built_from_a_real_cluster_config() {
        // The end-to-end shape of the join, against the 8.0.3 capture in
        // `configparser`: nodesExt in, mgmt-to-queryport out, with the
        // search-only node simply absent.
        let config = crate::configparser::tests::parse_sample();
        let map = NodeMap::from_config(&config, "default");

        assert_eq!(map.len(), 1, "one of the two nodes runs an indexer");
        assert_eq!(
            map.scan_address("192.168.107.128:8091"),
            Some(&scan_at("192.168.107.128"))
        );
        assert_eq!(map.scan_address("192.168.107.131:8091"), None);
    }

    #[test]
    fn the_scan_port_is_the_same_in_both_sets_so_tls_is_not_read_off_it() {
        // The trap `from_config` is written around: the server advertises no
        // `indexScanSSL`, so choosing the transport by which port set answered
        // would be choosing it by a coin that always lands the same way.
        let config = crate::configparser::tests::parse_sample();
        let node = &config.addresses_group_for_network_type("default").nodes[0];

        assert_eq!(node.non_ssl_ports.index_scan, node.ssl_ports.index_scan);
        assert_eq!(node.non_ssl_ports.index_scan, Some(9101));
    }

    /// The one route a non-partitioned index has.
    fn one(router: &IndexRouter, name: &str) -> Result<Route, RouteError> {
        let mut routes = router.routes(&a_ref(name))?;
        assert_eq!(routes.len(), 1, "this index is on one host");
        Ok(routes.remove(0))
    }

    #[test]
    fn routing_finds_the_instance_and_its_queryport() {
        let router = router(vec![status("ix_a", 7, 0, "10.0.0.2:8091")]);

        let route = one(&router, "ix_a").expect("a route");
        assert_eq!(route.defn_id, 7);
        assert_eq!(route.scan_address, scan_at("10.0.0.2"));
        assert_eq!(route.partitions, vec![0]);
    }

    #[test]
    fn replicas_are_used_in_turn() {
        let router = router(vec![
            status("ix_a", 7, 0, "10.0.0.1:8091"),
            status("ix_a", 7, 1, "10.0.0.2:8091"),
        ]);

        let mut seen: Vec<Address> = (0..4)
            .map(|_| one(&router, "ix_a").expect("a route").scan_address)
            .collect();
        seen.sort_by_key(|a| a.to_string());
        seen.dedup();

        assert_eq!(
            seen.len(),
            2,
            "four routes over two replicas should use both"
        );
    }

    #[test]
    fn a_building_index_is_not_ready_rather_than_missing() {
        // The distinction is the difference between "wait" and "you asked for
        // something that does not exist", and a caller acts differently on each.
        let router = router(vec![building("ix_a", 7)]);

        assert_eq!(
            router.routes(&a_ref("ix_a")),
            Err(RouteError::IndexNotReady(a_ref("ix_a")))
        );
    }

    #[test]
    fn an_unknown_index_is_reported_as_missing() {
        let router = router(vec![status("ix_a", 7, 0, "10.0.0.1:8091")]);

        assert_eq!(
            router.routes(&a_ref("ix_b")),
            Err(RouteError::IndexNotFound(a_ref("ix_b")))
        );
    }

    #[test]
    fn the_same_name_in_another_keyspace_is_a_different_index() {
        // Index names are unique per collection, not per cluster, so matching on
        // the name alone would route a scan at someone else's data.
        let router = router(vec![status("ix_a", 7, 0, "10.0.0.1:8091")]);

        let elsewhere = IndexRef::new("default", "_default", "others", "ix_a");
        assert_eq!(
            router.routes(&elsewhere),
            Err(RouteError::IndexNotFound(elsewhere))
        );
    }

    #[test]
    fn a_partitioned_index_routes_to_every_host_that_holds_part_of_it() {
        // Reading one host's partitions and calling it the index is a silently
        // short answer, which is the one failure worth being loud about.
        let router = router(partitioned(
            "ix_a",
            7,
            Some(4),
            &[("10.0.0.2:8091", &[3, 4]), ("10.0.0.1:8091", &[1, 2])],
        ));

        let routes = router.routes(&a_ref("ix_a")).expect("routes");

        // In partition order rather than the map's, so a scatter is reproducible
        // across two scans of the same index.
        assert_eq!(
            routes
                .iter()
                .map(|r| (r.scan_address.clone(), r.partitions.as_slice()))
                .collect::<Vec<_>>(),
            [
                (scan_at("10.0.0.1"), [1, 2].as_slice()),
                (scan_at("10.0.0.2"), [3, 4].as_slice()),
            ]
        );
        assert!(
            routes.iter().all(|r| r.inst_id == routes[0].inst_id),
            "one copy of the index, not a mixture of replicas"
        );
    }

    #[test]
    fn a_host_that_dropped_out_is_refused_rather_than_read_around() {
        // The failure this whole phase is written around: partitions are
        // addressed by number, so one that is in nobody's map is one the scan
        // never asks for, and the result is short by whatever it held with
        // nothing to say so. One of two hosts here, and the DDL is what says
        // there should be four partitions.
        let router = router(partitioned(
            "ix_a",
            7,
            Some(4),
            &[("10.0.0.1:8091", &[1, 2])],
        ));

        assert_eq!(
            router.routes(&a_ref("ix_a")),
            Err(RouteError::PartitionsMissing {
                index: a_ref("ix_a"),
                held: 2,
                want: 4,
            })
        );
    }

    #[test]
    fn summing_the_rows_is_not_the_check_and_this_is_why() {
        // `numPartition` is the row's own share — measured on 8.0 — so a scan
        // that lost a host sums the survivors and gets exactly what it expects.
        // The rows here agree with themselves perfectly and are still half an
        // index.
        let rows = partitioned("ix_a", 7, Some(4), &[("10.0.0.1:8091", &[1, 2])]);
        assert_eq!(
            rows.iter().map(|r| r.num_partition).sum::<u32>(),
            2,
            "the sum over surviving rows is not the index's partition count"
        );
        assert_eq!(rows[0].declared_partitions(), Some(4));
    }

    #[test]
    fn a_gap_in_the_numbering_is_caught_without_any_declaration() {
        // The fallback, for an index whose DDL named no count: partitions are
        // numbered from 1, so a complete set is exactly `1..=n`.
        let router = router(partitioned(
            "ix_a",
            7,
            None,
            &[("10.0.0.1:8091", &[1, 2]), ("10.0.0.2:8091", &[4])],
        ));

        assert_eq!(
            router.routes(&a_ref("ix_a")),
            Err(RouteError::PartitionsMissing {
                index: a_ref("ix_a"),
                held: 3,
                want: 4,
            })
        );
    }

    #[test]
    fn the_right_number_of_the_wrong_partitions_is_still_refused() {
        // Counting is not the check: these rows hold four partitions of a
        // four-partition index and are still not it. A map read mid-rebalance
        // can name a partition that has moved on beside one that has not
        // arrived, and a scan of that is short by one — which counting cannot
        // see and the numbering can.
        let router = router(partitioned(
            "ix_a",
            7,
            Some(4),
            &[("10.0.0.1:8091", &[1, 2]), ("10.0.0.2:8091", &[4, 5])],
        ));

        assert_eq!(
            router.routes(&a_ref("ix_a")),
            Err(RouteError::PartitionsMissing {
                index: a_ref("ix_a"),
                held: 4,
                want: 4,
            })
        );
    }

    #[test]
    fn a_complete_set_with_no_declaration_is_routed() {
        // The other half of the fallback: `1..=n` with no gaps is complete, and
        // an index created without a `num_partition` clause is the ordinary way
        // to get there.
        let router = router(partitioned(
            "ix_a",
            7,
            None,
            &[("10.0.0.1:8091", &[1, 2]), ("10.0.0.2:8091", &[3])],
        ));

        assert_eq!(router.routes(&a_ref("ix_a")).expect("routes").len(), 2);
    }

    #[test]
    fn two_hosts_claiming_one_partition_is_refused_rather_than_read_twice() {
        // Not a short read, but not a copy of the index either — every entry in
        // the shared partition would come back twice.
        let router = router(partitioned(
            "ix_a",
            7,
            Some(2),
            &[("10.0.0.1:8091", &[1, 2]), ("10.0.0.2:8091", &[2])],
        ));

        assert_eq!(
            router.routes(&a_ref("ix_a")),
            Err(RouteError::PartitionClaimedTwice {
                index: a_ref("ix_a"),
                partition: 2,
            })
        );
    }

    #[test]
    fn a_node_the_cluster_config_does_not_know_is_named() {
        let router = router(vec![status("ix_a", 7, 0, "10.9.9.9:8091")]);

        assert_eq!(
            router.routes(&a_ref("ix_a")),
            Err(RouteError::UnknownNode("10.9.9.9:8091".to_string()))
        );
    }

    #[test]
    fn a_stale_snapshot_is_not_an_index_that_does_not_exist() {
        // Before the first `/getIndexStatus`, "I have no row for that" is a
        // statement about this client and not about the cluster. Reporting it as
        // `IndexNotFound` would make an empty snapshot indistinguishable from a
        // dropped index.
        let router = IndexRouter::new(nodes());

        assert!(router.needs_refresh());
        assert_eq!(router.routes(&a_ref("ix_a")), Err(RouteError::NoTopology));
    }

    #[test]
    fn a_config_that_moved_the_nodes_invalidates_the_index_list() {
        // What wires the router to the config watcher: the node set changing is
        // a rebalance or a failover, and those are what move partitions.
        let router = router(vec![status("ix_a", 7, 0, "10.0.0.1:8091")]);
        assert!(!router.needs_refresh());

        // A config that says the same thing costs nothing.
        router.set_nodes(nodes());
        assert!(!router.needs_refresh());
        assert!(router.routes(&a_ref("ix_a")).is_ok());

        router.set_nodes(NodeMap::from_nodes([(
            "10.0.0.1".to_string(),
            Some(8091),
            "10.0.0.1".to_string(),
            Some(9101),
        )]));
        assert!(router.needs_refresh());
    }

    #[test]
    fn every_routing_failure_is_worth_a_refresh_and_the_server_saying_no_is_not() {
        // The asymmetry a typed check preserves and a substring match lost: the
        // *router* not finding an index means "not in the list I hold", and the
        // *server* not finding one means "I looked up your defnId and it is
        // gone". Same words, opposite answers.
        assert!(RouteError::IndexNotFound(a_ref("ix_a")).worth_refreshing());
        assert!(RouteError::IndexNotReady(a_ref("ix_a")).worth_refreshing());
        assert!(RouteError::NoTopology.worth_refreshing());
        assert!(RouteError::UnknownNode("10.9.9.9:8091".to_string()).worth_refreshing());
        assert!(RouteError::PartitionsMissing {
            index: a_ref("ix_a"),
            held: 2,
            want: 4,
        }
        .worth_refreshing());
        assert!(RouteError::PartitionClaimedTwice {
            index: a_ref("ix_a"),
            partition: 2,
        }
        .worth_refreshing());

        // A moved partition is the one server error a fresher topology fixes.
        assert!(ServerError::classify("Not my partition").is_retryable_after_refresh());
        assert!(!ServerError::classify("Index not found").is_retryable_after_refresh());
        // And a scan that timed out will time out again against a fresher
        // topology, so retrying it is a second helping of the same wait.
        assert!(!ServerError::classify("Index scan timed out").is_retryable_after_refresh());
    }

    #[test]
    fn the_refresh_decision_survives_the_trip_through_the_crate_error() {
        // Both taxonomies reach a caller as `crate::error::Error`, which is where
        // the substring matching used to happen. Same two answers, read off the
        // types.
        let routing: crate::error::Error = RouteError::IndexNotFound(a_ref("ix_a")).into();
        assert!(worth_refreshing(&routing));

        let moved: crate::error::Error = crate::error::ErrorKind::Indexer(IndexerError::from(
            ServerError::classify("Not my partition for defnId 7"),
        ))
        .into();
        assert!(worth_refreshing(&moved));

        let gone: crate::error::Error = crate::error::ErrorKind::Indexer(IndexerError::from(
            ServerError::classify("Index not found"),
        ))
        .into();
        assert!(
            !worth_refreshing(&gone),
            "the server looked up our defnId and it is gone; refreshing changes nothing"
        );

        assert!(!worth_refreshing(
            &crate::error::ErrorKind::NoEndpointsAvailable.into()
        ));
    }
}
