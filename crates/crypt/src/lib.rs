//! # crypt
//!
//! Protocol cryptography, following the UDS tunnel-server model:
//!
//! - AES-256-GCM per packet (the 16B tag acts as MAC; no separate HMAC).
//! - Nonce derived from a monotonic `seq` counter; strict anti-replay on the
//!   stream and sliding-window anti-replay on datagrams.
//! - Key derivation with HKDF-SHA256 (salt = 48B ticket), with
//!   unidirectional keys (`c2s`/`s2c`) and a separate domain for UDP.
//! - Periodic rekeying via HKDF ratchet (NIST AES-GCM invocation limit).
//! - ML-KEM-768 for the secret exchange (later phase).

pub mod consts;
mod datagram;
mod error;
mod keys;
mod stream;

pub use datagram::{DatagramCrypt, DatagramDecrypt};
pub use error::CryptError;
pub use keys::{
    PROOF_LENGTH, SessionKeys, derive_datagram_keys, derive_session_keys, derive_udp_token,
    possession_proof, ratchet_key,
};
pub use stream::{StreamCrypt, StreamDecrypt};
