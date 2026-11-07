// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Regression test: `EncryptedStream::recv_into` must be cancellation-safe.
//!
//! A `tokio::select!` that drops the receive future mid-record used to
//! lose the bytes already pulled from the transport, desynchronizing the
//! record framing (the next "header" was read from the middle of a record
//! and AEAD verification failed). It stayed hidden while records were
//! small enough to arrive in one TCP segment; near-64 KiB storage chunks
//! exposed it. This test drives a max-size record through a tiny-buffered
//! duplex (every read pends) while a fast timer cancels the receive future
//! in a tight loop: all records must still arrive intact.

use std::time::Duration;

use async_trait::async_trait;
use protocol::{TICKET_LENGTH, channel};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use transport::{EncryptedStream, ReliableTransport, TransportError};

/// Fixed test ticket.
fn test_ticket() -> [u8; TICKET_LENGTH] {
    [0x42; TICKET_LENGTH]
}

/// Fixed test pre-shared key.
fn test_key() -> [u8; protocol::crypt::consts::KEY_LENGTH] {
    [0x07; protocol::crypt::consts::KEY_LENGTH]
}

/// [`ReliableTransport`] over a tokio in-memory duplex.
struct DuplexTransport(DuplexStream);

#[async_trait]
impl ReliableTransport for DuplexTransport {
    async fn send(&mut self, bytes: &[u8]) -> transport::Result<()> {
        self.0.write_all(bytes).await?;
        Ok(())
    }

    async fn recv_into(&mut self, buf: &mut [u8]) -> transport::Result<usize> {
        let n = self.0.read(buf).await?;
        if n == 0 {
            return Err(TransportError::Closed);
        }
        Ok(n)
    }

    async fn close(self: Box<Self>) -> transport::Result<()> {
        Ok(())
    }
}

/// Builds the two ends of an encrypted stream over a small duplex, so a
/// max-size record needs many reads (and pends between them).
fn stream_pair() -> (
    EncryptedStream<DuplexTransport>,
    EncryptedStream<DuplexTransport>,
) {
    let (a, b) = tokio::io::duplex(4096);
    let keys = protocol::crypt::derive_session_keys(&test_ticket(), &test_key()).expect("keys");
    let client = EncryptedStream::new(DuplexTransport(a), &keys.c2s, &keys.s2c);
    let server = EncryptedStream::new(DuplexTransport(b), &keys.s2c, &keys.c2s);
    (client, server)
}

#[tokio::test]
async fn recv_survives_select_cancellation_storm() {
    let (mut client, mut server) = stream_pair();

    // Patterned max-size payload (detects both loss and reordering).
    let payload: Vec<u8> = (0..protocol::crypt::consts::MAX_PLAINTEXT)
        .map(|i| u8::try_from(i % 251).expect("fits"))
        .collect();

    let sender = {
        let payload = payload.clone();
        tokio::spawn(async move {
            for _ in 0..3 {
                server.send(channel::STORAGE, &payload).await.unwrap();
            }
        })
    };

    // Cancel the receive future every millisecond: with a 4 KiB duplex the
    // body reads genuinely pend, so the timer constantly drops the future
    // mid-record. Each retry must resume, never restart.
    let mut buf = vec![0u8; protocol::crypt::consts::MAX_PLAINTEXT + 64];
    for round in 0..3 {
        let (ch, n) = loop {
            tokio::select! {
                res = client.recv_into(&mut buf) => break res.unwrap(),
                () = tokio::time::sleep(Duration::from_millis(1)) => {}
            }
        };
        assert_eq!(ch, channel::STORAGE, "round {round}");
        assert_eq!(&buf[..n], payload.as_slice(), "round {round}");
    }
    sender.await.unwrap();
}
