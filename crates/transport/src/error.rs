// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Errors of the `transport` crate.

use protocol::ProtocolError;

/// Transport and encrypted stream layer errors.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// I/O error of the underlying socket.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Protocol error (invalid Hello handshake, etc.).
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    /// Cryptographic error (decryption, anti-replay, etc.).
    #[error("encryption error: {0}")]
    Crypt(#[from] crypt::CryptError),
    /// The payload exceeds the maximum size of a record.
    #[error("payload too large: {size} bytes (max {max})")]
    TooLarge {
        /// Attempted payload size.
        size: usize,
        /// Maximum allowed.
        max: usize,
    },
    /// The peer closed the connection.
    #[error("connection closed by peer")]
    Closed,
    /// Received a valid but unexpected control message at this point.
    #[error("unexpected control message: {0}")]
    UnexpectedMessage(String),
    /// The peer's protocol version differs from ours. Wire formats change
    /// between versions (e.g. v2 added `capture_ts_ms` to the fragment
    /// header, §9.1), so a mismatch must fail loudly, never parse silently.
    #[error("protocol version mismatch: peer speaks {got}, we speak {expected}")]
    VersionMismatch {
        /// Our protocol version.
        expected: u16,
        /// The peer's protocol version.
        got: u16,
    },
    /// The client failed the possession proof (wrong PSK or tampering).
    /// The server must treat this as an authentication failure: no session
    /// side effects (autologon, agent spawn) may follow.
    #[error("possession proof failed")]
    ProofFailed,
}

/// Standard result of the crate.
pub type Result<T> = std::result::Result<T, TransportError>;
