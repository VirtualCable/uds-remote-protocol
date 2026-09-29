//! Errors of the `crypt` crate.

/// Encryption, decryption and cryptographic framing errors.
#[derive(Debug, thiserror::Error)]
pub enum CryptError {
    /// AEAD failure: invalid tag, tampered AAD or wrong key.
    #[error("AEAD verification failed")]
    Aead,
    /// Repeated packet or older than the last processed `seq`.
    #[error("replay detected: got seq {got}, expected >= {expected}")]
    Replay {
        /// Received `seq`.
        got: u64,
        /// Minimum acceptable `seq`.
        expected: u64,
    },
    /// Packet too short to contain header + tag.
    #[error("packet too short: {0} bytes")]
    PacketTooShort(usize),
    /// Payload too large for the 16-bit `len` field.
    #[error("packet too large: {0} bytes of plaintext")]
    PacketTooLarge(usize),
    /// HKDF could not expand the requested material.
    #[error("key derivation failed")]
    KeyDerivation,
    /// The datagram session token does not match the expected one.
    #[error("datagram token mismatch")]
    BadToken,
    /// Datagram older than the anti-replay window (no longer verifiable).
    #[error("datagram too old: seq {seq}, window base {window_base}")]
    TooOld {
        /// Received `seq`.
        seq: u64,
        /// Current base of the sliding window.
        window_base: u64,
    },
}
