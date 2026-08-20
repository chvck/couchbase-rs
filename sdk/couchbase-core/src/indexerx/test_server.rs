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

//! A scripted queryport server, for testing the client over a real socket.
//!
//! Everything below the socket is exercised for real — framing, checksums, the
//! payload wrapper, the terminator rules — while needing no cluster. The pieces
//! that only a cluster can settle (does the *indexer* agree with our span
//! encoding, does it tolerate a slow reader) are integration tests and are not
//! this.
//!
//! The server is scripted rather than behavioural: a test says what frames to
//! send in reply to the scan, which keeps the interesting cases — an error
//! mid-stream, a batch boundary in an awkward place, a missing terminator —
//! expressible without writing an indexer.

use std::sync::Arc;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_util::codec::Framed;

use crate::address::Address;

use super::packet_codec::{Frame, PacketCodec};
use super::proto::fake;

/// What the server should do when the scan arrives.
#[derive(Debug, Clone)]
pub(super) struct Script {
    /// Frames sent, in order, in reply to a `ScanRequest`.
    pub responses: Vec<Bytes>,
    /// How the scan reply ends.
    pub ending: ScanEnding,
    /// Frames sent after an `EndStreamRequest` arrives, before the terminator.
    /// Models packets that were already in flight when the client gave up.
    pub after_end_stream: Vec<Bytes>,
}

/// How the server behaves once it has sent [`Script::responses`].
///
/// The three cases are genuinely different on the wire, and conflating them is
/// how a test ends up asserting something the protocol never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScanEnding {
    /// The scan is complete: terminate immediately. A real server does this
    /// exactly once, which is why `after_end_stream` is unreachable here.
    Terminated(Terminator),
    /// There are more rows than were sent, so the server waits. It terminates
    /// only when the client asks it to stop — the shape of every abandoned
    /// scan, and the only shape in which `EndStreamRequest` does anything.
    AwaitEndStream,
    /// The server died mid-stream: nothing more, and the socket closes.
    Truncated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Terminator {
    /// What a server sends to a client reporting version >= 9.
    StreamEnd(Option<u64>),
    /// What it sends to an older one.
    EndFrame,
}

impl Default for Script {
    fn default() -> Self {
        Script {
            responses: Vec::new(),
            ending: ScanEnding::Terminated(Terminator::StreamEnd(None)),
            after_end_stream: Vec::new(),
        }
    }
}

/// What the server observed, for a test to assert on afterwards.
#[derive(Debug, Default)]
pub(super) struct Observed {
    pub requests: Vec<&'static str>,
    /// The raw `ScanRequest` payload, so a test can assert what went on the
    /// wire rather than what the builder intended.
    pub scan_payload: Option<Bytes>,
}

pub(super) struct TestServer {
    pub addr: Address,
    pub observed: Arc<Mutex<Observed>>,
}

impl TestServer {
    /// Start a server that will accept exactly one connection.
    pub(super) async fn start(script: Script) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = local_address(&listener);
        let observed = Arc::new(Mutex::new(Observed::default()));

        let task_observed = Arc::clone(&observed);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            serve(stream, script, task_observed).await;
        });

        TestServer { addr, observed }
    }

    /// The same server behind a TLS terminator, with a certificate generated
    /// for this process only.
    ///
    /// It exists to prove that the framing survives a TLS stream, which is the
    /// one thing about the TLS path a non-TLS cluster cannot tell us. The
    /// server side is always `rustls`; the client under test may be either
    /// stack, and the interesting run is `--all-features`, where a `native-tls`
    /// client talks to this `rustls` server.
    #[cfg(feature = "rustls-tls")]
    pub(super) async fn start_tls(script: Script) -> TestServer {
        use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        use tokio_rustls::rustls::ServerConfig;
        use tokio_rustls::TlsAcceptor;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = local_address(&listener);
        let observed = Arc::new(Mutex::new(Observed::default()));

        let generated = rcgen::generate_simple_self_signed(vec![
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
        ])
        .expect("a self-signed certificate");
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(generated.cert)],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                    generated.signing_key.serialize_der(),
                )),
            )
            .expect("a server config");
        let acceptor = TlsAcceptor::from(Arc::new(config));

        let task_observed = Arc::clone(&observed);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let stream = acceptor.accept(stream).await.expect("tls handshake");
            serve(stream, script, task_observed).await;
        });

        TestServer { addr, observed }
    }
}

/// The bound address as an [`Address`], which is what `Client::connect` takes.
pub(super) fn local_address(listener: &TcpListener) -> Address {
    let addr = listener.local_addr().expect("addr");
    Address {
        host: addr.ip().to_string(),
        port: addr.port(),
    }
}

/// Generic over the socket so the same script drives a plain and a TLS
/// connection. There is nothing protocol-specific about which one it got, and
/// that is the claim the TLS test makes.
async fn serve<S>(stream: S, script: Script, observed: Arc<Mutex<Observed>>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut framed = Framed::new(stream, PacketCodec::default());
    let mut count_replies = 0i64;

    while let Some(Ok(frame)) = framed.next().await {
        let payload = match frame {
            Frame::Payload(payload) => payload,
            Frame::EndOfResponse => continue,
        };

        let kind = fake::request_kind(&payload);
        observed.lock().await.requests.push(kind);

        match kind {
            // Answered before the request loop on a real server, so it gets no
            // terminator. Getting this wrong is exactly the bug the client's
            // `exchange` doc comment is about, so the fake must be faithful.
            "AuthRequest" => {
                send(&mut framed, fake::auth_response(1)).await;
            }
            "HeloRequest" => {
                send(&mut framed, fake::helo_response(11)).await;
                terminate(&mut framed, Terminator::StreamEnd(None)).await;
            }
            "CountRequest" => {
                send(&mut framed, fake::count_response(count_replies)).await;
                count_replies += 1;
                terminate(&mut framed, Terminator::StreamEnd(None)).await;
            }
            "ScanRequest" => {
                observed.lock().await.scan_payload = Some(payload.clone());
                for response in &script.responses {
                    send(&mut framed, response.clone()).await;
                }
                match script.ending {
                    ScanEnding::Terminated(terminator) => {
                        terminate(&mut framed, terminator).await;
                    }
                    // Say nothing and wait for the client to give up.
                    ScanEnding::AwaitEndStream => {}
                    // The socket has to actually close, or the client waits
                    // forever and the test hangs instead of failing.
                    ScanEnding::Truncated => return,
                }
            }
            "EndStreamRequest" => {
                for response in &script.after_end_stream {
                    send(&mut framed, response.clone()).await;
                }
                terminate(&mut framed, Terminator::StreamEnd(None)).await;
            }
            _ => break,
        }
    }
}

async fn send<S>(framed: &mut Framed<S, PacketCodec>, payload: Bytes)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let _ = framed.send(Frame::Payload(payload)).await;
}

async fn terminate<S>(framed: &mut Framed<S, PacketCodec>, terminator: Terminator)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match terminator {
        Terminator::StreamEnd(read_units) => {
            send(framed, fake::stream_end(read_units)).await;
        }
        Terminator::EndFrame => {
            let _ = framed.send(Frame::EndOfResponse).await;
        }
    }
}
