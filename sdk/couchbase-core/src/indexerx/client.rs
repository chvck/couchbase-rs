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

//! One authenticated connection to an indexer's scan port.
//!
//! Queryport does not multiplex, so this is deliberately not a
//! [`Dispatcher`](crate::memdx::dispatcher::Dispatcher): a `Client` holds a
//! framed socket and serves one exchange at a time. Every operation takes
//! `&mut self`, so a second request cannot begin while one is outstanding, and
//! [`Client::scan`] takes the client **by value** because a stream owns its
//! connection until it ends (see [`super::scan`]).
//!
//! ### The terminator, which is not obvious
//!
//! Every exchange *except* authentication is terminated by a
//! `StreamEndResponse` — including the ones that look like plain
//! request/response. `Helo` and `Count` each get their answer **and then** a
//! terminator, because the server runs them through the same
//! `ScanResponseWriter::Done` as a scan (`indexer/scan_protocol.go`). A client
//! that reads only the answer leaves the terminator in the socket, and the next
//! request on that connection reads it as its own reply.
//!
//! Which terminator arrives depends on the version we report:
//!
//! | reported client version | terminator |
//! |---|---|
//! | `< 9` (pre-7.6) | a zero-length end-of-response frame |
//! | `>= 9` | `StreamEndResponse`, and **no** end frame |
//!
//! We report a current version, so it is always `StreamEndResponse`. Both are
//! accepted anyway: accepting the other costs one match arm, and rejecting it
//! would break against an older cluster for no gain.
//!
//! Authentication is the exception — the server answers it before entering the
//! request loop that appends a terminator.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt, Stream, StreamExt};
use tokio::time::Instant;
use tokio_util::codec::Framed;

use crate::address::Address;
// The transport, and only the transport. `indexerx` is a peer of `memdx` and
// shares none of its protocol, but "open a socket, maybe upgrade it to TLS,
// with a deadline and a keepalive" is not protocol — reimplementing it here
// would mean a second TLS path to keep in step with `tls_config`, and the two
// would drift.
use crate::memdx::connection::{
    ConnectOptions as ConnectionOptions, ConnectionType, Stream as ConnectionStream, TcpConnection,
    TlsConnection,
};
use crate::tls_config::TlsConfig;

use super::error::{AuthCode, Error};
use super::packet_codec::{Frame, PacketCodec, DEFAULT_MAX_PAYLOAD};
use super::proto::{self, Consistency, ParseError, Request, Response, ScanOptions};
use super::scan::ScanStream;

/// The client version reported during authentication.
///
/// This is the indexer's own version *enum* — `INDEXER_81_VERSION = 11` in
/// `common/const.go` — and not a product version. It is not cosmetic: the
/// server changes what it sends based on it, and reporting anything below 9
/// switches the terminator from `StreamEndResponse` to a bare end frame.
///
/// It moves when `query.proto` moves and not before, because it is a claim
/// about what this client can parse.
const CLIENT_VERSION: u32 = 11;

/// Matches `KvClientConfig`'s default, because there is no reason for the
/// indexer to be given longer than the data service.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Matches the agent's default. A scan connection sits idle in a pool between
/// scans exactly as a KV one does, so it wants the same keepalive.
const DEFAULT_TCP_KEEP_ALIVE_TIME: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub username: String,
    pub password: String,
    /// The ceiling on a single response frame. The server's batch size is
    /// configurable, so this is too.
    pub max_payload: usize,

    /// `Some` to speak TLS to the queryport, `None` for a plain socket.
    ///
    /// **The port does not change.** `nodeServices` advertises `indexScan` and
    /// no `indexScanSSL`, so a TLS scan is the same port with TLS on top of it
    /// — see [`crate::cbconfig::TerseExtNodePorts::index_scan`]. The caller
    /// therefore decides the transport, and the config cannot decide it for
    /// them by offering a different port.
    pub tls_config: Option<TlsConfig>,

    pub connect_timeout: Duration,
    pub tcp_keep_alive_time: Duration,
}

impl ConnectOptions {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        ConnectOptions {
            username: username.into(),
            password: password.into(),
            max_payload: DEFAULT_MAX_PAYLOAD,
            tls_config: None,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            tcp_keep_alive_time: DEFAULT_TCP_KEEP_ALIVE_TIME,
        }
    }

    pub fn with_tls_config(mut self, tls_config: Option<TlsConfig>) -> Self {
        self.tls_config = tls_config;
        self
    }
}

pub struct Client {
    /// Boxed because the transport is chosen at connect time and the protocol
    /// above it does not care which it got. One indirection per frame read,
    /// against a syscall — not worth a generic parameter spreading through
    /// [`ScanStream`] and everything that holds one.
    framed: Framed<Box<dyn ConnectionStream>, PacketCodec>,
    server_version: u32,
}

impl Client {
    /// Connect, authenticate, and exchange versions.
    ///
    /// Nodelay and the keepalive come from
    /// [`TcpConnection`](crate::memdx::connection::TcpConnection), which sets
    /// both: one small write followed by a long read is the shape of every
    /// exchange here, so Nagle would add an ack's latency to each one — the
    /// same reasoning that put it there for KV.
    pub async fn connect(addr: &Address, opts: &ConnectOptions) -> Result<Client, Error> {
        let connection_opts = ConnectionOptions {
            deadline: Instant::now() + opts.connect_timeout,
            tcp_keep_alive_time: opts.tcp_keep_alive_time,
        };

        let connection = match &opts.tls_config {
            Some(tls_config) => ConnectionType::Tls(
                TlsConnection::connect(addr.clone(), tls_config.clone(), connection_opts)
                    .await
                    .map_err(|e| {
                        Error::new_connect_error("failed to establish TLS to the queryport", e)
                    })?,
            ),
            None => ConnectionType::Tcp(
                TcpConnection::connect(addr.clone(), connection_opts)
                    .await
                    .map_err(|e| {
                        Error::new_connect_error("failed to open a socket to the queryport", e)
                    })?,
            ),
        };

        let mut client = Client {
            framed: Framed::new(connection.into_inner(), PacketCodec::new(opts.max_payload)),
            server_version: 0,
        };

        client.authenticate(opts).await?;
        client.server_version = client.helo().await?;

        Ok(client)
    }

    /// The version the server reported at `Helo`.
    ///
    /// **The indexer's own version enum, on the same scale as
    /// [`CLIENT_VERSION`]** — `HeloResponse.version` is `INDEXER_CUR_VERSION`,
    /// not the protobuf version, so the two are directly comparable. Measured
    /// against Couchbase 8.0, which reports 10.
    ///
    /// Reporting a *higher* version than the server is normal and is what the
    /// Go client does too: it always sends the version of the build it is part
    /// of. The server uses it to decide what it may send us, and every check is
    /// a lower bound.
    ///
    /// Zero means Couchbase 4.0.x, which predates the enum and is the only
    /// release needing a client-supplied consistency vector for a
    /// `request_plus` scan. We do not support it; the accessor exists so a
    /// caller can say so rather than silently producing a stale read.
    pub fn server_version(&self) -> u32 {
        self.server_version
    }

    /// A client over an in-memory pipe with nothing on the far end.
    ///
    /// For the layers above that need *a* connection to move around rather than
    /// one to talk to — [`indexerclient_provider`](crate::indexerclient_provider)
    /// is about which connection goes where, and its tests would otherwise need
    /// a server and a runtime to ask that question. `server_version` is how such
    /// a test tells two of them apart.
    #[cfg(test)]
    pub(crate) fn for_test(server_version: u32) -> Client {
        // One byte of buffer: nothing is ever written, and a test that did write
        // should block rather than look like it succeeded.
        let (stream, _far_end) = tokio::io::duplex(1);
        Client {
            framed: Framed::new(
                Box::new(stream) as Box<dyn ConnectionStream>,
                PacketCodec::new(DEFAULT_MAX_PAYLOAD),
            ),
            server_version,
        }
    }

    /// Count the entries in an index.
    ///
    /// The whole index — spans belong to [`Client::scan`]. `partitions` is the
    /// set this node holds, and is empty for a non-partitioned index.
    ///
    /// One exchange, no stream, which makes it the cheapest end-to-end proof
    /// that framing, the payload wrapper, auth and the error path all work
    /// against a real cluster.
    pub async fn count(
        &mut self,
        defn_id: u64,
        consistency: Consistency,
        partitions: Vec<u64>,
    ) -> Result<i64, Error> {
        match self
            .exchange(
                Request::count_all(defn_id, consistency, partitions),
                "CountResponse",
            )
            .await?
        {
            Response::Count(count) => Ok(count),
            _ => unreachable!("expect_kind admitted a non-count response"),
        }
    }

    /// Start a scan, taking ownership of the connection for its duration.
    ///
    /// The connection comes back from [`ScanStream::finish`], and only after
    /// the stream has been drained — see [`super::scan`] for why that is
    /// enforced by ownership rather than by a rule.
    pub async fn scan(mut self, opts: &ScanOptions) -> Result<ScanStream, Error> {
        // If the request cannot even be sent there is no stream to own the
        // connection, so the client is dropped here along with it.
        self.send(Request::scan(opts)).await?;
        Ok(ScanStream::new(self))
    }

    /// **Always the first packet on a connection.**
    ///
    /// A server that wants credentials from a connection that offered none
    /// replies `AUTH_MISSING` and closes it. The Go client treats that as
    /// "reconnect with auth" because it has connections predating the cluster's
    /// upgrade; we authenticate unconditionally, so `Missing` here means we
    /// have a bug and is reported rather than retried.
    async fn authenticate(&mut self, opts: &ConnectOptions) -> Result<(), Error> {
        self.send(Request::auth(
            &opts.username,
            &opts.password,
            CLIENT_VERSION,
        ))
        .await?;

        match self.read_response().await?.expect_kind("AuthResponse")? {
            Response::Auth(AuthCode::Success) => Ok(()),
            Response::Auth(code) => Err(Error::new_authentication_error(code)),
            _ => unreachable!("expect_kind admitted a non-auth response"),
        }
    }

    async fn helo(&mut self) -> Result<u32, Error> {
        match self.exchange(Request::helo(), "HeloResponse").await? {
            Response::Helo { version } => Ok(version),
            _ => unreachable!("expect_kind admitted a non-helo response"),
        }
    }

    /// Send one request, read one response, and consume the terminator.
    ///
    /// Correct only for the exchanges that are genuinely request/response. A
    /// scan streams and uses [`Client::scan`], because reading a single frame
    /// from a stream would leave the connection holding rows that the next
    /// caller would read as its own.
    async fn exchange(
        &mut self,
        request: Request,
        expected: &'static str,
    ) -> Result<Response, Error> {
        self.send(request).await?;
        let response = self.read_response().await?.expect_kind(expected)?;
        self.consume_terminator().await?;
        Ok(response)
    }

    /// Read the frame that ends an exchange, and insist it is one.
    ///
    /// Anything else means the connection is out of step — worth an error even
    /// though the answer is already in hand, because the alternative is
    /// returning a correct value on a connection that will corrupt the next
    /// caller's.
    async fn consume_terminator(&mut self) -> Result<(), Error> {
        match self.read_frame().await? {
            Frame::EndOfResponse => Ok(()),
            Frame::Payload(payload) => match proto::decode_response(payload)? {
                Response::StreamEnd { .. } => Ok(()),
                other => Err(ParseError::UnexpectedMessage {
                    expected: "StreamEndResponse",
                    got: other.kind(),
                }
                .into()),
            },
        }
    }

    pub(super) async fn send_end_stream(&mut self) -> Result<(), Error> {
        self.send(Request::end_stream()).await
    }

    async fn send(&mut self, request: Request) -> Result<(), Error> {
        self.framed
            .send(Frame::Payload(Bytes::from(request.encode())))
            .await?;
        Ok(())
    }

    /// Read the next frame and parse it, rejecting a terminator.
    ///
    /// A terminator where a response belongs means the server finished without
    /// answering, which is a desynchronised connection rather than an empty
    /// result.
    async fn read_response(&mut self) -> Result<Response, Error> {
        match self.read_frame().await? {
            Frame::Payload(payload) => proto::decode_response(payload),
            Frame::EndOfResponse => Err(Error::new_unexpected_eof_error()),
        }
    }

    pub(super) async fn read_frame(&mut self) -> Result<Frame, Error> {
        match self.framed.next().await {
            Some(Ok(frame)) => Ok(frame),
            Some(Err(e)) => Err(e.into()),
            None => Err(Error::new_unexpected_eof_error()),
        }
    }

    /// The poll-based counterpart of [`Client::read_frame`], for the stream.
    ///
    /// `Ok(None)` is a closed socket, which the caller decides how to read —
    /// for a stream it is a truncated answer and therefore an error.
    pub(super) fn poll_frame(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<Frame>, Error>> {
        match Pin::new(&mut self.framed).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(Ok(None)),
            Poll::Ready(Some(Ok(frame))) => Poll::Ready(Ok(Some(frame))),
            Poll::Ready(Some(Err(e))) => Poll::Ready(Err(e.into())),
        }
    }
}
