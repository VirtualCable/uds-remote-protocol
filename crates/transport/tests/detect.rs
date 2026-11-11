// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! UDP detection end-to-end tests (§4.2): after the TCP handshake the server
//! offers a UDP leg (`UdpOffer`), the client probes it, and lossy channels
//! only move to UDP if a valid `ProbeAck` comes back.

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use protocol::crypt::{consts, derive_datagram_keys};
use protocol::{Probe, ProbeAck, channel};
use transport::{
    Connector, DirectConnector, DirectListener, EncryptedDatagram, EncryptedStream, Target,
    udp_detect, udp_offer,
};

fn ticket() -> [u8; consts::TICKET_LENGTH] {
    [0x42; consts::TICKET_LENGTH]
}

fn secret() -> [u8; consts::KEY_LENGTH] {
    [0x07; consts::KEY_LENGTH]
}

/// Monotonic-ish micros clock for probe timestamps.
fn micros() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros(),
    )
    .unwrap()
}

async fn server_side(addr_tx: tokio::sync::oneshot::Sender<SocketAddr>, answer_probes: bool) {
    let listener = DirectListener::bind("127.0.0.1:0").await.unwrap();
    addr_tx.send(listener.local_addr().unwrap()).unwrap();

    let (raw, _) = listener.accept().await.unwrap();
    let mut stream = EncryptedStream::server_handshake(raw, &secret(), None)
        .await
        .unwrap();
    let (pending, token) = udp_offer(&mut stream, "127.0.0.1:0", &ticket(), &secret())
        .await
        .unwrap();

    if !answer_probes {
        // Blackhole: UDP is offered but never answered (filtered network).
        tokio::time::sleep(Duration::from_secs(2)).await;
        return;
    }

    let (udp, first) = pending.wait_first(&token).await.unwrap();
    let keys = derive_datagram_keys(&ticket(), &secret()).unwrap();
    let mut leg = EncryptedDatagram::new(udp, &keys.s2c, &keys.c2s, &token);

    // Answer the detection probe(s) with NTP-style timestamps.
    let mut first_buf = [0u8; 256];
    if let Some((ch, n)) = leg.decrypt_received(&first, &mut first_buf).unwrap() {
        let probe = Probe::decode(&first_buf[..n]).unwrap();
        let ack = ProbeAck {
            t1: probe.t1,
            t2: micros(),
            t3: micros(),
        };
        leg.send(ch, &ack.encode()).await.unwrap();
    }

    // Then echo one datagram to prove the leg carries real traffic.
    let mut buf = [0u8; 256];
    let (ch, n) = leg.recv_into(&mut buf).await.unwrap();
    leg.send(ch, &buf[..n]).await.unwrap();
}

async fn client_stream(addr: SocketAddr) -> EncryptedStream<Box<dyn transport::ReliableTransport>> {
    let connector = DirectConnector::new();
    let (raw, _) = connector
        .connect(&Target::new(addr, ticket(), secret()))
        .await
        .unwrap();
    EncryptedStream::client_handshake(raw, &ticket(), &secret(), 0, (0, 0))
        .await
        .unwrap()
}

#[tokio::test]
async fn udp_detection_succeeds_end_to_end() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(server_side(tx, true));
    let addr = rx.await.unwrap();

    let mut stream = client_stream(addr).await;
    let leg = udp_detect(
        &mut stream,
        addr.ip(),
        &ticket(),
        &secret(),
        micros,
        3,
        Duration::from_millis(300),
    )
    .await
    .unwrap()
    .expect("UDP must be detected when the server answers");

    // Loopback RTT is tiny but the measurement must be sane (< 1 s).
    assert!(leg.initial_rtt_micros < 1_000_000);

    // The leg carries real encrypted traffic.
    let mut dgram = leg.dgram;
    dgram.send(channel::CONTROL, b"ping").await.unwrap();
    let mut buf = [0u8; 64];
    let (_ch, n) = dgram.recv_into(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"ping");

    server.await.unwrap();
}

#[tokio::test]
async fn wait_first_ignores_tokenless_datagrams() {
    // §18.78: an unsolicited datagram must not lock the UDP peer before
    // authentication; only a datagram carrying the session token may.
    use transport::UdpTransport;
    let pending = UdpTransport::accept_first("127.0.0.1:0").unwrap();
    let port = pending.local_addr().unwrap().port();
    let token = [0x5Au8; consts::TOKEN_LENGTH];

    let attacker = UdpTransport::connect("127.0.0.1:0", format!("127.0.0.1:{port}").parse().unwrap())
        .await
        .unwrap();
    attacker.send_raw(b"unauthenticated junk").await.unwrap();

    let legit = UdpTransport::connect("127.0.0.1:0", format!("127.0.0.1:{port}").parse().unwrap())
        .await
        .unwrap();
    let mut probe = token.to_vec();
    probe.extend_from_slice(b"probe");
    legit.send_raw(&probe).await.unwrap();

    let (_udp, first) = tokio::time::timeout(Duration::from_secs(2), pending.wait_first(&token))
        .await
        .expect("the legit datagram must win the lock")
        .unwrap();
    assert_eq!(first, probe, "tokenless junk must be ignored");
}

#[tokio::test]
async fn udp_detection_falls_back_on_silence() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(server_side(tx, false));
    let addr = rx.await.unwrap();

    let mut stream = client_stream(addr).await;
    let leg = udp_detect(
        &mut stream,
        addr.ip(),
        &ticket(),
        &secret(),
        micros,
        2,
        Duration::from_millis(100),
    )
    .await
    .unwrap();

    assert!(leg.is_none(), "no ProbeAck -> lossy channels stay on TCP");
    server.await.unwrap();
}
