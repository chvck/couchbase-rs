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

//! Pooled connections to one indexer's scan port.
//!
//! Sits above [`indexerx`](crate::indexerx) the way
//! [`kvclientpool`](crate::kvclientpool) sits above [`memdx`](crate::memdx):
//! the protocol package knows how to be *a* connection, this knows how to have
//! several.
//!
//! It cannot reuse [`kvclientpool`](crate::kvclientpool), which is built around
//! `KvClient` and assumes multiplexing — `active_ops` counting past one,
//! `connection_concurrency` deciding how many operations share a socket.
//! Queryport does not multiplex at all: a connection serves one request, and a
//! *scan* owns its connection until the stream ends. So the degenerate case is
//! different enough in kind, not just in configuration, that a sibling is
//! honester than a type parameter.
//!
//! ### Leases, and where a scan's connection comes back
//!
//! [`ClientPool::acquire`] hands out a [`Lease`]. Dropping it returns the
//! connection — that is safe because the lease only exists while the connection
//! is idle-between-requests, and a request that failed took the lease with it.
//!
//! A scan is the exception, because [`Client::scan`] consumes the client: the
//! stream owns the connection for its lifetime. [`Lease::start_scan`] hands the
//! connection over and leaves the lease holding nothing but the slot, and
//! [`IndexScanStream`](crate::results::index_scan::IndexScanStream) — which owns
//! both — gives the connection back through [`Lease::restore`] when it is
//! dropped. There are two ways that ends, and the split is exactly where `Drop`
//! can and cannot help:
//!
//! - **Read to the end and dropped.** The stream reached its terminator, so
//!   there is nothing to drain and returning the connection is synchronous.
//!   `Drop` does it, which matters because this is the common path and it must
//!   not cost a connection.
//! - **Abandoned part way.** The server still has rows to send, so the
//!   connection would have to be drained before anyone else could use it — and
//!   draining is asynchronous, which `Drop` is not. The socket closes instead,
//!   which is wasteful and never wrong.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::address::Address;
use crate::indexerx::client::{Client, ConnectOptions};
use crate::indexerx::error::Error;
use crate::indexerx::proto::ScanOptions;
use crate::indexerx::scan::ScanStream;

/// Default ceiling on established connections to one host.
///
/// A protection limit rather than a tuning knob, and it has to sit well above
/// any legitimate working set: a cap *below* the offered concurrency does not
/// remove load, it converts throughput into queueing, which reads as a fix and
/// is not one. Measured working set on the perf lab is ~32 concurrent scans per
/// gateway against the one host holding an index.
///
/// **It multiplies by the fleet.** Every gateway keeps its own pool per host, so
/// an indexer fronted by six gateways sees up to `6 × max_connections`. The
/// number that protects the server is this one divided by the fleet size.
pub const DEFAULT_MAX_CONNECTIONS: usize = 256;

/// Default idle lifetime before a pooled connection is closed.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Default wait for a connection slot before reporting backpressure.
pub const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct PoolOptions {
    /// Hard ceiling on connections established to this host at once.
    ///
    /// Enforced by a semaphore whose permits are held by the connections
    /// themselves, so an idle pooled connection still occupies a slot — the cap
    /// is on sockets that exist, not on sockets in use.
    pub max_connections: usize,

    /// How long a connection may sit idle before it is closed.
    ///
    /// **This is the eviction policy**, and it replaced a count cap that was the
    /// cause of a measured defect: keeping only N idle connections means that at
    /// any offered concurrency above N, every return beyond the Nth closes its
    /// socket and the next request dials a fresh one. At 32 concurrent scans
    /// against a cap of 4 that is ~87% of returns, which cost 7-9% of scans to
    /// client timeouts and about a quarter of the indexer's CPU. A time-based
    /// policy lets the pool size follow the working set instead of a guess.
    pub idle_timeout: Duration,

    /// How long [`ClientPool::reserve`] waits for a slot before giving up with
    /// [`PoolExhausted`](crate::indexerx::ErrorKind::PoolExhausted).
    pub acquire_timeout: Duration,

    pub connect: ConnectOptions,
}

impl PoolOptions {
    pub fn new(connect: ConnectOptions) -> Self {
        PoolOptions {
            max_connections: DEFAULT_MAX_CONNECTIONS,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            acquire_timeout: DEFAULT_ACQUIRE_TIMEOUT,
            connect,
        }
    }
}

/// What a pool has been doing, for the operator who has to size it.
///
/// A queue that cannot be seen is the fair half of the objection to having one
/// at all, so waiting is counted rather than merely permitted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Connections established and not yet closed, idle ones included.
    pub established: usize,
    /// Connections pooled and reusable right now.
    pub idle: usize,
    /// Callers waiting for a slot at this instant.
    pub waiting: usize,
    /// Reservations that had to wait at all.
    pub waits: u64,
    /// Total time spent waiting, over every reservation that waited.
    pub wait_total: Duration,
    /// Reservations that gave up at `acquire_timeout`.
    pub timeouts: u64,
    /// Sockets opened, over the pool's life.
    pub dialed: u64,
    /// Sockets closed by the idle sweep, over the pool's life.
    pub reaped: u64,
}

/// A pooled connection and the slot it occupies.
///
/// The permit lives here rather than in the lease, which is what makes every
/// return path correct without any of them saying so: this type owns the right
/// to have the socket open, and whichever way a connection stops being pooled —
/// released, discarded, dropped, drained, or lost to a failed send — dropping it
/// hands the slot back.
struct Pooled {
    client: Client,
    permit: OwnedSemaphorePermit,
    idle_since: Instant,
}

/// The pooled connections, and the generation they were dialled in.
///
/// One mutex over both because they are read together and have to agree: a
/// connection popped from `queue` may only be reused if it belongs to the
/// generation the pool is in now, and a [`ClientPool::drain`] landing between
/// the two reads is exactly what the generation exists to catch.
struct Idle {
    /// LIFO, so a burst reuses the warmest connection. Because `give_back`
    /// pushes to the back and `take_idle` pops it, front-to-back is ascending
    /// return time — so the idle sweep only has to walk the front, and stops at
    /// the first connection young enough to keep.
    queue: VecDeque<Pooled>,
    /// Bumped by [`ClientPool::drain`]. Everything in `queue` belongs to this
    /// generation, because draining clears the queue and a connection returned
    /// from an older one is dropped rather than pooled.
    generation: u64,
}

struct Inner {
    endpoint: Address,
    options: PoolOptions,
    idle: Mutex<Idle>,
    slots: Arc<Semaphore>,
    /// Signalled when a connection is *returned*, which is the one thing
    /// `slots` cannot signal for itself. See
    /// [`wait_for_reservation`](ClientPool::wait_for_reservation).
    returned: tokio::sync::Notify,
    waiting: AtomicUsize,
    waits: AtomicU64,
    wait_micros: AtomicU64,
    timeouts: AtomicU64,
    dialed: AtomicU64,
    reaped: AtomicU64,
}

#[derive(Clone)]
pub struct ClientPool {
    inner: Arc<Inner>,
}

impl ClientPool {
    pub fn new(endpoint: Address, options: PoolOptions) -> ClientPool {
        let slots = Arc::new(Semaphore::new(options.max_connections));
        let sweep_every = (options.idle_timeout / 2).max(Duration::from_secs(1));
        let pool = ClientPool {
            inner: Arc::new(Inner {
                endpoint,
                options,
                idle: Mutex::new(Idle {
                    queue: VecDeque::new(),
                    generation: 0,
                }),
                slots,
                returned: tokio::sync::Notify::new(),
                waiting: AtomicUsize::new(0),
                waits: AtomicU64::new(0),
                wait_micros: AtomicU64::new(0),
                timeouts: AtomicU64::new(0),
                dialed: AtomicU64::new(0),
                reaped: AtomicU64::new(0),
            }),
        };
        pool.spawn_reaper(sweep_every);
        pool
    }

    /// Sweep on a timer as well as on use.
    ///
    /// **Sweeping only on `reserve` and `give_back` never reaps anything**,
    /// because those are exactly the moments the pool is *busy*: when traffic
    /// stops, nothing calls them again and the connections stay open forever.
    /// That is not theoretical — gateways were found holding three hundred idle
    /// connections to indexers with no load running at all, which is the state
    /// the idle timeout exists to prevent and the one a lazy-only sweep
    /// guarantees.
    ///
    /// **Weak, so the task is not what keeps the pool alive.** It exits on the
    /// first tick after the last `ClientPool` goes away, which costs at most one
    /// interval and needs no shutdown plumbing. And it only spawns inside a
    /// runtime, so constructing a pool in a synchronous test stays legal.
    fn spawn_reaper(&self, every: Duration) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak = Arc::downgrade(&self.inner);
        handle.spawn(async move {
            let mut ticker = tokio::time::interval(every);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                ClientPool { inner }.sweep_idle();
            }
        });
    }

    /// Claim a connection slot, reusing an idle connection if one is warm.
    ///
    /// **Split from dialling on purpose.** A scattered scan needs a connection
    /// to every host at once, and a slot is worth claiming before any round trip
    /// is paid for: reserving is an in-process semaphore acquisition with no
    /// network in it, so the dialling still happens concurrently afterwards and
    /// the scatter costs one round trip rather than one per host.
    ///
    /// **Taking slots from several pools at once can deadlock** — two scatters
    /// each holding the slot the other is waiting for — and nothing here
    /// prevents it. `acquire_timeout` bounds it instead, and `max_connections`
    /// is a protection limit set far above any working set, so reaching the cap
    /// at all means the offered concurrency is already past what the indexer can
    /// serve. A caller wanting the guarantee rather than the bound would reserve
    /// every pool in a fixed endpoint order before connecting any of them.
    pub async fn reserve(&self) -> Result<Reservation, Error> {
        self.sweep_idle();

        // The uncontended path must not pay for the bookkeeping the contended
        // one needs, and it is the overwhelmingly common one.
        if let Some(reservation) = self.try_reserve() {
            return Ok(reservation);
        }

        let started = Instant::now();
        self.inner.waiting.fetch_add(1, Ordering::Relaxed);
        let waited = tokio::time::timeout(
            self.inner.options.acquire_timeout,
            self.wait_for_reservation(),
        )
        .await;
        self.inner.waiting.fetch_sub(1, Ordering::Relaxed);

        let elapsed = started.elapsed();
        self.inner.waits.fetch_add(1, Ordering::Relaxed);
        self.inner
            .wait_micros
            .fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);

        waited.map_err(|_| {
            self.inner.timeouts.fetch_add(1, Ordering::Relaxed);
            Error::new_pool_exhausted_error(self.inner.endpoint.to_string(), elapsed)
        })
    }

    /// Reserve a slot and connect, for callers with only one host to talk to.
    pub async fn acquire(&self) -> Result<Lease, Error> {
        self.reserve().await?.connect().await
    }

    /// A warm connection, or a free slot to dial into, if either is there now.
    fn try_reserve(&self) -> Option<Reservation> {
        if let Some((pooled, generation)) = self.take_idle() {
            return Some(Reservation {
                client: Some(pooled.client),
                permit: pooled.permit,
                pool: self.clone(),
                generation,
            });
        }

        let permit = self.inner.slots.clone().try_acquire_owned().ok()?;
        // Read after the slot is claimed rather than before: a drain has nothing
        // to say about a socket that does not exist yet, and the generation is
        // there to mark connections that were already open when the credentials
        // changed.
        Some(Reservation {
            client: None,
            permit,
            pool: self.clone(),
            generation: self.generation(),
        })
    }

    /// Wait for a connection to come back, or for a slot to come free.
    ///
    /// **Both, because the semaphore can only report one of them.** A returned
    /// connection keeps its permit — that is what makes the cap count sockets
    /// that *exist* rather than sockets in use — so the available count does not
    /// move when one is pooled. A caller parked on the semaphore alone would
    /// therefore sleep through every return and wake only when a socket
    /// *closed*: at the cap, a stall for the whole `acquire_timeout` ending in
    /// backpressure, while the pool holds an idle connection the waiter was
    /// entitled to.
    ///
    /// So returning also signals `returned`, and this waits on either.
    /// **Interest is registered before the re-check**, or a return landing
    /// between the two would be missed and the wait would be for nothing.
    ///
    /// A caller arriving fresh can still take the connection this one was woken
    /// for, because `reserve` tries before it queues. That is ordinary
    /// unfairness rather than a stall — the loop waits again — and it is bounded
    /// by `acquire_timeout`. Fairness here would mean handing a returned
    /// connection to a named waiter, which costs a queue of wakers to serve a
    /// case that only arises at a cap set far above the working set.
    async fn wait_for_reservation(&self) -> Reservation {
        loop {
            let returned = self.inner.returned.notified();
            tokio::pin!(returned);
            returned.as_mut().enable();

            if let Some(reservation) = self.try_reserve() {
                return reservation;
            }

            tokio::select! {
                permit = self.inner.slots.clone().acquire_owned() => {
                    // The semaphore is never closed, so the error is
                    // unreachable; looping on it is the safe reading either way.
                    if let Ok(permit) = permit {
                        return Reservation {
                            client: None,
                            permit,
                            pool: self.clone(),
                            generation: self.generation(),
                        };
                    }
                }
                () = returned => {}
            }
        }
    }

    /// How many connections are pooled. Test and metric surface.
    pub fn idle_count(&self) -> usize {
        self.inner.idle.lock().expect("pool mutex").queue.len()
    }

    /// What this pool has been doing, for sizing it.
    pub fn stats(&self) -> PoolStats {
        let idle = self.idle_count();
        let available = self.inner.slots.available_permits();
        PoolStats {
            established: self.inner.options.max_connections.saturating_sub(available),
            idle,
            waiting: self.inner.waiting.load(Ordering::Relaxed),
            waits: self.inner.waits.load(Ordering::Relaxed),
            wait_total: Duration::from_micros(self.inner.wait_micros.load(Ordering::Relaxed)),
            timeouts: self.inner.timeouts.load(Ordering::Relaxed),
            dialed: self.inner.dialed.load(Ordering::Relaxed),
            reaped: self.inner.reaped.load(Ordering::Relaxed),
        }
    }

    /// Mark every connection unusable.
    ///
    /// Idle connections are closed now; leased ones are discarded rather than
    /// returned when their holder releases them, because a scan already
    /// streaming on one is still a valid scan and killing it mid-read would turn
    /// a rotation into a user-visible error.
    ///
    /// **It says nothing about connections dialled afterwards**, which is why
    /// [`IndexComponent`](crate::indexcomponent) drops the pool from its map in
    /// the same breath: a pool's [`ConnectOptions`] are fixed when it is built,
    /// so the credentials on the wire only really change when the pool does.
    pub fn drain(&self) {
        let drained = {
            let mut idle = self.inner.idle.lock().expect("pool mutex");
            idle.generation += 1;
            std::mem::take(&mut idle.queue)
        };

        // Dropped outside the lock. Closing these sockets hands their permits
        // back, which can wake a caller parked for one, and waking it while
        // holding the mutex would have it contend for the lock it needs the
        // instant it runs.
        drop(drained);
    }

    /// Close connections that have been idle past `idle_timeout`.
    ///
    /// Swept on reserve and on return, and on a timer besides, because those two
    /// only happen while the pool is busy — see
    /// [`spawn_reaper`](ClientPool::spawn_reaper). The deque is ordered by
    /// return time, so this walks only what it closes. Dropping the `Pooled`
    /// closes the socket and hands its slot back in the same move.
    fn sweep_idle(&self) {
        let timeout = self.inner.options.idle_timeout;
        let now = Instant::now();

        let mut closed = Vec::new();
        {
            let mut idle = self.inner.idle.lock().expect("pool mutex");
            while idle
                .queue
                .front()
                .is_some_and(|p| now.duration_since(p.idle_since) >= timeout)
            {
                closed.push(idle.queue.pop_front().expect("just peeked at it"));
            }
        }

        if !closed.is_empty() {
            self.inner
                .reaped
                .fetch_add(closed.len() as u64, Ordering::Relaxed);
            // Dropped outside the lock, for the reason `drain` gives.
            drop(closed);
        }
    }

    /// The warmest pooled connection, and the generation to stamp its lease
    /// with. Both under one lock, so a [`drain`](ClientPool::drain) cannot land
    /// between them and make a connection it closed look current.
    fn take_idle(&self) -> Option<(Pooled, u64)> {
        let mut idle = self.inner.idle.lock().expect("pool mutex");
        let generation = idle.generation;
        idle.queue.pop_back().map(|pooled| (pooled, generation))
    }

    fn generation(&self) -> u64 {
        self.inner.idle.lock().expect("pool mutex").generation
    }

    fn give_back(&self, client: Client, permit: OwnedSemaphorePermit, generation: u64) {
        self.sweep_idle();

        let pooled = Pooled {
            client,
            permit,
            idle_since: Instant::now(),
        };
        let stale = {
            let mut idle = self.inner.idle.lock().expect("pool mutex");
            if generation == idle.generation {
                idle.queue.push_back(pooled);
                None
            } else {
                Some(pooled)
            }
        };

        match stale {
            // Claimed before a drain, so it authenticated with credentials that
            // have since been replaced. Dropping it — outside the lock, for the
            // reason `drain` gives — closes the socket and hands its slot back
            // with it, which is a wake the semaphore delivers on its own.
            Some(pooled) => drop(pooled),
            // A pooled connection moves no semaphore count, so this is the only
            // thing that can tell a waiter a connection is free.
            None => self.inner.returned.notify_one(),
        }
    }

    /// Pool a connection nobody dialled, for the tests about bookkeeping.
    ///
    /// Takes a real slot, so a test that puts one here and then acquires it sees
    /// the same accounting a dialled connection would produce.
    #[cfg(test)]
    fn put_idle_for_test(&self, client: Client) {
        let permit = self
            .inner
            .slots
            .clone()
            .try_acquire_owned()
            .expect("a free slot");
        let generation = self.generation();
        self.give_back(client, permit, generation);
    }
}

/// A claimed connection slot, not yet necessarily a connection.
///
/// Holding one guarantees the pool has room for this connection; dropping one
/// without connecting hands the room back.
pub struct Reservation {
    client: Option<Client>,
    permit: OwnedSemaphorePermit,
    pool: ClientPool,
    generation: u64,
}

impl Reservation {
    /// Reuse the warm connection, or dial one into the slot already claimed.
    pub async fn connect(self) -> Result<Lease, Error> {
        let Reservation {
            client,
            permit,
            pool,
            generation,
        } = self;

        let client = match client {
            Some(client) => client,
            None => {
                // A dial that fails drops `permit` here, which frees the slot.
                let client =
                    Client::connect(&pool.inner.endpoint, &pool.inner.options.connect).await?;
                pool.inner.dialed.fetch_add(1, Ordering::Relaxed);
                client
            }
        };

        Ok(Lease {
            client: Some(client),
            permit: Some(permit),
            pool,
            generation,
        })
    }
}

/// A connection borrowed from the pool.
pub struct Lease {
    client: Option<Client>,
    /// The slot this connection occupies. `Some` while the lease is live; taken
    /// when the connection moves on, to the pool or to nothing.
    permit: Option<OwnedSemaphorePermit>,
    pool: ClientPool,
    /// The pool generation this slot was claimed in. A
    /// [`drain`](ClientPool::drain) since then means the connection goes away
    /// rather than back.
    generation: u64,
}

impl Lease {
    /// The borrowed connection, for the operations that only borrow it.
    ///
    /// Panics after [`start_scan`](Lease::start_scan), which gives the
    /// connection to the stream: from then until [`restore`](Lease::restore) the
    /// lease holds a slot and nothing else.
    pub fn client_mut(&mut self) -> &mut Client {
        self.client.as_mut().expect("a lease always holds a client")
    }

    /// Start a scan, which takes the connection for the stream's lifetime.
    ///
    /// [`Client::scan`] consumes the client, so the lease cannot go on holding
    /// it: the connection moves into the [`ScanStream`] and comes back through
    /// [`restore`](Lease::restore) when whoever owns the stream is finished with
    /// it. Meanwhile the lease keeps the slot open on the stream's behalf, which
    /// is what stops the cap being a lie for as long as scans are running.
    pub async fn start_scan(&mut self, opts: ScanOptions) -> Result<ScanStream, Error> {
        let client = self.client.take().expect("a lease always holds a client");

        // A request that could not be sent leaves no stream to own the
        // connection, so it closes here rather than going back into the pool in
        // an unknown state — the lease still holds the permit, and frees the
        // slot when it drops.
        client.scan(opts).await
    }

    /// Take a connection back from a stream that ended cleanly.
    ///
    /// The counterpart to [`start_scan`](Lease::start_scan): from here the
    /// lease's ordinary return paths apply again, so dropping it pools the
    /// connection exactly as if no scan had happened.
    pub fn restore(&mut self, client: Client) {
        self.client = Some(client);
    }

    /// Give the connection back explicitly, rather than on drop.
    ///
    /// Only worth calling when a caller wants the pool's count to be accurate
    /// at a specific moment — a test, or a metric read straight afterwards.
    pub fn release(mut self) {
        if let (Some(client), Some(permit)) = (self.client.take(), self.permit.take()) {
            self.pool.give_back(client, permit, self.generation);
        }
    }

    /// Discard the connection instead of pooling it.
    ///
    /// For when something happened that this type cannot see but the caller
    /// can — an operation that left the connection's state in doubt. The slot
    /// goes back with the socket.
    pub fn discard(mut self) {
        drop(self.client.take());
        drop(self.permit.take());
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let (Some(client), Some(permit)) = (self.client.take(), self.permit.take()) {
            self.pool.give_back(client, permit, self.generation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_address() -> Address {
        Address {
            host: "127.0.0.1".to_string(),
            port: 9101,
        }
    }

    fn test_connect_options() -> ConnectOptions {
        ConnectOptions::new("user", "pass")
    }

    /// A connection to nothing.
    ///
    /// Every test here is about the pool's bookkeeping — which connection comes
    /// back out, which one is dropped, whose slot is free — and none of them
    /// sends a byte, so a [`Client`] over a pipe with nothing on the far end is
    /// the whole of what they need. The version number is how a test tells one
    /// of them from another.
    fn fake_client(server_version: u32) -> Client {
        Client::for_test(server_version)
    }

    fn test_pool() -> ClientPool {
        ClientPool::new(test_address(), PoolOptions::new(test_connect_options()))
    }

    /// The pool is built and driven synchronously here on purpose: no reaper
    /// task spawns outside a runtime, so a sweep only happens where a test asks
    /// for one.
    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        futures::executor::block_on(f)
    }

    #[test]
    fn a_pooled_connection_is_handed_back_out() {
        let pool = test_pool();
        pool.put_idle_for_test(fake_client(7));

        let mut lease = block_on(pool.acquire()).expect("a warm connection");

        assert_eq!(
            lease.client_mut().server_version(),
            7,
            "acquire reused the pooled connection rather than dialling"
        );
        assert_eq!(pool.idle_count(), 0, "and took it out of the pool");
        assert_eq!(
            pool.stats().dialed,
            0,
            "reuse is the whole point: nothing was dialled"
        );

        lease.release();
        assert_eq!(pool.idle_count(), 1, "released back into the pool");
    }

    #[test]
    fn a_discarded_connection_frees_its_slot_without_being_pooled() {
        let pool = test_pool();
        pool.put_idle_for_test(fake_client(1));
        let lease = block_on(pool.acquire()).expect("a warm connection");

        lease.discard();

        assert_eq!(pool.idle_count(), 0, "a discarded connection is not pooled");
        assert_eq!(
            pool.stats().established,
            0,
            "but its slot came back, or the cap would leak one per discard"
        );
    }

    #[test]
    fn an_idle_connection_older_than_the_timeout_is_closed() {
        // The policy this file exists for. A pool that never reaps holds its
        // connections open against an indexer doing nothing at all.
        let pool = ClientPool::new(
            test_address(),
            PoolOptions {
                idle_timeout: Duration::ZERO,
                ..PoolOptions::new(test_connect_options())
            },
        );
        pool.put_idle_for_test(fake_client(1));

        // Reserving sweeps first, and at a zero timeout everything pooled is
        // already too old to keep.
        drop(block_on(pool.reserve()).expect("a slot"));

        assert_eq!(pool.idle_count(), 0);
        assert_eq!(pool.stats().reaped, 1, "and it was counted as reaped");
    }

    #[test]
    fn a_drained_pool_does_not_hand_back_an_old_connection() {
        let pool = ClientPool::new(test_address(), PoolOptions::new(test_connect_options()));
        pool.put_idle_for_test(fake_client(1));
        assert_eq!(pool.idle_count(), 1);

        pool.drain();

        assert_eq!(
            pool.idle_count(),
            0,
            "a drained pool must not serve a connection dialled with the old credentials"
        );
    }

    #[test]
    fn a_lease_taken_before_a_drain_does_not_return_its_connection() {
        // The half of draining that `idle_count` cannot show on its own: a scan
        // still streaming when the credentials rotate keeps running, and its
        // connection is dropped rather than pooled when it ends.
        let pool = test_pool();
        pool.put_idle_for_test(fake_client(1));
        let lease = block_on(pool.acquire()).expect("a warm connection");

        pool.drain();
        lease.release();

        assert_eq!(
            pool.idle_count(),
            0,
            "the connection was claimed under the credentials the drain replaced"
        );
        assert_eq!(
            pool.stats().established,
            0,
            "and its slot came back rather than being held by a socket nobody may use"
        );
    }

    /// A pool that can hold exactly one connection, so the second caller has to
    /// wait. `acquire_timeout` is short enough that a test which is *meant* to
    /// give up does so quickly, and long enough that one which is not has no
    /// reason to.
    fn capped_pool() -> ClientPool {
        ClientPool::new(
            test_address(),
            PoolOptions {
                max_connections: 1,
                acquire_timeout: Duration::from_millis(200),
                ..PoolOptions::new(test_connect_options())
            },
        )
    }

    /// Spin until the pool reports a caller parked in `wait_for_slot`.
    ///
    /// Sleeping a fixed interval instead would make the test either slow or
    /// flaky, and it is the *waiting* that the test needs to have started, not
    /// some duration to have passed.
    async fn wait_until_someone_is_waiting(pool: &ClientPool) {
        for _ in 0..10_000 {
            if pool.stats().waiting == 1 {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("nobody ever parked for a slot");
    }

    #[tokio::test]
    async fn a_pool_at_its_cap_reports_backpressure_rather_than_waiting_forever() {
        // The only error this module raises on its own, and the whole contended
        // path that produces it: nothing else here leaves the uncontended
        // `try_acquire_owned` fast path.
        let pool = capped_pool();
        pool.put_idle_for_test(fake_client(1));
        let _held = pool.acquire().await.expect("the only slot");

        // `let else` rather than `expect_err`, which would want a `Debug` on
        // `Lease` that nothing else in the crate needs.
        let Err(err) = pool.acquire().await else {
            panic!("a second connection is over the cap");
        };

        assert!(
            matches!(
                err.kind(),
                crate::indexerx::ErrorKind::PoolExhausted { endpoint, .. }
                    if endpoint == &test_address().to_string()
            ),
            "backpressure names the host it is about, got {err}"
        );
        let stats = pool.stats();
        assert_eq!(stats.timeouts, 1, "and is counted, or it cannot be sized");
        assert_eq!(stats.waits, 1, "the wait itself is counted too");
        assert!(stats.wait_total > Duration::ZERO);
        assert_eq!(stats.waiting, 0, "and nobody is left parked afterwards");
    }

    #[tokio::test]
    async fn a_waiter_at_the_cap_is_served_by_a_returned_connection() {
        // A connection coming back is a slot becoming free. If only a *closing*
        // socket wakes a waiter, then at the cap the pool stalls for the whole
        // acquire timeout and then reports exhaustion while holding an idle
        // connection the waiter was entitled to.
        let pool = capped_pool();
        pool.put_idle_for_test(fake_client(9));
        let held = pool.acquire().await.expect("the only slot");

        let waiter = tokio::spawn({
            let pool = pool.clone();
            async move {
                let mut lease = pool.acquire().await?;
                Ok::<u32, Error>(lease.client_mut().server_version())
            }
        });

        wait_until_someone_is_waiting(&pool).await;
        held.release();

        let served = waiter
            .await
            .expect("the waiter's task")
            .expect("the waiter got a connection rather than backpressure");

        assert_eq!(
            served, 9,
            "and it is the connection that was returned, not a fresh dial"
        );
        assert_eq!(pool.stats().dialed, 0, "nothing was dialled");
    }

    #[test]
    fn a_connection_claimed_after_a_drain_is_poolable_again() {
        // Draining marks a generation, not the pool: connections claimed after
        // it are ordinary, or one rotation would stop the pool ever working.
        let pool = test_pool();
        pool.drain();
        pool.put_idle_for_test(fake_client(1));

        let lease = block_on(pool.acquire()).expect("a warm connection");
        lease.release();

        assert_eq!(pool.idle_count(), 1);
    }
}
