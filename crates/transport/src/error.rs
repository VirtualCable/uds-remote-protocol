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
}

/// Standard result of the crate.
pub type Result<T> = std::result::Result<T, TransportError>;
