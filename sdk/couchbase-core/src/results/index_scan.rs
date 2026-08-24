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

//! The entries of one index scan, and the order they are in.
//!
//! # The ordering contract, which is the design decision in this module
//!
//! **Within one node's stream, entries are in index order. Across nodes there is
//! no order at all, and this crate will not create one.**
//!
//! An index scan is scattered when the index is partitioned: one stream per host
//! holding a piece of it (see [`crate::indexrouter`]), each of them sorted,
//! arriving concurrently. Turning N sorted streams into one sorted stream is a
//! merge, and a merge needs to compare two entry keys.
//!
//! That comparison is the problem. Entry keys arrive as JSON text under
//! [`DataEncoding::Json`](crate::indexerx::DataEncoding), and JSON text does not
//! compare in index order: as text, `[10]` sorts before `[9]`, `"B"` sorts
//! before `"a"`, and a number written `1.0` and one written `1` are different
//! strings and the same value. Ordering them means decoding each key into a
//! value and comparing those values under a collation — N1QL's, or the calling
//! application's, which are not always the same one. This crate does not know
//! which collation a caller wants and cannot guess one that is right for
//! everybody: a merge under the wrong collation returns every entry, in an order
//! that looks sorted and is not, which is worse than returning them unsorted and
//! saying so.
//!
//! So the contract is the one cbcore-rs settled on, kept deliberately:
//!
//! - **What the caller gets for free.** Every entry, exactly once, from every
//!   host holding part of the index — the part that needs routing, and that a
//!   caller cannot do for itself. Scanning `IndexScanResults` as a
//!   [`Stream`] yields them in arrival order.
//! - **What it costs the caller.** An ordered read of a *partitioned* index is
//!   the caller's merge to write. [`IndexScanResults::into_streams`] hands over
//!   the per-host streams, each already sorted, so the merge is a k-way merge
//!   under the caller's own comparison and not a re-sort of everything.
//! - **The case that pays nothing.** An index that is not partitioned is one
//!   stream, so its output is already in index order.
//!   [`IndexScanResults::is_index_ordered`] is that question, asked of the scan
//!   in hand rather than assumed — because an index that gets partitioned later
//!   turns a correct program into a quietly wrong one, and this is the line that
//!   catches it.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;

use crate::address::Address;
use crate::error::{Error, ErrorKind};
use crate::indexerclient_provider::Lease;
use crate::indexerx::{ScanEntry, ScanStream};

/// One host's share of a scan: its stream, and which partitions it is reading.
///
/// Sorted, because the request asks the indexer to sort and a single indexer
/// answers for the partitions it holds.
pub struct IndexScanStream {
    address: Address,
    partitions: Vec<u64>,
    stream: ScanStream,
    /// **The connection the scan lives on**, held for the stream's life. The
    /// server releases a scan when its connection goes away, so returning the
    /// lease early would end the scan the caller is still reading.
    lease: Lease,
}

impl IndexScanStream {
    pub(crate) fn new(
        address: Address,
        partitions: Vec<u64>,
        stream: ScanStream,
        lease: Lease,
    ) -> IndexScanStream {
        IndexScanStream {
            address,
            partitions,
            stream,
            lease,
        }
    }

    /// The queryport this share came from.
    pub fn address(&self) -> &Address {
        &self.address
    }

    /// The partitions this host holds. `[0]` for an index that is not
    /// partitioned — the server's numbering, unchanged.
    pub fn partitions(&self) -> &[u64] {
        &self.partitions
    }

    /// The read units this share consumed, once it has ended. `None` before
    /// then, and on a cluster that does not meter.
    pub fn read_units(&self) -> Option<u64> {
        self.stream.read_units()
    }

    /// Whether this share reached its terminator, as opposed to being abandoned
    /// or having failed.
    pub fn is_ended(&self) -> bool {
        self.stream.is_ended()
    }
}

/// Give the connection back when the scan is over.
///
/// **Only when the stream reached its terminator**, which is the one case that
/// needs no draining and so the only one `Drop` can settle: a scan abandoned
/// part way still has rows coming, and nobody else may use the connection until
/// they have been read. Closing it instead is wasteful and never wrong. It is
/// also not the common path — the common path is a scan read to its end, and
/// that one must not cost a connection or the pool above is pointless.
impl Drop for IndexScanStream {
    fn drop(&mut self) {
        if let Some(client) = self.stream.take_if_ended() {
            self.lease.restore(client);
        }
    }
}

/// The stream's shape, not its contents: which host, which partitions, and
/// whether it has ended. There is nothing readable inside a live stream, and a
/// `Debug` that consumed one would be a `Debug` that changed the answer.
impl std::fmt::Debug for IndexScanStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexScanStream")
            .field("address", &self.address)
            .field("partitions", &self.partitions)
            .field("ended", &self.stream.is_ended())
            .finish()
    }
}

impl Stream for IndexScanStream {
    type Item = Result<ScanEntry, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().stream)
            .poll_next(cx)
            .map(|entry| entry.map(|e| e.map_err(|e| Error::from(ErrorKind::Indexer(e)))))
    }
}

/// The entries of one index scan, from every host holding part of the index.
///
/// **Read the module docs before relying on the order they come out in.**
///
/// Dropping this abandons the scan, which is safe by construction: a stream that
/// had not ended closes its connection rather than returning it to the pool, so
/// no half-read response is left behind for the next request on that socket to
/// mistake for its own.
pub struct IndexScanResults {
    streams: Vec<IndexScanStream>,
    /// Indices into `streams` that have not ended yet. Ended streams stay in
    /// `streams` — they still hold the read units they consumed, and a caller
    /// asking what a finished scan cost should not get an empty answer.
    active: Vec<usize>,
    /// Which of `active` to poll first next time, so that a fast host cannot
    /// starve a slow one and the buffering stays bounded by the streams
    /// themselves.
    next: usize,
    defn_id: u64,
    inst_id: u64,
    replica_id: u32,
}

impl IndexScanResults {
    pub(crate) fn new(
        streams: Vec<IndexScanStream>,
        defn_id: u64,
        inst_id: u64,
        replica_id: u32,
    ) -> IndexScanResults {
        IndexScanResults {
            active: (0..streams.len()).collect(),
            streams,
            next: 0,
            defn_id,
            inst_id,
            replica_id,
        }
    }

    /// What the scan addressed. The queryport wire format has no index names, so
    /// this is the only identity the request carried.
    pub fn defn_id(&self) -> u64 {
        self.defn_id
    }

    /// Which copy of the index was read. Every stream here shares it — that is
    /// what makes them one copy of the index rather than a mixture of replicas.
    pub fn inst_id(&self) -> u64 {
        self.inst_id
    }

    pub fn replica_id(&self) -> u32 {
        self.replica_id
    }

    /// How many hosts this scan is spread over.
    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }

    /// Whether reading this as one [`Stream`] yields entries in index order.
    ///
    /// True exactly when the scan is not scattered. Assert on it rather than on
    /// the index's shape: an index that is partitioned later silently changes
    /// the answer, and this is where that shows up.
    pub fn is_index_ordered(&self) -> bool {
        self.streams.len() == 1
    }

    /// The read units the scan consumed, summed over the hosts that reported
    /// any.
    ///
    /// Meaningful once the scan has ended; before then it is the total so far.
    pub fn read_units(&self) -> Option<u64> {
        self.streams
            .iter()
            .filter_map(IndexScanStream::read_units)
            .reduce(|a, b| a + b)
    }

    /// The per-host streams, for a caller merging them under its own collation.
    ///
    /// Each is sorted; the merge is a k-way merge and not a re-sort. See the
    /// module docs for why the merge is here rather than in this crate.
    pub fn into_streams(self) -> Vec<IndexScanStream> {
        self.streams
    }
}

impl std::fmt::Debug for IndexScanResults {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexScanResults")
            .field("defn_id", &self.defn_id)
            .field("inst_id", &self.inst_id)
            .field("replica_id", &self.replica_id)
            .field("streams", &self.streams)
            .finish()
    }
}

impl Stream for IndexScanResults {
    type Item = Result<ScanEntry, Error>;

    /// Entries in **arrival order**, which is index order only when the scan is
    /// not scattered.
    ///
    /// An error ends the whole scan: one host failing means the answer is short
    /// by whatever that host held, and there is no result to be built out of the
    /// rest that is not silently incomplete. The other streams are dropped there
    /// and then, which closes their connections — so
    /// [`into_streams`](IndexScanResults::into_streams) after an error hands
    /// back nothing, and [`read_units`](IndexScanResults::read_units) stops
    /// counting.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            if this.active.is_empty() {
                // Every host reached its terminator, or one of them failed.
                return Poll::Ready(None);
            }

            let mut pending = false;
            let mut ended = None;

            for offset in 0..this.active.len() {
                let slot = (this.next + offset) % this.active.len();
                let stream = &mut this.streams[this.active[slot]];

                match Pin::new(stream).poll_next(cx) {
                    Poll::Ready(Some(Ok(entry))) => {
                        this.next = (slot + 1) % this.active.len();
                        return Poll::Ready(Some(Ok(entry)));
                    }
                    Poll::Ready(Some(Err(e))) => {
                        // Dropped rather than parked: the answer is already
                        // short, so the only thing the other hosts still hold
                        // is open sockets.
                        this.active.clear();
                        this.streams.clear();
                        return Poll::Ready(Some(Err(e)));
                    }
                    Poll::Ready(None) => {
                        ended = Some(slot);
                        break;
                    }
                    Poll::Pending => pending = true,
                }
            }

            match ended {
                // Out of the rotation, but not out of `streams`: polling an
                // ended stream once per entry for the rest of the scan is the
                // thing this avoids.
                Some(slot) => {
                    this.active.remove(slot);
                    this.next = 0;
                }
                None => {
                    return if pending {
                        Poll::Pending
                    } else {
                        Poll::Ready(None)
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;
    use crate::indexerclient_provider::{ClientPool, PoolOptions};
    use crate::indexerx::proto::{fake, ScanOptions};
    use crate::indexerx::test_server::{ScanEnding, Script, TestServer};
    use crate::indexerx::ConnectOptions;

    /// A pool pointed at a scripted queryport, and one share of a scan read from
    /// it. Returned separately because every test here asks the same question of
    /// them: what the pool holds once the share is gone.
    async fn scan_against(script: Script) -> (ClientPool, IndexScanStream) {
        let server = TestServer::start(script).await;
        let pool = ClientPool::new(
            server.addr.clone(),
            PoolOptions::new(ConnectOptions::new("user", "pass")),
        );

        let mut lease = pool.acquire().await.expect("a connection");
        let stream = lease
            .start_scan(ScanOptions::new(7, "req-1"))
            .await
            .expect("a scan");

        let share = IndexScanStream::new(server.addr.clone(), vec![0], stream, lease);
        // The server is dropped here on purpose: it accepts one connection and
        // the scan already has it, so nothing outlives it that the test needs.
        (pool, share)
    }

    #[tokio::test]
    async fn a_scan_read_to_its_end_gives_its_connection_back() {
        // The property the pool exists for. Without it every route of every scan
        // costs a socket, and the pool is a map of deques that never fill.
        let (pool, mut share) = scan_against(Script::default()).await;

        while share.next().await.is_some() {}

        assert!(share.is_ended());
        assert_eq!(pool.idle_count(), 0, "the connection is still the scan's");

        drop(share);

        assert_eq!(
            pool.idle_count(),
            1,
            "and goes back to the pool once the scan is finished with it"
        );
    }

    #[tokio::test]
    async fn a_scan_abandoned_part_way_closes_its_connection_instead() {
        // The server still has rows to send, so the connection cannot be reused
        // until they have been read — and draining is asynchronous, which `Drop`
        // is not. Closing it is wasteful and never wrong; pooling it would hand
        // the next scan somebody else's rows.
        let script = Script {
            responses: vec![fake::entries(&[
                (None, &b"doc-1"[..]),
                (None, &b"doc-2"[..]),
            ])],
            // Not `Terminated`, which would have sent the terminator along with
            // the rows and left nothing outstanding. `AwaitEndStream` is the
            // server that has more to send and waits to be told to stop — the
            // shape of every abandoned scan, and the one this test is about.
            ending: ScanEnding::AwaitEndStream,
            ..Script::default()
        };
        let (pool, mut share) = scan_against(script).await;

        share.next().await.expect("an entry").expect("no error");
        assert!(!share.is_ended());

        drop(share);

        assert_eq!(
            pool.idle_count(),
            0,
            "a scan that was still running does not leave a reusable connection"
        );
        assert_eq!(
            pool.stats().established,
            0,
            "but its slot came back, or an abandoned scan would cost the cap one"
        );
    }
}
