//! Integration tests of the `transport` crate: encrypted E2E echo over direct
//! TCP on loopback, with a real Hello handshake.

use protocol::{TICKET_LENGTH, channel};
use transport::{
    Connector, DirectConnector, DirectListener, EncryptedStream, ReliableTransport, Target,
    TransportError,
};

/// Fixed test ticket.
fn test_ticket() -> [u8; TICKET_LENGTH] {
    [0x42; TICKET_LENGTH]
}

/// Fixed test pre-shared key.
fn test_key() -> [u8; crypt::consts::KEY_LENGTH] {
    [0x07; crypt::consts::KEY_LENGTH]
}

/// Connects a client with a completed handshake against `addr`.
async fn connect(addr: std::net::SocketAddr) -> EncryptedStream<Box<dyn ReliableTransport>> {
    let connector = DirectConnector::new();
    let (raw, _) = connector
        .connect(&Target::new(addr, test_ticket(), test_key()))
        .await
        .unwrap();
    EncryptedStream::client_handshake(raw, &test_ticket(), &test_key())
        .await
        .unwrap()
}

#[tokio::test]
async fn server_rejects_a_mismatched_protocol_version() {
    let listener = DirectListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (raw, _) = listener.accept().await.unwrap();
        EncryptedStream::server_handshake(raw, &test_key(), None).await
    });

    // A peer speaking a different wire format (e.g. v1 fragments without
    // capture_ts_ms) must be rejected, never silently misparsed.
    let connector = DirectConnector::new();
    let (mut raw, _) = connector
        .connect(&Target::new(addr, test_ticket(), test_key()))
        .await
        .unwrap();
    let hello = protocol::Hello {
        version: protocol::PROTOCOL_VERSION + 1,
        command: protocol::Command::Open,
        ticket: test_ticket(),
    };
    raw.send(&hello.encode()).await.unwrap();

    let result = server.await.unwrap();
    let Err(err) = result else {
        panic!("version mismatch must fail the handshake");
    };
    assert!(
        matches!(err, TransportError::VersionMismatch { expected, got }
            if expected == protocol::PROTOCOL_VERSION && got == protocol::PROTOCOL_VERSION + 1),
        "unexpected error: {err:?}"
    );
}

#[tokio::test]
async fn echo_roundtrip_over_encrypted_tcp() {
    let listener = DirectListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (raw, _) = listener.accept().await.unwrap();
        let mut stream = EncryptedStream::server_handshake(raw, &test_key(), None).await.unwrap();
        let mut buf = [0u8; 4096];
        let (ch, n) = stream.recv_into(&mut buf).await.unwrap();
        stream.send(ch, &buf[..n]).await.unwrap();
    });

    let mut client = connect(addr).await;
    client.send(channel::CONTROL, b"hola").await.unwrap();
    let mut buf = [0u8; 4096];
    let (ch, n) = client.recv_into(&mut buf).await.unwrap();
    assert_eq!(ch, channel::CONTROL);
    assert_eq!(&buf[..n], b"hola");

    server.await.unwrap();
}

#[tokio::test]
async fn multiple_frames_keep_order_and_channels() {
    let listener = DirectListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (raw, _) = listener.accept().await.unwrap();
        let mut stream = EncryptedStream::server_handshake(raw, &test_key(), None).await.unwrap();
        let mut buf = [0u8; 4096];
        let mut received = Vec::new();
        for _ in 0..3 {
            let (ch, n) = stream.recv_into(&mut buf).await.unwrap();
            received.push((ch, buf[..n].to_vec()));
        }
        received
    });

    let mut client = connect(addr).await;
    client.send(channel::CONTROL, b"uno").await.unwrap();
    client.send(channel::DISPLAY, b"dos").await.unwrap();
    client.send(channel::AUDIO_OUT, b"tres").await.unwrap();

    let received = server.await.unwrap();
    assert_eq!(
        received,
        vec![
            (channel::CONTROL, b"uno".to_vec()),
            (channel::DISPLAY, b"dos".to_vec()),
            (channel::AUDIO_OUT, b"tres".to_vec()),
        ]
    );
}

#[tokio::test]
async fn large_payload_survives_tcp_fragmentation() {
    let listener = DirectListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // 60_000 bytes: fits in one record (MAX_PLAINTEXT = 65517) but TCP will
    // fragment it into several reads on loopback.
    let payload: Vec<u8> = (0..60_000u32).map(|i| (i % 251) as u8).collect();
    let expected = payload.clone();

    let server = tokio::spawn(async move {
        let (raw, _) = listener.accept().await.unwrap();
        let mut stream = EncryptedStream::server_handshake(raw, &test_key(), None).await.unwrap();
        let mut buf = vec![0u8; 70_000];
        let (ch, n) = stream.recv_into(&mut buf).await.unwrap();
        assert_eq!(ch, channel::DISPLAY);
        buf.truncate(n);
        buf
    });

    let mut client = connect(addr).await;
    client.send(channel::DISPLAY, &payload).await.unwrap();

    let received = server.await.unwrap();
    assert_eq!(received, expected);
}

#[tokio::test]
async fn payload_above_max_is_rejected() {
    let listener = DirectListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (raw, _) = listener.accept().await.unwrap();
        let _stream = EncryptedStream::server_handshake(raw, &test_key(), None).await.unwrap();
        // Keeps listening; the client will fail before sending.
    });

    let mut client = connect(addr).await;
    let too_big = vec![0u8; crypt::consts::MAX_PLAINTEXT + 1];
    let result = client.send(channel::CONTROL, &too_big).await;
    assert!(matches!(result, Err(TransportError::TooLarge { .. })));

    server.abort();
}

#[tokio::test]
async fn handshake_with_garbage_is_rejected() {
    use tokio::io::AsyncWriteExt;

    let listener = DirectListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (raw, _) = listener.accept().await.unwrap();
        EncryptedStream::server_handshake(raw, &test_key(), None).await
    });

    // Client that sends garbage (of the exact length of a Hello) instead of
    // a valid Hello: the signature does not match and the server must reject it.
    let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    socket
        .write_all(&[0xAB; protocol::Hello::ENCODED_LEN])
        .await
        .unwrap();

    let result = server.await.unwrap();
    assert!(matches!(result, Err(TransportError::Protocol(_))));
}

#[tokio::test]
async fn server_sees_client_ticket() {
    let listener = DirectListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (raw, _) = listener.accept().await.unwrap();
        let stream = EncryptedStream::server_handshake(raw, &test_key(), None).await.unwrap();
        stream.peer_hello().unwrap().ticket
    });

    let _client = connect(addr).await;
    let ticket = server.await.unwrap();
    assert_eq!(ticket, test_ticket());
}

#[tokio::test]
async fn connect_to_closed_port_fails() {
    let connector = DirectConnector::new();
    let target = Target::new("127.0.0.1:1".parse().unwrap(), test_ticket(), test_key());
    let result = connector.connect(&target).await;
    assert!(matches!(result, Err(TransportError::Io(_))));
}

#[tokio::test]
async fn reliable_transport_is_dyn_compatible() {
    let listener = DirectListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (_raw, _) = listener.accept().await.unwrap();
    });

    let connector = DirectConnector::new();
    let (raw, _) = connector
        .connect(&Target::new(addr, test_ticket(), test_key()))
        .await
        .unwrap();
    // Must be able to live behind a trait object (design requirement for the
    // future TunnelConnector).
    let mut boxed: Box<dyn ReliableTransport> = Box::new(raw);
    boxed.send(b"ping").await.unwrap();
    boxed.close().await.unwrap();

    server.await.unwrap();
}
