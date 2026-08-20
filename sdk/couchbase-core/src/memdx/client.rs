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

use bytes::{Bytes, BytesMut};
use futures::{SinkExt, TryFutureExt};
use snap::raw::Decoder;
use std::backtrace::Backtrace;
use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::io;
use std::io::{empty, IoSlice};
use std::net::SocketAddr;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::spawn;
use std::{env, mem};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, Join, ReadHalf, WriteHalf};
use tokio::select;
use tokio::sync::mpsc::unbounded_channel;
use tokio::sync::mpsc::{Receiver, Sender, UnboundedReceiver, UnboundedSender};
use tokio::sync::{mpsc, oneshot, Mutex, MutexGuard, RwLock};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tokio_util::codec::FramedRead;
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

use crate::memdx::client_response::ClientResponse;
use crate::memdx::codec::KeyValueCodec;
use crate::memdx::connection::{ConnectionType, Stream};
use crate::memdx::datatype::DataTypeFlag;
use crate::memdx::dispatcher::{
    Dispatcher, DispatcherOptions, OnReadLoopCloseHandler, OrphanResponseHandler,
    UnsolicitedPacketHandler,
};
use crate::memdx::error;
use crate::memdx::error::{CancellationErrorKind, Error};
use crate::memdx::hello_feature::HelloFeature::DataType;
use crate::memdx::magic::Magic;
use crate::memdx::opcode::OpCode;
use crate::memdx::packet::{RequestPacket, ResponsePacket};
use crate::memdx::pendingop::ClientPendingOp;
use crate::memdx::subdoc::SubdocRequestInfo;
use crate::orphan_reporter::OrphanContext;

pub(crate) type OpaqueMap = HashMap<u32, SenderContext>;

#[derive(Debug, Clone)]
pub struct ResponseContext {
    pub cas: Option<u64>,
    pub subdoc_info: Option<SubdocRequestInfo>,
    pub scope_name: Option<String>,
    pub collection_name: Option<String>,
}

/// The channel one dispatched operation's replies come back on.
///
/// Almost every memcached operation is answered exactly once. A one-shot channel is a
/// single allocation and is consumed by the reply it carries, where an mpsc has to be
/// built to hold a queue that will never hold more than one thing. Only a *persistent*
/// operation -- one that answers many times under a single opaque, as a range scan does --
/// needs the queue, so which channel an operation gets is decided by that flag and by
/// nothing else.
#[derive(Debug)]
pub(crate) enum ResponseSender {
    OneShot(oneshot::Sender<error::Result<ClientResponse>>),
    Streaming(mpsc::Sender<error::Result<ClientResponse>>),
}

impl ResponseSender {
    /// The reply channel for an operation, sized by whether it answers once or many times.
    pub(crate) fn new_pair(is_persistent: bool) -> (ResponseSender, ResponseReceiver) {
        if is_persistent {
            let (tx, rx) = mpsc::channel(1);
            (
                ResponseSender::Streaming(tx),
                ResponseReceiver::Streaming(rx),
            )
        } else {
            let (tx, rx) = oneshot::channel();
            (
                ResponseSender::OneShot(tx),
                ResponseReceiver::OneShot(Some(rx)),
            )
        }
    }

    pub(crate) fn is_persistent(&self) -> bool {
        matches!(self, ResponseSender::Streaming(_))
    }

    /// Hand one reply to whoever is waiting for it, returning it if nobody is.
    ///
    /// An undeliverable reply comes back rather than being dropped, because a response
    /// nobody is waiting for still has somewhere to go -- the orphan reporter.
    pub(crate) async fn send(
        self,
        response: error::Result<ClientResponse>,
    ) -> Option<error::Result<ClientResponse>> {
        match self {
            ResponseSender::OneShot(tx) => tx.send(response).err(),
            ResponseSender::Streaming(tx) => tx.send(response).await.err().map(|e| e.0),
        }
    }

    /// Hand over a reply without waiting for room. Only the streaming arm can be full.
    #[cfg(test)]
    pub(crate) fn try_send(
        self,
        response: error::Result<ClientResponse>,
    ) -> Option<error::Result<ClientResponse>> {
        match self {
            ResponseSender::OneShot(tx) => tx.send(response).err(),
            ResponseSender::Streaming(tx) => tx.try_send(response).err().map(|e| e.into_inner()),
        }
    }
}

/// The receiving half of an operation's reply channel, owned by its `ClientPendingOp`.
#[derive(Debug)]
pub(crate) enum ResponseReceiver {
    /// `None` once the single reply has been read: tokio's one-shot receiver panics if it
    /// is polled again after it has completed.
    OneShot(Option<oneshot::Receiver<error::Result<ClientResponse>>>),
    Streaming(mpsc::Receiver<error::Result<ClientResponse>>),
}

#[derive(Debug)]
pub(crate) struct SenderContext {
    pub sender: ResponseSender,
    pub context: Option<ResponseContext>,
}

impl SenderContext {
    /// Take what is needed to answer one reply for `opaque`, if anyone is still waiting.
    ///
    /// A one-shot channel is consumed by its single reply, so the whole entry leaves the
    /// map with it. A persistent operation keeps its entry -- there are more replies to
    /// come -- and only its sender is cloned.
    fn take_for_reply(
        map: &mut OpaqueMap,
        opaque: u32,
    ) -> Option<(ResponseSender, Option<ResponseContext>)> {
        // One lookup, not two: this runs inside the connection's mutex for every
        // reply, and the one-shot arm -- which used to hash and probe a second
        // time to remove what it had just found -- is the common case.
        let Entry::Occupied(mut entry) = map.entry(opaque) else {
            return None;
        };

        if let ResponseSender::Streaming(sender) = &entry.get().sender {
            return Some((
                ResponseSender::Streaming(sender.clone()),
                entry.get().context.clone(),
            ));
        }

        let entry = entry.remove();
        Some((entry.sender, entry.context))
    }
}

/// A request, encoded and waiting for the wire.
///
/// The opaque and opcode travel with the bytes because the only thing that can fail a
/// write now is the writer, and it has to be able to name the operation it failed.
#[derive(Debug)]
struct WriteFrame {
    buf: BytesMut,
    opaque: u32,
    op_code: OpCode,
}

#[derive(Debug)]
enum WriteCommand {
    Frame(WriteFrame),
    /// Write everything queued ahead of this, then shut the write half down and report
    /// how that went.
    Close(oneshot::Sender<io::Result<()>>),
}

/// Encode buffers, handed back by the writer once their bytes are on the wire.
///
/// Every operation encodes into one of these, so without the pool every operation would
/// allocate one -- and the point of the writer task is to spend fewer syscalls, not more
/// allocations. `tests/allocations.rs` is what holds that line.
#[derive(Debug, Default)]
struct BufferPool {
    buffers: std::sync::Mutex<Vec<BytesMut>>,
}

impl BufferPool {
    /// Enough for a header and a small key, extras and body without growing.
    const INITIAL_CAPACITY: usize = 256;
    /// How many buffers are kept between operations. A connection with more requests than
    /// this in flight at once is not going to notice one allocation.
    const MAX_BUFFERS: usize = 64;
    /// A buffer grown by one large document is dropped rather than kept: pooling it would
    /// hold that much memory per connection for as long as the connection lives.
    const MAX_RETAINED_CAPACITY: usize = 64 * 1024;

    fn take(&self) -> BytesMut {
        self.buffers
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| BytesMut::with_capacity(Self::INITIAL_CAPACITY))
    }

    fn put(&self, mut buf: BytesMut) {
        if buf.capacity() > Self::MAX_RETAINED_CAPACITY {
            return;
        }

        buf.clear();

        let mut buffers = self.buffers.lock().unwrap();
        if buffers.len() < Self::MAX_BUFFERS {
            buffers.push(buf);
        }
    }
}

struct ReadLoopOptions {
    pub client_id: String,
    pub unsolicited_packet_handler: UnsolicitedPacketHandler,
    pub orphan_handler: Option<OrphanResponseHandler>,
    pub on_read_close_handler: OnReadLoopCloseHandler,
    pub on_close_cancel: CancellationToken,
    pub disable_decompression: bool,
    pub local_addr: SocketAddr,
    pub peer_addr: SocketAddr,
    pub closed: Arc<AtomicBool>,
}

#[derive(Debug)]
struct ClientReadHandle {
    read_handle: JoinHandle<()>,
}

impl ClientReadHandle {
    pub async fn await_completion(&mut self) {
        (&mut self.read_handle).await.unwrap_or_default()
    }
}

#[derive(Debug)]
pub struct Client {
    current_opaque: AtomicU32,
    opaque_map: Arc<std::sync::Mutex<OpaqueMap>>,

    client_id: String,

    write_tx: UnboundedSender<WriteCommand>,
    buffer_pool: Arc<BufferPool>,
    on_close_cancel: DropGuard,

    local_addr: SocketAddr,
    peer_addr: SocketAddr,

    closed: Arc<AtomicBool>,
}

impl Client {
    fn register_handler(&self, response_context: SenderContext) -> u32 {
        let mut map = self.opaque_map.lock().unwrap();

        let opaque = self.current_opaque.fetch_add(1, Ordering::SeqCst);

        map.insert(opaque, response_context);

        opaque
    }

    async fn drain_opaque_map(opaque_map: Arc<std::sync::Mutex<OpaqueMap>>) {
        let mut senders = vec![];
        {
            let mut guard = opaque_map.lock().unwrap();
            guard.drain().for_each(|(_, v)| {
                senders.push(v);
            });
        }

        for context in senders {
            // A waiter that has already gone away needs no telling.
            let _ = context
                .sender
                .send(Err(Error::new_cancelled_error(
                    CancellationErrorKind::ClosedInFlight,
                )))
                .await;
        }
    }

    async fn on_read_loop_close(
        client_id: &str,
        stream: FramedRead<ReadHalf<Box<dyn Stream>>, KeyValueCodec>,
        opaque_map: Arc<std::sync::Mutex<OpaqueMap>>,
        on_read_loop_close: OnReadLoopCloseHandler,
        graceful: bool,
    ) {
        drop(stream);

        Self::drain_opaque_map(opaque_map).await;

        // If the client is being shut down, the receiver may have already been dropped
        // (e.g. the owning kvclient is tearing down too), which is expected and not an error.
        if on_read_loop_close.send(()).is_err() && !graceful {
            warn!("{} failed to notify read loop closure", &client_id);
        }

        debug!("{client_id} read loop shut down");
    }

    async fn read_loop(
        mut stream: FramedRead<ReadHalf<Box<dyn Stream>>, KeyValueCodec>,
        opaque_map: Arc<std::sync::Mutex<OpaqueMap>>,
        mut opts: ReadLoopOptions,
    ) {
        // Constructed once rather than per iteration: `cancelled()` builds a
        // future that registers a waker in the token's waiter list on first poll
        // and deregisters on drop, so rebuilding it each time cost two
        // waiter-list operations per response. The branch below returns, so this
        // future completes at most once.
        let cancelled = std::pin::pin!(opts.on_close_cancel.cancelled());
        let mut cancelled = cancelled;

        loop {
            select! {
                (_) = &mut cancelled => {
                    Self::on_read_loop_close(&opts.client_id, stream, opaque_map, opts.on_read_close_handler, true).await;
                    return;
                },
                (next) = stream.next() => {
                    match next {
                        Some(input) => {
                            match input {
                                Ok(mut packet) => {
                                    if packet.magic == Magic::ServerReq {

                                        trace!(
                                            "Handling server request on {}. Opcode={}",
                                            opts.client_id,
                                            packet.op_code,
                                        );

                                        (opts.unsolicited_packet_handler)(packet).await;
                                        continue;
                                    }

                                    trace!(
                                        "Resolving response on {}. Opcode={}. Opaque={}. Status={}",
                                        opts.client_id,
                                        packet.op_code,
                                        packet.opaque,
                                        packet.status,
                                    );

                                    let opaque = packet.opaque;

                                    let waiter = {
                                        let mut map = opaque_map.lock().unwrap();
                                        SenderContext::take_for_reply(&mut map, opaque)
                                    };

                                    if let Some((sender, response_context)) = waiter {
                                        if let Some(value) = &packet.value {
                                            if !opts.disable_decompression && (packet.datatype & u8::from(DataTypeFlag::Compressed) != 0) {
                                                let mut decoder = Decoder::new();
                                                let new_value = match decoder
                                                    .decompress_vec(value)
                                                     {
                                                        Ok(v) => v,
                                                        Err(e) => {
                                                            let _ = sender.send(Err(Error::new_decompression_error().with(e))).await;
                                                         continue;
                                                        }
                                                    };

                                                packet.datatype &= !u8::from(DataTypeFlag::Compressed);
                                                packet.value = Some(Bytes::from(new_value));
                                            }
                                        }

                                        let is_persistent = sender.is_persistent();
                                        let resp = ClientResponse::new(packet, response_context);

                                        if let Some(undelivered) = sender.send(Ok(resp)).await {
                                            // The waiter went away between its reply being routed
                                            // and the reply being handed over. That is one
                                            // operation's business: this used to close the read
                                            // loop, which cancelled every *other* operation on the
                                            // connection because one caller stopped listening.
                                            debug!(
                                                "{} has no waiter left for opaque {}, reporting its response as an orphan",
                                                opts.client_id, opaque,
                                            );

                                            // A persistent operation's entry stays in the map for
                                            // the replies still to come, and with its consumer gone
                                            // nobody will read them either. This does not stop the
                                            // server sending them -- cancelling the operation
                                            // server-side is the streaming caller's job.
                                            if is_persistent {
                                                opaque_map.lock().unwrap().remove(&opaque);
                                            }

                                            if let (Some(orphan_handler), Ok(resp)) = (&opts.orphan_handler, undelivered) {
                                                orphan_handler(
                                                    resp.packet(),
                                                    OrphanContext {
                                                        client_id: opts.client_id.clone(),
                                                        local_addr: opts.local_addr,
                                                        peer_addr: opts.peer_addr,
                                                    },
                                                );
                                            }
                                        }
                                    } else if let Some(ref orphan_handler) = opts.orphan_handler {
                                        orphan_handler(
                                            packet,
                                            OrphanContext {
                                                client_id: opts.client_id.clone(),
                                                local_addr: opts.local_addr,
                                                peer_addr: opts.peer_addr,
                                            },
                                        );
                                    }
                                }
                                Err(e) => {
                                    warn!("{} failed to read frame {}", opts.client_id, e);
                                    let graceful = opts.closed.load(Ordering::SeqCst) || opts.on_close_cancel.is_cancelled();
                                    Self::on_read_loop_close(&opts.client_id, stream, opaque_map, opts.on_read_close_handler, graceful).await;
                                    return;
                                }
                            }
                        }
                        None => {
                            let graceful = opts.closed.load(Ordering::SeqCst) || opts.on_close_cancel.is_cancelled();
                            Self::on_read_loop_close(&opts.client_id, stream, opaque_map, opts.on_read_close_handler, graceful).await;
                            return;
                        }
                    }
                }
            }
        }
    }

    fn split_stream<StreamType: AsyncRead + AsyncWrite + Send + Unpin>(
        stream: StreamType,
    ) -> (ReadHalf<StreamType>, WriteHalf<StreamType>) {
        tokio::io::split(stream)
    }

    /// The only thing that writes to the socket.
    ///
    /// Requests used to be written one syscall at a time, each dispatching task taking the
    /// writer's lock in turn, so a hundred concurrent operations cost a hundred writes.
    /// Here they cost one: take a request, take everything else already queued behind it,
    /// hand the runtime back once so the tasks that were mid-dispatch can queue into the
    /// same batch, take whatever that produced, and write the lot.
    ///
    /// The loop is cbcore-rs's (`src/memdx/rawclient.rs`). What is not is what crosses the
    /// channel: cbcore-rs sends an owned packet and lets the writer's codec encode it,
    /// which copies every document body twice. Here the request is encoded by the
    /// dispatching task -- the one place that can borrow the caller's key and value -- and
    /// the encode buffer itself is what is queued, written, and handed back for reuse.
    async fn write_loop(
        mut stream: WriteHalf<Box<dyn Stream>>,
        mut commands: UnboundedReceiver<WriteCommand>,
        opaque_map: Arc<std::sync::Mutex<OpaqueMap>>,
        buffer_pool: Arc<BufferPool>,
        on_close_cancel: CancellationToken,
        client_id: String,
    ) {
        // Asked once. A TLS stream cannot take a vector of buffers, and that does not
        // change over the life of a connection.
        let vectored = stream.is_write_vectored();

        let mut batch: Vec<WriteFrame> = Vec::new();
        // Only used when the stream cannot take a vector; see `write_batch`.
        let mut coalesced = BytesMut::new();

        while let Some(command) = commands.recv().await {
            let mut close_ack = match command {
                WriteCommand::Frame(frame) => {
                    batch.push(frame);
                    None
                }
                WriteCommand::Close(ack) => Some(ack),
            };

            if close_ack.is_none() {
                // Everything queued while the previous batch was being written.
                let mut batched = false;
                while let Ok(command) = commands.try_recv() {
                    match command {
                        WriteCommand::Frame(frame) => {
                            batched = true;
                            batch.push(frame);
                        }
                        WriteCommand::Close(ack) => {
                            close_ack = Some(ack);
                            break;
                        }
                    }
                }

                if batched && close_ack.is_none() {
                    // There is concurrency here to exploit, so give the runtime back once:
                    // a task that is about to queue a request joins this batch rather than
                    // paying for a syscall of its own. With nothing else in flight this is
                    // skipped, because then the yield is pure added latency.
                    tokio::task::yield_now().await;

                    while let Ok(command) = commands.try_recv() {
                        match command {
                            WriteCommand::Frame(frame) => batch.push(frame),
                            WriteCommand::Close(ack) => {
                                close_ack = Some(ack);
                                break;
                            }
                        }
                    }
                }
            }

            let written = Self::write_batch(&mut stream, &batch, vectored, &mut coalesced).await;

            if let Err(e) = written {
                debug!(
                    "{} failed to write a batch of {} requests: {}",
                    client_id,
                    batch.len(),
                    e
                );

                // Nothing more can be queued from here. A `dispatch` racing this is told
                // on the spot and unregisters its own opaque, where a request accepted by
                // a channel about to be dropped would have waited for a reply that nobody
                // was left to fail.
                commands.close();

                // Every request in the batch, and every request queued behind it, is
                // answered with the failure. Those still queued were never written at all;
                // the batch itself may have been written in part, which is the same hazard
                // a failed write of a single request always carried.
                for frame in batch.drain(..) {
                    Self::fail_frame(&opaque_map, &frame, &e).await;
                    buffer_pool.put(frame.buf);
                }

                while let Ok(command) = commands.try_recv() {
                    match command {
                        WriteCommand::Frame(frame) => {
                            Self::fail_frame(&opaque_map, &frame, &e).await;
                            buffer_pool.put(frame.buf);
                        }
                        WriteCommand::Close(ack) => {
                            let _ = ack.send(Err(io::Error::new(e.kind(), e.to_string())));
                        }
                    }
                }

                if let Some(ack) = close_ack {
                    let _ = ack.send(Err(e));
                }

                // A connection that cannot be written to is finished. Taking the read loop
                // down with it is what cancels the operations still awaiting replies and
                // tells the pool to stop handing this client out.
                on_close_cancel.cancel();
                return;
            }

            for frame in batch.drain(..) {
                buffer_pool.put(frame.buf);
            }

            if let Some(ack) = close_ack {
                // Closed before the shutdown for the same reason as the failure path: from
                // here a `dispatch` is refused rather than accepted and forgotten.
                commands.close();
                let shutdown = stream.shutdown().await;

                let closing = io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the connection was closed before the request was written",
                );
                while let Ok(command) = commands.try_recv() {
                    if let WriteCommand::Frame(frame) = command {
                        Self::fail_frame(&opaque_map, &frame, &closing).await;
                        buffer_pool.put(frame.buf);
                    }
                }

                let _ = ack.send(shutdown);
                return;
            }
        }

        // The channel closed, so the client is gone. Dropping the write half here is what
        // closes the socket.
    }

    /// Write one batch, in one syscall where the stream allows one.
    async fn write_batch(
        stream: &mut WriteHalf<Box<dyn Stream>>,
        batch: &[WriteFrame],
        vectored: bool,
        coalesced: &mut BytesMut,
    ) -> io::Result<()> {
        match batch.len() {
            0 => return Ok(()),
            1 => return stream.write_all(&batch[0].buf).await,
            _ => {}
        }

        if !vectored {
            // A stream that cannot take a vector -- TLS -- would otherwise turn the batch
            // back into one write per request, so the frames are joined first. That is a
            // second copy of each request, and it buys the batch its single write on the
            // one kind of connection that copies the plaintext again anyway.
            coalesced.clear();
            for frame in batch {
                coalesced.extend_from_slice(&frame.buf);
            }

            return stream.write_all(coalesced).await;
        }

        let mut slices: Vec<IoSlice> = batch.iter().map(|f| IoSlice::new(&f.buf)).collect();
        let mut slices = slices.as_mut_slice();

        while !slices.is_empty() {
            let written = stream.write_vectored(slices).await?;
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "the connection took none of the batch",
                ));
            }

            IoSlice::advance_slices(&mut slices, written);
        }

        Ok(())
    }

    /// Answer one operation with the write failure that stopped it.
    ///
    /// The kind stays `Dispatch`, because that is what it is and what the layer above
    /// reads to decide the operation can be retried on a different connection.
    async fn fail_frame(
        opaque_map: &Arc<std::sync::Mutex<OpaqueMap>>,
        frame: &WriteFrame,
        cause: &io::Error,
    ) {
        let waiter = { opaque_map.lock().unwrap().remove(&frame.opaque) };

        // No waiter means the caller has already given up on it.
        let Some(waiter) = waiter else {
            return;
        };

        // `io::Error` is not `Clone`, and a batch has one failure between all of it.
        let cause = io::Error::new(cause.kind(), cause.to_string());

        let _ = waiter
            .sender
            .send(Err(Error::new_dispatch_error(
                frame.opaque,
                frame.op_code,
                Box::new(Error::from(cause)),
            )))
            .await;
    }
}

impl Dispatcher for Client {
    fn new(conn: ConnectionType, opts: DispatcherOptions) -> Self {
        let local_addr = *conn.local_addr();
        let peer_addr = *conn.peer_addr();

        let (r, w) = tokio::io::split(conn.into_inner());

        let codec = KeyValueCodec::default();
        let reader = FramedRead::new(r, codec);

        let cancel_token = CancellationToken::new();
        let cancel_child = cancel_token.child_token();
        // Kept for the writer, which cancels the read loop when the connection can no
        // longer be written to. Cloned before the guard, which consumes the token.
        let write_cancel = cancel_token.clone();
        let cancel_guard = cancel_token.drop_guard();

        let opaque_map = Arc::new(std::sync::Mutex::new(OpaqueMap::default()));

        let read_opaque_map = Arc::clone(&opaque_map);
        let read_uuid = opts.id.clone();

        let closed = Arc::new(AtomicBool::new(false));
        let read_closed = Arc::clone(&closed);

        let buffer_pool = Arc::new(BufferPool::default());
        let (write_tx, write_rx) = unbounded_channel();

        let write_opaque_map = Arc::clone(&opaque_map);
        let write_buffer_pool = Arc::clone(&buffer_pool);
        let write_uuid = opts.id.clone();

        tokio::spawn(async move {
            Client::write_loop(
                w,
                write_rx,
                write_opaque_map,
                write_buffer_pool,
                write_cancel,
                write_uuid,
            )
            .await;
        });

        tokio::spawn(async move {
            Client::read_loop(
                reader,
                read_opaque_map,
                ReadLoopOptions {
                    client_id: read_uuid,
                    unsolicited_packet_handler: opts.unsolicited_packet_handler,
                    orphan_handler: opts.orphan_handler,
                    on_read_close_handler: opts.on_read_close_tx,
                    on_close_cancel: cancel_child,
                    disable_decompression: opts.disable_decompression,
                    local_addr,
                    peer_addr,
                    closed: read_closed,
                },
            )
            .await;
        });

        Self {
            current_opaque: AtomicU32::new(1),
            opaque_map,
            client_id: opts.id,

            on_close_cancel: cancel_guard,

            write_tx,
            buffer_pool,

            local_addr,
            peer_addr,

            closed,
        }
    }

    async fn dispatch<'a>(
        &self,
        mut packet: RequestPacket<'a>,
        is_persistent: bool,
        response_context: Option<ResponseContext>,
    ) -> error::Result<ClientPendingOp> {
        let (response_tx, response_rx) = ResponseSender::new_pair(is_persistent);

        let opaque = self.register_handler(SenderContext {
            sender: response_tx,
            context: response_context,
        });
        packet.opaque = Some(opaque);
        let op_code = packet.op_code;

        trace!(
            "Writing request on {}. Opcode={}. Opaque={}",
            &self.client_id,
            packet.op_code,
            opaque,
        );

        // Encoded here, on the task that owns the key and the value the packet borrows,
        // and queued as bytes. There is no await between registering the opaque and
        // queueing it, so nothing can drop this future in between and leave the entry
        // behind -- which is what the guard that used to live here was for.
        let mut buf = self.buffer_pool.take();
        if let Err(e) = KeyValueCodec::encode_request(packet, &mut buf) {
            self.opaque_map.lock().unwrap().remove(&opaque);
            self.buffer_pool.put(buf);

            return Err(Error::new_dispatch_error(opaque, op_code, Box::new(e)));
        }

        if let Err(e) = self.write_tx.send(WriteCommand::Frame(WriteFrame {
            buf,
            opaque,
            op_code,
        })) {
            debug!(
                "{} could not queue packet {} {}: the writer has stopped",
                self.client_id, opaque, op_code
            );

            self.opaque_map.lock().unwrap().remove(&opaque);
            if let WriteCommand::Frame(frame) = e.0 {
                self.buffer_pool.put(frame.buf);
            }

            return Err(Error::new_dispatch_error(
                opaque,
                op_code,
                Box::new(Error::from(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the connection is no longer writable",
                ))),
            ));
        }

        Ok(ClientPendingOp::new(
            opaque,
            self.opaque_map.clone(),
            response_rx,
        ))
    }

    async fn close(&self) -> error::Result<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }

        info!("Closing client {}", self.client_id);

        let (ack_tx, ack_rx) = oneshot::channel();

        // The writer shuts the socket down once everything queued ahead of the close is on
        // the wire, and answers when it has. A writer that is already gone -- because a
        // write failed -- has nothing left to close.
        let close_err = if self.write_tx.send(WriteCommand::Close(ack_tx)).is_ok() {
            ack_rx.await.unwrap_or(Ok(())).err()
        } else {
            None
        };

        Self::drain_opaque_map(self.opaque_map.clone()).await;

        if let Some(e) = close_err {
            return Err(Error::new_close_error(
                e.to_string(),
                Box::new(Error::from(e)),
            ));
        }

        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        info!("Dropping client {}", self.client_id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use futures::future::BoxFuture;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::Instant;

    use super::*;
    use crate::address::Address;
    use crate::memdx::codec::HEADER_SIZE;
    use crate::memdx::connection::{ConnectOptions, TcpConnection};
    use crate::memdx::magic::Magic;
    use crate::memdx::opcode::OpCode;

    /// The 24 bytes of a body-less success response.
    fn response_header(op_code: OpCode, opaque: u32) -> [u8; HEADER_SIZE] {
        let mut header = [0u8; HEADER_SIZE];
        header[0] = Magic::Res.into();
        header[1] = op_code.into();
        header[12..16].copy_from_slice(&opaque.to_be_bytes());
        header
    }

    /// Reads one request off the wire and answers nothing, returning its opaque.
    async fn read_request(socket: &mut TcpStream) -> u32 {
        let mut header = [0u8; HEADER_SIZE];
        socket.read_exact(&mut header).await.unwrap();

        let body_len = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
        if body_len > 0 {
            let mut body = vec![0u8; body_len];
            socket.read_exact(&mut body).await.unwrap();
        }

        u32::from_be_bytes(header[12..16].try_into().unwrap())
    }

    /// A client on a real socket, with the orphans it reports and its read-loop closure.
    async fn connect_client(
        addr: std::net::SocketAddr,
    ) -> (Client, Arc<Mutex<Vec<u32>>>, oneshot::Receiver<()>) {
        let conn = TcpConnection::connect(
            Address {
                host: addr.ip().to_string(),
                port: addr.port(),
            },
            ConnectOptions {
                deadline: Instant::now() + Duration::from_secs(5),
                tcp_keep_alive_time: Duration::from_secs(30),
            },
        )
        .await
        .unwrap();

        let orphans = Arc::new(Mutex::new(Vec::new()));
        let reported = Arc::clone(&orphans);
        let (on_read_close_tx, on_read_close_rx) = oneshot::channel();

        let client = Client::new(
            ConnectionType::Tcp(conn),
            DispatcherOptions {
                unsolicited_packet_handler: Arc::new(|_packet| {
                    Box::pin(async {}) as BoxFuture<'static, ()>
                }),
                orphan_handler: Some(Arc::new(move |packet: ResponsePacket, _ctx| {
                    reported.lock().unwrap().push(packet.opaque)
                })),
                on_read_close_tx,
                disable_decompression: false,
                id: "test-client".to_string(),
            },
        );

        (client, orphans, on_read_close_rx)
    }

    /// Answers every request it can see, one write per read.
    async fn serve_requests(mut socket: TcpStream) {
        let mut chunk = vec![0u8; 64 * 1024];
        let mut pending: Vec<u8> = Vec::new();

        loop {
            let read = match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            pending.extend_from_slice(&chunk[..read]);

            let mut out: Vec<u8> = Vec::new();
            let mut consumed = 0;
            while pending.len() - consumed >= HEADER_SIZE {
                let header = &pending[consumed..consumed + HEADER_SIZE];
                let body_len = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
                if pending.len() - consumed < HEADER_SIZE + body_len {
                    break;
                }

                let opaque = u32::from_be_bytes(header[12..16].try_into().unwrap());
                out.extend_from_slice(&response_header(OpCode::Noop, opaque));
                consumed += HEADER_SIZE + body_len;
            }
            pending.drain(..consumed);

            if !out.is_empty() && socket.write_all(&out).await.is_err() {
                return;
            }
        }
    }

    /// Requests written as one batch are still whole requests, in order, each answered to
    /// the operation that asked for it.
    #[tokio::test]
    async fn a_batch_of_concurrent_requests_all_come_back() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            serve_requests(socket).await;
        });

        let (client, _orphans, _on_read_close_rx) = connect_client(addr).await;

        // Bodies of different sizes, so a batch written as one vector would show up as
        // garbage on the server if the frames were not each written whole.
        let ops = (0..64usize).map(|i| {
            let client = &client;
            async move {
                let key = vec![b'k'; 1 + i];
                let value = vec![b'v'; 1 + i * 7];

                let packet = RequestPacket::new(Magic::Req, OpCode::Set, 0)
                    .extras(&[0u8; 8])
                    .key(&key)
                    .value(&value);

                let mut op = client.dispatch(packet, false, None).await.unwrap();
                op.recv().await.map(|resp| resp.packet().opaque)
            }
        });

        let answered: Vec<u32> = futures::future::join_all(ops)
            .await
            .into_iter()
            .map(|r| r.expect("every request in the batch should be answered"))
            .collect();

        let mut sorted = answered.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            64,
            "each operation should have been answered its own reply, got {answered:?}"
        );
    }

    /// A connection that cannot be written to reports that through `recv`, not from
    /// `dispatch`.
    ///
    /// This is the behaviour change the writer task brings: `dispatch` queues, so by the
    /// time a write fails its caller has already been handed a pending operation.
    #[tokio::test]
    async fn a_write_that_cannot_land_is_reported_to_the_operation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            // Gone before a single request is read.
            drop(socket);
        });

        let (client, _orphans, _on_read_close_rx) = connect_client(addr).await;
        server.await.unwrap();

        // The socket does not refuse a write until the peer's reset has been seen, so this
        // takes a couple of attempts. What it must never do is answer one.
        let mut dispatched_onto_a_dead_connection = false;
        for _ in 0..20 {
            let packet = RequestPacket::new(Magic::Req, OpCode::Noop, 0);

            let mut op = match client.dispatch(packet, false, None).await {
                Ok(op) => op,
                // Once the writer has stopped, dispatch fails on the spot again.
                Err(e) => {
                    assert!(e.is_dispatch_error(), "unexpected dispatch error: {e}");
                    assert!(
                        dispatched_onto_a_dead_connection,
                        "the first dispatch onto a dead connection should still have been accepted"
                    );
                    return;
                }
            };
            dispatched_onto_a_dead_connection = true;

            match tokio::time::timeout(Duration::from_millis(500), op.recv()).await {
                Ok(Ok(_)) => panic!("a closed connection answered an operation"),
                Ok(Err(_)) => return,
                Err(_) => continue,
            }
        }

        panic!("a dead connection never reported a failure");
    }

    /// A reply for an operation whose caller has gone away must not take the connection --
    /// and every other operation on it -- down with it.
    ///
    /// The read loop used to close on a failed hand-over, so one caller that stopped
    /// listening cancelled everything else in flight on the same socket.
    #[tokio::test]
    async fn a_reply_nobody_is_waiting_for_does_not_close_the_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (abandoned_tx, abandoned_rx) = oneshot::channel::<u32>();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            // Answer the operation whose waiter has already gone...
            let abandoned = abandoned_rx.await.unwrap();
            socket
                .write_all(&response_header(OpCode::Noop, abandoned))
                .await
                .unwrap();

            // ...and then a perfectly ordinary one, on the same connection.
            let opaque = read_request(&mut socket).await;
            socket
                .write_all(&response_header(OpCode::Noop, opaque))
                .await
                .unwrap();

            // Held open until the test is done with it.
            socket
        });

        let (client, orphans, mut on_read_close_rx) = connect_client(addr).await;

        // An opaque whose receiver is already gone is the race this is about: the read
        // loop takes the entry out of the map to answer it, and by the time it does the
        // caller has dropped its `ClientPendingOp`.
        let (sender, receiver) = ResponseSender::new_pair(false);
        drop(receiver);
        let abandoned = client.register_handler(SenderContext {
            sender,
            context: None,
        });
        abandoned_tx.send(abandoned).unwrap();

        let mut op = client
            .dispatch(RequestPacket::new(Magic::Req, OpCode::Noop, 0), false, None)
            .await
            .unwrap();

        let response = op
            .recv()
            .await
            .expect("the connection should still be answering");
        assert_eq!(response.packet().op_code, OpCode::Noop);

        assert_eq!(
            orphans.lock().unwrap().as_slice(),
            [abandoned],
            "the reply that could not be handed over should have been reported as an orphan"
        );
        assert!(
            matches!(
                on_read_close_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "the read loop should still be running"
        );

        drop(client);
        let _ = server.await;
    }
}
