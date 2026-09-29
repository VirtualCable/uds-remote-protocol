//! Errors of the `protocol` crate.

/// Protocol encoding/decoding errors.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    /// Handshake signature not recognized.
    #[error("bad handshake signature")]
    BadSignature,
    /// Unknown handshake command.
    #[error("unknown handshake command: {0:#04X}")]
    UnknownCommand(u8),
    /// Buffer too short for the message.
    #[error("truncated message")]
    Truncated,
    /// Malformed input that does not match the expected format.
    #[error("malformed: {0}")]
    Malformed(&'static str),
    /// Postcard serialization error.
    #[error("serialization error: {0}")]
    Serialization(#[from] postcard::Error),
}
