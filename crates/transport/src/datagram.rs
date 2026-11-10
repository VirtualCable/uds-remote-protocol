// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Encrypted datagram layer over any [`DatagramTransport`] (design doc §6.2).
//!
//! Same role as [`crate::EncryptedStream`] but for the lossy leg: each
//! datagram is a self-contained, discardable element. Invalid traffic
//! (foreign tokens, tampering, replays) is dropped silently — over UDP that
//! is normal network behaviour, not necessarily an attack.

use protocol::ChannelId;
use protocol::crypt::{CryptError, DatagramCrypt, DatagramDecrypt, consts};

use crate::error::{Result, TransportError};
use crate::traits::DatagramTransport;
use std::sync::Arc;

/// Encrypted datagram channel over a [`DatagramTransport`] (direct UDP or the
/// launcher's local tunnel UDP relay).
pub struct EncryptedDatagram<T: DatagramTransport> {
    inner: T,
    tx: DatagramCrypt,
    rx: DatagramDecrypt,
    // Reusable receive buffer (no allocs in the hot path, risks r2 §2.1).
    rx_buf: Vec<u8>,
    // Reusable encryption scratch buffer (zero-alloc hot path, §12.6).
    tx_buf: Vec<u8>,
    /// Swallowed invalid datagrams: `(bad_token, aead, replay, too_old)`.
    invalid_dgrams: (u64, u64, u64, u64),
    /// Lab knob (§18.77): `UDSRP_SIM_LOSS=<permille 0..1000>` drops raw
    /// datagrams before decrypt, so the sequence gap is real for the loss
    /// estimator (unlike dropping after `recv_into`, which hides it).
    sim_loss: (u64, u64),
}

/// Reads the `UDSRP_SIM_LOSS` lab knob (permille + xorshift64* state).
fn sim_loss_from_env() -> (u64, u64) {
    let permille: u64 = std::env::var("UDSRP_SIM_LOSS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
        .min(1000);
    if permille > 0 {
        tracing::warn!(permille, "lab: simulating inbound UDP loss (permille)");
    }
    (permille, 0x9E37_79B9_7F4A_7C15)
}

/// True when the next raw datagram must be dropped (lab knob, §18.77).
fn sim_loss_hit(sim_loss: &mut (u64, u64)) -> bool {
    let (permille, rng) = sim_loss;
    if *permille == 0 {
        return false;
    }
    *rng ^= *rng >> 12;
    *rng ^= *rng << 25;
    *rng ^= *rng >> 27;
    (rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) % 1000 < *permille
}

impl<T: DatagramTransport> EncryptedDatagram<T> {
    /// Creates the layer with explicit send/receive keys and session token.
    #[must_use]
    pub fn new(
        inner: T,
        tx_key: &[u8; consts::KEY_LENGTH],
        rx_key: &[u8; consts::KEY_LENGTH],
        token: &[u8; consts::TOKEN_LENGTH],
    ) -> Self {
        Self {
            inner,
            tx: DatagramCrypt::new(tx_key, token),
            rx: DatagramDecrypt::new(rx_key, token),
            rx_buf: vec![0u8; 2048],
            tx_buf: vec![
                0u8;
                consts::DATAGRAM_HEADER_SIZE
                    + consts::CHANNEL_ID_SIZE
                    + consts::MAX_DATAGRAM_PAYLOAD
                    + consts::TAG_LENGTH
            ],
            invalid_dgrams: (0, 0, 0, 0),
            sim_loss: sim_loss_from_env(),
        }
    }

    /// Sends `payload` encrypted on `channel`, as a single datagram.
    pub async fn send(&mut self, channel: ChannelId, payload: &[u8]) -> Result<()> {
        if payload.len() > consts::MAX_DATAGRAM_PAYLOAD {
            return Err(TransportError::TooLarge {
                size: payload.len(),
                max: consts::MAX_DATAGRAM_PAYLOAD,
            });
        }
        let n = self.tx.encrypt_into(channel, payload, &mut self.tx_buf)?;
        self.inner.send(&self.tx_buf[..n]).await
    }

    /// Receives the next valid datagram: channel id and plaintext length,
    /// copied into `buf`. Invalid datagrams (foreign token, tampering,
    /// replay, too old) are dropped (counted, see [`Self::dropped_invalid`])
    /// and the wait continues.
    pub async fn recv_into(&mut self, buf: &mut [u8]) -> Result<(ChannelId, usize)> {
        loop {
            let n = self.inner.recv_into(&mut self.rx_buf).await?;
            if sim_loss_hit(&mut self.sim_loss) {
                continue;
            }
            let dgram = &self.rx_buf[..n];
            let Self {
                rx, invalid_dgrams, ..
            } = self;
            if let Some(result) = try_decrypt(rx, dgram, buf, invalid_dgrams)? {
                return Ok(result);
            }
        }
    }

    /// Decrypts an already-received datagram (e.g. the first one, consumed
    /// by peer discovery). Returns `Ok(None)` for droppable noise (foreign
    /// token, tampering, replay, too old).
    pub fn decrypt_received(
        &mut self,
        dgram: &[u8],
        buf: &mut [u8],
    ) -> Result<Option<(ChannelId, usize)>> {
        self.try_decrypt(dgram, buf)
    }

    /// Invalid datagrams swallowed so far: `(bad_token, aead, replay,
    /// too_old)`. Diagnostics for the loss-accounting blind spot: these
    /// never show in `lost_estimate` (they never had a valid sequence).
    #[must_use]
    pub fn dropped_invalid(&self) -> (u64, u64, u64, u64) {
        self.invalid_dgrams
    }

    fn try_decrypt(&mut self, dgram: &[u8], buf: &mut [u8]) -> Result<Option<(ChannelId, usize)>> {
        try_decrypt(&mut self.rx, dgram, buf, &mut self.invalid_dgrams)
    }

    /// Packet loss estimate derived from sequence gaps (§12.2).
    #[must_use]
    pub fn lost_estimate(&self) -> u64 {
        self.rx.lost_estimate()
    }

    /// Valid datagrams received so far; with [`Self::lost_estimate`] it gives
    /// a loss rate over any window (§12.2).
    #[must_use]
    pub fn received(&self) -> u64 {
        self.rx.received()
    }

    /// Splits into independent send/receive halves sharing the socket, so a
    /// paced sender task (§12.6) can run concurrently with the receive loop
    /// without lock convoys.
    #[must_use]
    pub fn into_split(self) -> (DatagramSink<T>, DatagramSource<T>) {
        let inner = Arc::new(self.inner);
        (
            DatagramSink {
                inner: Arc::clone(&inner),
                tx: self.tx,
                tx_buf: self.tx_buf,
            },
            DatagramSource {
                inner,
                rx: self.rx,
                rx_buf: self.rx_buf,
                invalid_dgrams: (0, 0, 0, 0),
                sim_loss: self.sim_loss,
            },
        )
    }
}

/// Shared swallow-and-count decrypt path for [`EncryptedDatagram`] and
/// [`DatagramSource`]: invalid datagrams increment `counters`
/// `(bad_token, aead, replay, too_old)` and return `Ok(None)`.
fn try_decrypt(
    rx: &mut DatagramDecrypt,
    dgram: &[u8],
    buf: &mut [u8],
    counters: &mut (u64, u64, u64, u64),
) -> Result<Option<(ChannelId, usize)>> {
    match rx.decrypt_into(dgram, buf) {
        Ok((channel, n)) => Ok(Some((channel, n))),
        Err(CryptError::BufferTooSmall { needed, .. }) => Err(TransportError::TooLarge {
            size: needed,
            max: buf.len(),
        }),
        Err(
            e @ (CryptError::BadToken
            | CryptError::Aead
            | CryptError::PacketTooShort(_)
            | CryptError::Replay { .. }
            | CryptError::TooOld { .. }),
        ) => {
            match e {
                CryptError::BadToken => counters.0 += 1,
                CryptError::Aead | CryptError::PacketTooShort(_) => counters.1 += 1,
                CryptError::Replay { .. } => counters.2 += 1,
                CryptError::TooOld { .. } => counters.3 += 1,
                _ => unreachable!("filtered above"),
            }
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

/// Send half of a split [`EncryptedDatagram`]: encrypts and sends, owns the
/// sequence counter. `Clone` is deliberately not implemented: one sender
/// task owns pacing and ordering.
pub struct DatagramSink<T: DatagramTransport> {
    inner: Arc<T>,
    tx: DatagramCrypt,
    tx_buf: Vec<u8>,
}

impl<T: DatagramTransport> DatagramSink<T> {
    /// Sends `payload` encrypted on `channel`, as a single datagram.
    pub async fn send(&mut self, channel: ChannelId, payload: &[u8]) -> Result<()> {
        if payload.len() > consts::MAX_DATAGRAM_PAYLOAD {
            return Err(TransportError::TooLarge {
                size: payload.len(),
                max: consts::MAX_DATAGRAM_PAYLOAD,
            });
        }
        let n = self.tx.encrypt_into(channel, payload, &mut self.tx_buf)?;
        self.inner.send(&self.tx_buf[..n]).await
    }
}

/// Receive half of a split [`EncryptedDatagram`]: owns the replay window and
/// the loss counters.
pub struct DatagramSource<T: DatagramTransport> {
    inner: Arc<T>,
    rx: DatagramDecrypt,
    rx_buf: Vec<u8>,
    invalid_dgrams: (u64, u64, u64, u64),
    /// Lab knob (§18.77), inherited from [`EncryptedDatagram::into_split`].
    sim_loss: (u64, u64),
}

impl<T: DatagramTransport> DatagramSource<T> {
    /// Receives the next valid datagram: channel id and plaintext length,
    /// copied into `buf`. Invalid datagrams are dropped (counted, see
    /// [`Self::dropped_invalid`]).
    pub async fn recv_into(&mut self, buf: &mut [u8]) -> Result<(ChannelId, usize)> {
        loop {
            let n = self.inner.recv_into(&mut self.rx_buf).await?;
            if sim_loss_hit(&mut self.sim_loss) {
                continue;
            }
            if let Some(result) = try_decrypt(
                &mut self.rx,
                &self.rx_buf[..n],
                buf,
                &mut self.invalid_dgrams,
            )? {
                return Ok(result);
            }
        }
    }

    /// Invalid datagrams swallowed so far: `(bad_token, aead, replay,
    /// too_old)` (§12.2 diagnostics blind spot).
    #[must_use]
    pub fn dropped_invalid(&self) -> (u64, u64, u64, u64) {
        self.invalid_dgrams
    }

    /// Packet loss estimate derived from sequence gaps (§12.2).
    #[must_use]
    pub fn lost_estimate(&self) -> u64 {
        self.rx.lost_estimate()
    }

    /// Valid datagrams received so far (§12.2).
    #[must_use]
    pub fn received(&self) -> u64 {
        self.rx.received()
    }
}

impl EncryptedDatagram<crate::UdpTransport> {
    /// Local socket address of the underlying UDP transport.
    pub fn local_addr(&self) -> Result<std::net::SocketAddr> {
        self.inner.local_addr()
    }
}
