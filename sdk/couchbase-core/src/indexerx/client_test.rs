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

//! The client against a scripted server, over a real socket.

use futures::StreamExt;

use super::client::{Client, ConnectOptions};
use super::error::{AuthCode, ErrorKind, ServerError};
use super::proto::{fake, Consistency, DataEncoding, Projection, ScanOptions};
use super::span::{Filter, Inclusion, Scan};
use super::test_server::{local_address, ScanEnding, Script, Terminator, TestServer};

async fn connect(script: Script) -> (Client, TestServer) {
    let server = TestServer::start(script).await;
    let client = Client::connect(&server.addr, &ConnectOptions::new("user", "pass"))
        .await
        .expect("connect");
    (client, server)
}

fn row(key: &'static [u8]) -> (Option<&'static [u8]>, &'static [u8]) {
    (None, key)
}

#[tokio::test]
async fn connecting_authenticates_then_exchanges_versions() {
    let (client, server) = connect(Script::default()).await;

    assert_eq!(client.server_version(), 11);
    assert_eq!(
        server.observed.lock().await.requests,
        vec!["AuthRequest", "HeloRequest"],
        "auth is always first, and Helo follows it"
    );
}

#[tokio::test]
async fn two_exchanges_on_one_connection_do_not_desynchronise() {
    // The regression test for the terminator rule. Helo and Count each get a
    // StreamEndResponse *after* their answer; a client that reads only the
    // answer leaves it in the socket, and the next request reads it as its own
    // reply. Connecting already does one exchange, so a second one here is
    // enough to catch it — and without the fix it fails with an
    // UnexpectedMessage rather than hanging.
    let (mut client, _server) = connect(Script::default()).await;

    let first = client
        .count(1, Consistency::Session, vec![])
        .await
        .expect("first count");
    let second = client
        .count(1, Consistency::Session, vec![])
        .await
        .expect("second count");

    assert_eq!((first, second), (0, 1), "each count got its own reply");
}

#[tokio::test]
async fn a_scan_yields_every_entry_in_order_across_batches() {
    let script = Script {
        responses: vec![
            fake::entries(&[row(b"doc-1"), row(b"doc-2")]),
            // An empty batch is legal and must not be read as the end.
            fake::entries(&[]),
            fake::entries(&[row(b"doc-3")]),
        ],
        ending: ScanEnding::Terminated(Terminator::StreamEnd(Some(42))),
        ..Script::default()
    };
    let (client, _server) = connect(script).await;

    let mut stream = client
        .scan(ScanOptions::new(7, "req-1"))
        .await
        .expect("scan");

    let mut keys = Vec::new();
    while let Some(entry) = stream.next().await {
        keys.push(entry.expect("entry").primary_key);
    }

    assert_eq!(keys, vec![&b"doc-1"[..], &b"doc-2"[..], &b"doc-3"[..]]);
    assert!(stream.is_ended());
    assert_eq!(stream.read_units(), Some(42));
    stream.finish().await.expect("the connection comes back");
}

#[tokio::test]
async fn entry_keys_survive_the_round_trip() {
    let script = Script {
        responses: vec![fake::entries(&[(Some(b"[3,5]"), b"doc-1")])],
        ..Script::default()
    };
    let (client, _server) = connect(script).await;

    let mut stream = client
        .scan(ScanOptions::new(7, "req-1"))
        .await
        .expect("scan");
    let entry = stream.next().await.expect("an entry").expect("no error");

    assert_eq!(entry.entry_key.as_deref(), Some(&b"[3,5]"[..]));
    assert_eq!(entry.primary_key, &b"doc-1"[..]);
}

#[tokio::test]
async fn abandoning_a_scan_stops_the_server_and_recovers_the_connection() {
    // The case the whole module is shaped around: read one row of three, then
    // give the connection back. `finish` must send EndStreamRequest, absorb
    // whatever was already in flight, and leave a connection the next caller
    // can use.
    let script = Script {
        responses: vec![fake::entries(&[
            row(b"doc-1"),
            row(b"doc-2"),
            row(b"doc-3"),
        ])],
        // The server has more to send and is waiting, which is the only state
        // in which asking it to stop means anything.
        ending: ScanEnding::AwaitEndStream,
        after_end_stream: vec![fake::entries(&[row(b"doc-4")])],
    };
    let (client, server) = connect(script).await;

    let mut stream = client
        .scan(ScanOptions::new(7, "req-1"))
        .await
        .expect("scan");
    let first = stream.next().await.expect("an entry").expect("no error");
    assert_eq!(first.primary_key, &b"doc-1"[..]);

    let mut client = stream.finish().await.expect("the connection comes back");

    assert!(
        server
            .observed
            .lock()
            .await
            .requests
            .contains(&"EndStreamRequest"),
        "the server was asked to stop"
    );

    // The connection is genuinely usable, which is the claim that matters.
    client
        .count(1, Consistency::Session, vec![])
        .await
        .expect("the recovered connection still works");
}

#[tokio::test]
async fn an_indexer_error_ends_the_scan_but_not_the_connection() {
    // A refusal arrives inside a well-formed packet and the server writes its
    // terminator immediately after, so the connection survives even though the
    // scan does not.
    let script = Script {
        responses: vec![
            fake::entries(&[row(b"doc-1")]),
            fake::stream_error("Not my partition"),
        ],
        ..Script::default()
    };
    let (client, _server) = connect(script).await;

    let mut stream = client
        .scan(ScanOptions::new(7, "req-1"))
        .await
        .expect("scan");

    assert_eq!(
        stream
            .next()
            .await
            .expect("an entry")
            .expect("no error")
            .primary_key,
        &b"doc-1"[..]
    );

    match stream.next().await {
        Some(Err(e)) => match e.kind() {
            ErrorKind::Server(e @ ServerError::NotMyPartition(_)) => {
                assert!(e.is_retryable_after_refresh());
            }
            other => panic!("expected a not-my-partition error, got {other:?}"),
        },
        other => panic!("expected a not-my-partition error, got {other:?}"),
    }
    assert!(
        stream.next().await.is_none(),
        "an error ends the stream rather than preceding more rows"
    );
    assert!(!stream.is_ended(), "it did not end, it was refused");

    let mut client = stream.finish().await.expect("the connection comes back");
    client
        .count(1, Consistency::Session, vec![])
        .await
        .expect("the recovered connection still works");
}

#[tokio::test]
async fn a_truncated_stream_is_an_error_and_not_an_end() {
    // No terminator, socket closes. The rows already yielded are a prefix of
    // the answer rather than the whole of it, so reporting success here would
    // be a silently short result — the one failure mode worth being loud about.
    let script = Script {
        responses: vec![fake::entries(&[row(b"doc-1")])],
        ending: ScanEnding::Truncated,
        ..Script::default()
    };
    let (client, _server) = connect(script).await;

    let mut stream = client
        .scan(ScanOptions::new(7, "req-1"))
        .await
        .expect("scan");
    stream.next().await.expect("an entry").expect("no error");

    match stream.next().await {
        Some(Err(e)) if matches!(e.kind(), ErrorKind::UnexpectedEof) => {}
        other => panic!("expected an unexpected-eof, got {other:?}"),
    }
    assert!(
        stream.finish().await.is_err(),
        "a broken connection is not handed back"
    );
}

#[tokio::test]
async fn an_old_servers_end_frame_still_ends_the_stream() {
    // We report a current version so this should not happen, but accepting it
    // costs one match arm and refusing it would break against an older cluster.
    let script = Script {
        responses: vec![fake::entries(&[row(b"doc-1")])],
        ending: ScanEnding::Terminated(Terminator::EndFrame),
        ..Script::default()
    };
    let (client, _server) = connect(script).await;

    let mut stream = client
        .scan(ScanOptions::new(7, "req-1"))
        .await
        .expect("scan");
    stream.next().await.expect("an entry").expect("no error");

    assert!(stream.next().await.is_none());
    assert!(stream.is_ended());
    stream.finish().await.expect("the connection comes back");
}

#[tokio::test]
async fn the_scan_request_says_what_the_options_asked_for() {
    // The conformance assertion for this layer: what actually goes on the wire,
    // rather than what the builder intended. Cheaper and stricter than reading
    // someone else's query plan.
    let (client, server) = connect(Script::default()).await;

    let mut opts = ScanOptions::new(8891, "req-42");
    opts.partitions = vec![1, 2];
    opts.consistency = Consistency::Session;
    opts.offset = 10;
    opts.limit = 50;
    opts.distinct = true;
    opts.data_encoding = DataEncoding::Json;
    opts.projection = Some(Projection::keys_and(vec![0, 1]));
    opts.scans = vec![
        Scan::filtered(vec![
            Filter::eq(b"3".to_vec()),
            Filter::range(b"5".to_vec(), b"9".to_vec()).with_inclusion(Inclusion::Low),
        ]),
        Scan::equals(vec![b"1".to_vec(), b"2".to_vec()]),
    ];

    let stream = client.scan(opts).await.expect("scan");
    stream.finish().await.expect("finish");

    let payload = server
        .observed
        .lock()
        .await
        .scan_payload
        .clone()
        .expect("a scan request reached the server");
    let sent = fake::scan_request_fields(&payload);

    assert_eq!(sent.defn_id, 8891);
    assert_eq!(sent.request_id.as_deref(), Some("req-42"));
    assert_eq!(sent.partition_ids, vec![1, 2]);
    assert_eq!(sent.cons, 2, "Session is request_plus");
    assert_eq!(sent.offset, 10);
    assert_eq!(sent.limit, 50);
    assert!(sent.distinct);
    assert_eq!(
        sent.data_enc_fmt,
        Some(0),
        "JSON, so no collatejson decoder"
    );
    assert_eq!(
        sent.reverse, None,
        "never set: the indexer parses it and never reads it"
    );
    assert_eq!(sent.projected_entry_keys, Some(vec![0, 1]));
    assert_eq!(sent.projected_primary_key, Some(true));

    assert_eq!(
        sent.scan_filters[0],
        vec![
            (Some(b"3".to_vec()), Some(b"3".to_vec()), 3),
            (Some(b"5".to_vec()), Some(b"9".to_vec()), 1),
        ],
        "an equality is an inclusive range; Low is inclusion 1"
    );
    assert_eq!(
        sent.scan_equals[1],
        vec![b"1".to_vec(), b"2".to_vec()],
        "the second span pins the whole key"
    );
}

#[tokio::test]
async fn an_unbounded_end_is_omitted_rather_than_sent_as_a_sentinel() {
    let (client, server) = connect(Script::default()).await;

    let mut opts = ScanOptions::new(1, "req-1");
    opts.scans = vec![Scan::filtered(vec![Filter {
        low: Some(b"5".to_vec()),
        high: None,
        inclusion: Inclusion::Low,
    }])];

    let stream = client.scan(opts).await.expect("scan");
    stream.finish().await.expect("finish");

    let payload = server
        .observed
        .lock()
        .await
        .scan_payload
        .clone()
        .expect("scan");
    let sent = fake::scan_request_fields(&payload);

    assert_eq!(sent.scan_filters[0], vec![(Some(b"5".to_vec()), None, 1)]);
}

#[tokio::test]
async fn bad_credentials_fail_the_connection_rather_than_the_first_request() {
    // The server closes on an auth failure, so there is no usable client to
    // hand back — connect is the right place for this to surface.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = local_address(&listener);

    tokio::spawn(async move {
        use futures::SinkExt;
        use tokio_util::codec::Framed;

        let (stream, _) = listener.accept().await.expect("accept");
        let mut framed = Framed::new(stream, super::packet_codec::PacketCodec::default());
        let _ = framed.next().await;
        let _ = framed
            .send(super::packet_codec::Frame::Payload(fake::auth_response(2)))
            .await;
    });

    match Client::connect(&addr, &ConnectOptions::new("user", "wrong")).await {
        Err(e) if matches!(e.kind(), ErrorKind::Authentication(AuthCode::Failure)) => {}
        Err(other) => panic!("expected an authentication failure, got {other:?}"),
        Ok(_) => panic!("expected an authentication failure, got a connection"),
    }
}

/// The scan path over TLS.
///
/// Gated on `rustls-tls` because the *server* side is always rustls; the client
/// under test is whichever stack the build selected, so `--all-features` runs a
/// `native-tls` client against it and the default build runs a rustls one. That
/// is deliberate — it is the only place the two stacks meet.
#[cfg(feature = "rustls-tls")]
mod tls {
    use super::*;
    use crate::tls_config::TlsConfig;

    /// A client config that trusts the test server's throwaway certificate.
    ///
    /// The point of the test is the transport, not the trust decision, so both
    /// arms turn verification off — with the crate's own `InsecureCertVerifier`
    /// on the rustls side, which is what the integration tests already use.
    #[cfg(all(feature = "rustls-tls", not(feature = "native-tls")))]
    fn insecure_tls_config() -> TlsConfig {
        use std::sync::Arc;

        Arc::new(
            tokio_rustls::rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(
                    crate::insecure_certverfier::InsecureCertVerifier {},
                ))
                .with_no_client_auth(),
        )
    }

    #[cfg(feature = "native-tls")]
    fn insecure_tls_config() -> TlsConfig {
        tokio_native_tls::native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
            .expect("a native-tls connector")
    }

    #[tokio::test]
    async fn a_whole_scan_survives_the_tls_stream() {
        // Connect, authenticate, exchange versions, stream two batches, take the
        // terminator, and hand the connection back — the same script as the
        // plain-socket test, over TLS. Framing is the thing at risk: a TLS
        // record boundary falls wherever it likes relative to a six-byte header,
        // so a codec that assumed a frame arrives whole would fail here and
        // pass on loopback TCP.
        let script = Script {
            responses: vec![
                fake::entries(&[row(b"tls-1"), row(b"tls-2")]),
                fake::entries(&[row(b"tls-3")]),
            ],
            ending: ScanEnding::Terminated(Terminator::StreamEnd(Some(7))),
            ..Script::default()
        };
        let server = TestServer::start_tls(script).await;

        let opts = ConnectOptions::new("user", "pass").with_tls_config(Some(insecure_tls_config()));
        let client = Client::connect(&server.addr, &opts)
            .await
            .expect("a TLS connection to the queryport");
        assert_eq!(client.server_version(), 11, "Helo came back over TLS");

        let mut stream = client
            .scan(ScanOptions::new(7, "tls-req"))
            .await
            .expect("scan");
        let mut keys = Vec::new();
        while let Some(entry) = stream.next().await {
            keys.push(entry.expect("entry").primary_key);
        }

        assert_eq!(keys, vec![&b"tls-1"[..], &b"tls-2"[..], &b"tls-3"[..]]);
        assert!(stream.is_ended());
        assert_eq!(stream.read_units(), Some(7));

        let mut client = stream.finish().await.expect("the connection comes back");
        client
            .count(1, Consistency::Session, vec![])
            .await
            .expect("the recovered TLS connection still works");

        assert_eq!(
            server.observed.lock().await.requests,
            vec!["AuthRequest", "HeloRequest", "ScanRequest", "CountRequest"],
            "every exchange went over the one TLS connection, in order"
        );
    }
}
