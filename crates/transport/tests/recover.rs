// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Session recovery over a direct connection (§4.3): after a TCP drop the
//! client reconnects with `Command::Recover` and both ends resume from the
//! last known sequence numbers — nonce reuse is never allowed.

use protocol::crypt::consts;
use protocol::{Command, channel};
use transport::{
    Connector, DirectConnector, DirectListener, EncryptedStream, ReliableTransport, Target,
    TransportError,
};

fn ticket() -> [u8; consts::TICKET_LENGTH] {
    [0x42; consts::TICKET_LENGTH]
}

fn secret() -> [u8; consts::KEY_LENGTH] {
    [0x07; consts::KEY_LENGTH]
}

/// Echo server handling exactly two connections: the original and one
/// recovery. Keeps the session seqs between connections (a minimal in-memory
/// session store, like the real server runtime will). Returns the commands
/// it saw, in order.
async fn echo_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<Vec<Command>>) {
    let listener = DirectListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let mut stored: Option<transport::StreamSeqs> = None;
        let mut commands = Vec::new();
        for _ in 0..2 {
            let (raw, _) = listener.accept().await.unwrap();
            let mut stream = EncryptedStream::server_handshake(raw, &secret(), stored)
                .await
                .unwrap();
            commands.push(stream.peer_hello().unwrap().command);
            let mut buf = [0u8; 256];
            // Echo messages until the client goes away.
            while let Ok((ch, n)) = stream.recv_into(&mut buf).await {
                if stream.send(ch, &buf[..n]).await.is_err() {
                    break;
                }
            }
            stored = Some(stream.seqs());
        }
        commands
    });
    (addr, handle)
}

async fn connect(addr: std::net::SocketAddr) -> EncryptedStream<Box<dyn ReliableTransport>> {
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
async fn recover_resumes_session_after_tcp_drop() {
    let (addr, server) = echo_server().await;

    // 1. Normal session: a couple of echo round-trips.
    let mut client = connect(addr).await;
    let mut buf = [0u8; 256];
    for msg in [b"one".as_slice(), b"two".as_slice()] {
        client.send(channel::CONTROL, msg).await.unwrap();
        let (_ch, n) = client.recv_into(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], msg);
    }

    // 2. Save the sequence numbers and simulate a network drop.
    let seqs = client.seqs();
    assert!(seqs.send > 0 && seqs.recv > 0);
    drop(client);

    // 3. Reconnect with Recover + the saved seqs.
    let connector = DirectConnector::new();
    let (raw, _) = connector
        .connect(&Target::new(addr, ticket(), secret()))
        .await
        .unwrap();
    let transport::RecoverOutcome::Resumed(mut recovered) =
        EncryptedStream::client_recover(raw, &ticket(), &secret(), seqs, 0, (0, 0))
            .await
            .unwrap()
    else {
        panic!("server must still hold the session");
    };

    // 4. The session continues transparently. If either side had restarted
    // its seqs, the peer would reject packets as replays and this would fail.
    recovered.send(channel::CONTROL, b"three").await.unwrap();
    let (_ch, n) = recovered.recv_into(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"three");

    // Seqs continued from the saved point, not from 0.
    assert_eq!(recovered.seqs().send, seqs.send + 1);
    assert_eq!(recovered.seqs().recv, seqs.recv + 1);

    drop(recovered);
    let commands = server.await.unwrap();
    assert_eq!(commands, vec![Command::Open, Command::Recover]);
}

#[tokio::test]
async fn recover_never_rolls_back_server_seqs() {
    let (addr, server) = echo_server().await;

    let mut client = connect(addr).await;
    let mut buf = [0u8; 256];
    client.send(channel::CONTROL, b"msg").await.unwrap();
    client.recv_into(&mut buf).await.unwrap();
    drop(client);

    // A client presenting stale (lower) seqs must never pull the server
    // backwards: the server answers with the seqs it stored, and the merge
    // leaves both ends at max(own, peer expectation).
    let connector = DirectConnector::new();
    let (raw, _) = connector
        .connect(&Target::new(addr, ticket(), secret()))
        .await
        .unwrap();
    let zeroed = transport::StreamSeqs { send: 0, recv: 0 };
    let transport::RecoverOutcome::Resumed(mut recovered) =
        EncryptedStream::client_recover(raw, &ticket(), &secret(), zeroed, 0, (0, 0))
            .await
            .unwrap()
    else {
        panic!("server must still hold the session");
    };

    // The server stored send=2/recv=2 (proof exchange + one echo): the
    // resumed client must be there too, not at 0 — so no nonce ever repeats.
    assert_eq!(recovered.seqs(), transport::StreamSeqs { send: 2, recv: 2 });

    // And the session works from that point on.
    recovered.send(channel::CONTROL, b"again").await.unwrap();
    let (_ch, n) = recovered.recv_into(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"again");

    drop(recovered);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server).await;
}

#[tokio::test]
async fn recover_with_open_only_server_still_works() {
    // Sanity: a server that gets Open for a new ticket behaves exactly as
    // before (recovery support must not break the normal path).
    let listener = DirectListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (raw, _) = listener.accept().await.unwrap();
        let mut stream = EncryptedStream::server_handshake(raw, &secret(), None)
            .await
            .unwrap();
        assert_eq!(stream.peer_hello().unwrap().command, Command::Open);
        let mut buf = [0u8; 64];
        let (ch, n) = stream.recv_into(&mut buf).await.unwrap();
        stream.send(ch, &buf[..n]).await.unwrap();
    });

    let mut client = connect(addr).await;
    client.send(channel::CONTROL, b"hi").await.unwrap();
    let mut buf = [0u8; 64];
    let (_ch, n) = client.recv_into(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"hi");
    server.await.unwrap();
}

#[tokio::test]
async fn receive_after_peer_gone_reports_closed() {
    let listener = DirectListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (raw, _) = listener.accept().await.unwrap();
        let mut stream = EncryptedStream::server_handshake(raw, &secret(), None)
            .await
            .unwrap();
        let mut buf = [0u8; 64];
        stream.recv_into(&mut buf).await
    });

    let client = connect(addr).await;
    drop(client); // vanish without saying goodbye

    let result = server.await.unwrap();
    assert!(matches!(
        result,
        Err(TransportError::Closed | TransportError::Io(_))
    ));
}
