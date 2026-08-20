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

//! How many small operations one memcached connection can turn over.
//!
//! The server here is a loopback socket that answers every request with a
//! header-only success, and that answers a whole batch of requests with one
//! write. That is deliberate: it costs almost nothing, so what the numbers move
//! with is the client's own per-operation cost — encoding, correlation, and how
//! many write syscalls a batch of concurrent operations turns into.
//!
//! A real cluster cannot measure that. The nearest one is four shared nodes over
//! a network whose round trip is three orders of magnitude larger than the thing
//! being changed, and shared with whatever else is running.
//!
//! The concurrency sweep is the point of the shape: at concurrency 1 there is
//! never a second request to batch with, so it is the control that says the
//! change costs nothing when there is nothing to gain; the higher rows are where
//! a writer that coalesces has anything to coalesce.

// The KV dispatch path is a deep async stack, and rustc's layout queries go
// deeper than the default limit through it. See the crate root.
#![recursion_limit = "256"]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use couchbase_core::address::Address;
use couchbase_core::memdx::client::Client;
use couchbase_core::memdx::connection::{ConnectOptions, ConnectionType, TcpConnection};
use couchbase_core::memdx::dispatcher::{Dispatcher, DispatcherOptions};
use couchbase_core::memdx::magic::Magic;
use couchbase_core::memdx::opcode::OpCode;
use couchbase_core::memdx::packet::{RequestPacket, ResponsePacket};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use futures::future::{join_all, BoxFuture};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::time::Instant;

const HEADER_SIZE: usize = 24;

/// Operations per measured iteration, split across the concurrency being swept.
const OPS_PER_ITERATION: usize = 512;

fn response_header(op_code: u8, opaque: u32) -> [u8; HEADER_SIZE] {
    let mut header = [0u8; HEADER_SIZE];
    header[0] = 0x81; // Magic::Res
    header[1] = op_code;
    header[12..16].copy_from_slice(&opaque.to_be_bytes());
    header
}

/// Answers every request it can see with a success, one write per read.
async fn serve(mut socket: TcpStream) {
    socket.set_nodelay(true).unwrap();

    let mut chunk = vec![0u8; 64 * 1024];
    let mut pending: Vec<u8> = Vec::new();
    let mut out: Vec<u8> = Vec::new();

    loop {
        let read = match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        pending.extend_from_slice(&chunk[..read]);

        out.clear();
        let mut consumed = 0;
        while pending.len() - consumed >= HEADER_SIZE {
            let header = &pending[consumed..consumed + HEADER_SIZE];
            let body_len = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
            if pending.len() - consumed < HEADER_SIZE + body_len {
                break;
            }

            let opaque = u32::from_be_bytes(header[12..16].try_into().unwrap());
            out.extend_from_slice(&response_header(header[1], opaque));
            consumed += HEADER_SIZE + body_len;
        }
        pending.drain(..consumed);

        if !out.is_empty() && socket.write_all(&out).await.is_err() {
            return;
        }
    }
}

/// The server runs on its own thread and its own runtime, so it is not competing
/// with the client for the runtime being measured.
fn spawn_server() -> SocketAddr {
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            addr_tx.send(listener.local_addr().unwrap()).unwrap();

            loop {
                let (socket, _) = listener.accept().await.unwrap();
                tokio::spawn(serve(socket));
            }
        });
    });

    addr_rx.recv().unwrap()
}

async fn connect(addr: SocketAddr) -> (Arc<Client>, oneshot::Receiver<()>) {
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

    let (on_read_close_tx, on_read_close_rx) = oneshot::channel();

    let client = Client::new(
        ConnectionType::Tcp(conn),
        DispatcherOptions {
            unsolicited_packet_handler: Arc::new(|_packet: ResponsePacket| {
                Box::pin(async {}) as BoxFuture<'static, ()>
            }),
            orphan_handler: None,
            on_read_close_tx,
            disable_decompression: false,
            id: "bench-client".to_string(),
        },
    );

    (Arc::new(client), on_read_close_rx)
}

/// One small write: 8 bytes of extras, a 16 byte key, a 32 byte body.
async fn one_op(client: &Client, key: &[u8], value: &[u8]) {
    let packet = RequestPacket::new(Magic::Req, OpCode::Set, 0)
        .extras(&[0u8; 8])
        .key(key)
        .value(value)
        .vbucket_id(0);

    let mut op = client.dispatch(packet, false, None).await.unwrap();
    op.recv().await.unwrap();
}

fn pipeline(c: &mut Criterion) {
    let addr = spawn_server();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let (client, _on_read_close) = runtime.block_on(connect(addr));

    let mut group = c.benchmark_group("kv_pipeline");
    group.throughput(Throughput::Elements(OPS_PER_ITERATION as u64));

    for concurrency in [1usize, 8, 64, 256] {
        let per_task = OPS_PER_ITERATION / concurrency;

        group.bench_with_input(
            BenchmarkId::from_parameter(concurrency),
            &concurrency,
            |b, &concurrency| {
                let client = Arc::clone(&client);
                b.to_async(&runtime).iter(|| {
                    let client = Arc::clone(&client);
                    async move {
                        join_all((0..concurrency).map(|task| {
                            let client = Arc::clone(&client);
                            async move {
                                let key = [b'k'; 16];
                                let value = [b'v'; 32];
                                for _ in 0..per_task {
                                    one_op(&client, &key, &value).await;
                                }
                                task
                            }
                        }))
                        .await
                    }
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default()
        .sample_size(50)
        .warm_up_time(Duration::from_secs(3))
        .measurement_time(Duration::from_secs(10));
    targets = pipeline
);
criterion_main!(benches);
