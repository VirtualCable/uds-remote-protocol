// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Encrypted datagram integration tests over real UDP loopback (§6.2).

use crypt::consts;
use protocol::channel;
use transport::{EncryptedDatagram, TransportError, UdpTransport};

const TOKEN: [u8; consts::TOKEN_LENGTH] = [0x5A; consts::TOKEN_LENGTH];
const KEY_C2S: [u8; consts::KEY_LENGTH] = [0x11; consts::KEY_LENGTH];
const KEY_S2C: [u8; consts::KEY_LENGTH] = [0x22; consts::KEY_LENGTH];

/// Connected (client, server) encrypted datagram pair over loopback.
async fn connected_pair() -> (
    EncryptedDatagram<UdpTransport>,
    EncryptedDatagram<UdpTransport>,
) {
    let server = UdpTransport::accept_first("127.0.0.1:0").unwrap();
    let server_addr = server.local_addr().unwrap();

    let client_raw = UdpTransport::connect("127.0.0.1:0", server_addr).await.unwrap();
    // Trigger the server's peer discovery with a first (probe) datagram.
    client_raw
        .send_raw(b"hello-server")
        .await
        .unwrap();
    let (server_raw, _first) = server.wait_first().await.unwrap();

    let client = EncryptedDatagram::new(client_raw, &KEY_C2S, &KEY_S2C, &TOKEN);
    let server = EncryptedDatagram::new(server_raw, &KEY_S2C, &KEY_C2S, &TOKEN);
    (client, server)
}

#[tokio::test]
async fn datagram_echo_roundtrip() {
    let (mut client, mut server) = connected_pair().await;

    let echo = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let (ch, n) = server.recv_into(&mut buf).await.unwrap();
        server.send(ch, &buf[..n]).await.unwrap();
        server
    });

    client.send(channel::AUDIO_OUT, b"opus-frame").await.unwrap();
    let mut buf = [0u8; 2048];
    let (ch, n) = client.recv_into(&mut buf).await.unwrap();
    assert_eq!(ch, channel::AUDIO_OUT);
    assert_eq!(&buf[..n], b"opus-frame");
    echo.await.unwrap();
}

#[tokio::test]
async fn multiple_datagrams_arrive() {
    let (mut client, mut server) = connected_pair().await;

    let recv = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let mut got = Vec::new();
        for _ in 0..3 {
            let (ch, n) = server.recv_into(&mut buf).await.unwrap();
            got.push((ch, buf[..n].to_vec()));
        }
        got
    });

    client.send(channel::DISPLAY, b"f1").await.unwrap();
    client.send(channel::DISPLAY, b"f2").await.unwrap();
    client.send(channel::AUDIO_OUT, b"a1").await.unwrap();

    let got = recv.await.unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(got[0], (channel::DISPLAY, b"f1".to_vec()));
    assert_eq!(got[1], (channel::DISPLAY, b"f2".to_vec()));
    assert_eq!(got[2], (channel::AUDIO_OUT, b"a1".to_vec()));
}

#[tokio::test]
async fn max_size_datagram_roundtrip() {
    let (mut client, mut server) = connected_pair().await;

    let payload = vec![0xAB; consts::MAX_DATAGRAM_PAYLOAD];
    let expected = payload.clone();
    let recv = tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        let (_ch, n) = server.recv_into(&mut buf).await.unwrap();
        assert_eq!(n, consts::MAX_DATAGRAM_PAYLOAD);
        buf[..n].to_vec()
    });

    client.send(channel::DISPLAY, &payload).await.unwrap();
    assert_eq!(recv.await.unwrap(), expected);
}

#[tokio::test]
async fn payload_above_max_is_rejected() {
    let (mut client, _server) = connected_pair().await;
    let too_big = vec![0u8; consts::MAX_DATAGRAM_PAYLOAD + 1];
    let result = client.send(channel::DISPLAY, &too_big).await;
    assert!(matches!(result, Err(TransportError::TooLarge { .. })));
}

#[tokio::test]
async fn foreign_traffic_is_dropped_not_delivered() {
    let (mut client, mut server) = connected_pair().await;

    // A third party with wrong keys/token sends garbage to the server socket;
    // the server must skip it silently and still deliver the next valid one.
    let raw_attacker = UdpTransport::connect("127.0.0.1:0", server.local_addr().unwrap())
        .await
        .unwrap();
    raw_attacker.send_raw(b"garbage-not-our-protocol").await.unwrap();
    client.send(channel::CONTROL, b"valid").await.unwrap();

    let mut buf = [0u8; 256];
    let (ch, n) = server.recv_into(&mut buf).await.unwrap();
    assert_eq!(ch, channel::CONTROL);
    assert_eq!(&buf[..n], b"valid");
}

#[tokio::test]
async fn lost_estimate_is_exposed() {
    let (client, _server) = connected_pair().await;
    assert_eq!(client.lost_estimate(), 0);
}
