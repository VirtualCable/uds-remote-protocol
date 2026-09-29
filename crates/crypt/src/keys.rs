//! Session key derivation with HKDF-SHA256.

use hkdf::Hkdf;
use sha2::Sha256;

use crate::consts::KEY_LENGTH;
use crate::error::CryptError;

/// Domain separation label for the TCP stream.
pub const STREAM_INFO: &[u8] = b"crypt";
/// Domain separation label for UDP datagrams.
pub const DATAGRAM_INFO: &[u8] = b"crypt-udp";

/// Unidirectional session keys (each direction has its own key and `seq`
/// counter, so the `(key, nonce)` pair is never repeated).
#[derive(Clone, PartialEq, Eq)]
pub struct SessionKeys {
    /// Client → server key.
    pub c2s: [u8; KEY_LENGTH],
    /// Server → client key.
    pub s2c: [u8; KEY_LENGTH],
}

impl SessionKeys {
    fn expand(hk: &Hkdf<Sha256>, info: &[u8]) -> Result<Self, CryptError> {
        let mut okm = [0u8; 2 * KEY_LENGTH];
        hk.expand(info, &mut okm).map_err(|_| CryptError::KeyDerivation)?;
        let mut c2s = [0u8; KEY_LENGTH];
        let mut s2c = [0u8; KEY_LENGTH];
        c2s.copy_from_slice(&okm[..KEY_LENGTH]);
        s2c.copy_from_slice(&okm[KEY_LENGTH..]);
        Ok(Self { c2s, s2c })
    }
}

impl std::fmt::Debug for SessionKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionKeys").finish_non_exhaustive()
    }
}

/// Derives the session keys for the TCP stream:
/// `HKDF-SHA256(salt = ticket, ikm = shared_secret, info = "crypt")`.
pub fn derive_session_keys(
    ticket: &[u8; crate::consts::TICKET_LENGTH],
    shared_secret: &[u8; KEY_LENGTH],
) -> Result<SessionKeys, CryptError> {
    let hk = Hkdf::<Sha256>::new(Some(ticket), shared_secret);
    SessionKeys::expand(&hk, STREAM_INFO)
}

/// Derives the session keys for UDP datagrams (separate domain).
pub fn derive_datagram_keys(
    ticket: &[u8; crate::consts::TICKET_LENGTH],
    shared_secret: &[u8; KEY_LENGTH],
) -> Result<SessionKeys, CryptError> {
    let hk = Hkdf::<Sha256>::new(Some(ticket), shared_secret);
    SessionKeys::expand(&hk, DATAGRAM_INFO)
}

/// Symmetric key advance (ratchet) for periodic rekeying:
/// `HKDF-SHA256(salt = nil, ikm = current_key, info = "crypt-rekey")`.
pub fn ratchet_key(key: &[u8; KEY_LENGTH]) -> Result<[u8; KEY_LENGTH], CryptError> {
    let hk = Hkdf::<Sha256>::new(None, key);
    let mut next = [0u8; KEY_LENGTH];
    hk.expand(b"crypt-rekey", &mut next)
        .map_err(|_| CryptError::KeyDerivation)?;
    Ok(next)
}

/// Derives the UDP session token (§4.2/§6.2):
/// `HKDF-SHA256(salt = ticket, ikm = shared_secret, info = "crypt-udp-token")`,
/// first 16 bytes.
///
/// The token binds datagrams to a session. In direct mode both ends derive
/// it, so it never travels over the control channel (the `UdpOffer` message
/// still carries it explicitly for flexibility, e.g. a future tunnel leg
/// where the token is assigned by the relay instead).
pub fn derive_udp_token(
    ticket: &[u8; crate::consts::TICKET_LENGTH],
    shared_secret: &[u8; KEY_LENGTH],
) -> Result<[u8; crate::consts::TOKEN_LENGTH], CryptError> {
    let hk = Hkdf::<Sha256>::new(Some(ticket), shared_secret);
    let mut token = [0u8; crate::consts::TOKEN_LENGTH];
    hk.expand(b"crypt-udp-token", &mut token)
        .map_err(|_| CryptError::KeyDerivation)?;
    Ok(token)
}
