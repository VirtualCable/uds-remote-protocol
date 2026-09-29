//! # crypt
//!
//! Criptografía del protocolo, siguiendo el modelo de tunnel-server UDS:
//!
//! - AES-256-GCM por paquete (el tag de 16B hace de MAC; no hay HMAC aparte).
//! - Nonce derivado de contador `seq` monotónico; anti-replay estricto en
//!   stream y por ventana deslizante en datagramas.
//! - Derivación de claves con HKDF-SHA256 (salt = ticket de 48B), con claves
//!   unidireccionales (`c2s`/`s2c`) y dominio separado para UDP.
//! - Rekeying periódico por ratchet HKDF (límite NIST de invocaciones AES-GCM).
//! - ML-KEM-768 para el intercambio de secreto (fase posterior).

pub mod consts;
mod error;
mod keys;
mod stream;

pub use error::CryptError;
pub use keys::{SessionKeys, derive_datagram_keys, derive_session_keys, ratchet_key};
pub use stream::{StreamCrypt, StreamDecrypt};
