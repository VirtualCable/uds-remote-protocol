// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Latency probe messages (design doc §12.1): 2-step NTP-style exchange.
//!
//! 1. `A → B`: [`Probe`] `{ t1 }` (A's local monotonic timestamp).
//! 2. `B → A`: [`ProbeAck`] `{ t1, t2, t3 }` (t1 echoed + t2 arrival at B +
//!    t3 departure from B). **Replying is optional**: an overloaded peer
//!    drops the probe — and that absence is itself a congestion signal.
//!
//! On receiving the ack at `t4`, A computes
//! **RTT = (t4 − t1) − (t3 − t2)** — see [`compute_rtt`] — with no
//! synchronized clocks. Timestamps are opaque `u64` values (e.g. micros
//! from a local monotonic clock); this crate never reads a clock itself
//! (WASM-compatible).
//!
//! Wire format (fixed-size, no serde): tag byte + big-endian timestamps.

/// Wire tag for [`Probe`].
const TAG_PROBE: u8 = 1;
/// Wire tag for [`ProbeAck`].
const TAG_PROBE_ACK: u8 = 2;

/// Errors decoding probe messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProbeError {
    /// Buffer shorter than the fixed message size.
    #[error("truncated probe message")]
    Truncated,
    /// Unknown message tag byte.
    #[error("unknown probe tag: {0}")]
    UnknownTag(u8),
}

/// Latency probe: the sender stamps its local `t1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    /// Sender's local monotonic timestamp at send time.
    pub t1: u64,
}

impl Probe {
    /// Encoded size: tag(1) + t1(8).
    pub const ENCODED_LEN: usize = 9;

    /// Encodes to the fixed wire format.
    #[must_use]
    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[0] = TAG_PROBE;
        out[1..].copy_from_slice(&self.t1.to_be_bytes());
        out
    }

    /// Decodes from the wire format.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProbeError> {
        if bytes.len() < Self::ENCODED_LEN {
            return Err(ProbeError::Truncated);
        }
        if bytes[0] != TAG_PROBE {
            return Err(ProbeError::UnknownTag(bytes[0]));
        }
        Ok(Self {
            t1: u64::from_be_bytes(bytes[1..9].try_into().expect("8-byte slice")),
        })
    }
}

/// Probe reply: echoes `t1` and adds B's arrival (`t2`) and departure (`t3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeAck {
    /// Echoed sender timestamp.
    pub t1: u64,
    /// Receiver's local timestamp when the probe arrived.
    pub t2: u64,
    /// Receiver's local timestamp when the ack was sent.
    pub t3: u64,
}

impl ProbeAck {
    /// Encoded size: tag(1) + 3 timestamps(24).
    pub const ENCODED_LEN: usize = 25;

    /// Encodes to the fixed wire format.
    #[must_use]
    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[0] = TAG_PROBE_ACK;
        out[1..9].copy_from_slice(&self.t1.to_be_bytes());
        out[9..17].copy_from_slice(&self.t2.to_be_bytes());
        out[17..25].copy_from_slice(&self.t3.to_be_bytes());
        out
    }

    /// Decodes from the wire format.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProbeError> {
        if bytes.len() < Self::ENCODED_LEN {
            return Err(ProbeError::Truncated);
        }
        if bytes[0] != TAG_PROBE_ACK {
            return Err(ProbeError::UnknownTag(bytes[0]));
        }
        Ok(Self {
            t1: u64::from_be_bytes(bytes[1..9].try_into().expect("8-byte slice")),
            t2: u64::from_be_bytes(bytes[9..17].try_into().expect("8-byte slice")),
            t3: u64::from_be_bytes(bytes[17..25].try_into().expect("8-byte slice")),
        })
    }
}

/// NTP-style RTT: `RTT = (t4 − t1) − (t3 − t2)`.
///
/// Saturates to 0 on clock anomalies (e.g. `t4 < t1` or `t3 < t2`) instead
/// of wrapping around.
#[must_use]
pub fn compute_rtt(t1: u64, t2: u64, t3: u64, t4: u64) -> u64 {
    t4.saturating_sub(t1).saturating_sub(t3.saturating_sub(t2))
}
