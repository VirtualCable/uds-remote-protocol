//! Derivación de claves de sesión con HKDF-SHA256.

use hkdf::Hkdf;
use sha2::Sha256;

use crate::consts::KEY_LENGTH;
use crate::error::CryptError;

/// Label de separación de dominio para el stream TCP.
pub const STREAM_INFO: &[u8] = b"crypt";
/// Label de separación de dominio para datagramas UDP.
pub const DATAGRAM_INFO: &[u8] = b"crypt-udp";

/// Claves unidireccionales de sesión (cada dirección tiene clave y contador
/// `seq` propios, de modo que jamás se repite el par `(clave, nonce)`).
#[derive(Clone, PartialEq, Eq)]
pub struct SessionKeys {
    /// Clave cliente → servidor.
    pub c2s: [u8; KEY_LENGTH],
    /// Clave servidor → cliente.
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

/// Deriva las claves de sesión para el stream TCP:
/// `HKDF-SHA256(salt = ticket, ikm = shared_secret, info = "crypt")`.
pub fn derive_session_keys(
    ticket: &[u8; crate::consts::TICKET_LENGTH],
    shared_secret: &[u8; KEY_LENGTH],
) -> Result<SessionKeys, CryptError> {
    let hk = Hkdf::<Sha256>::new(Some(ticket), shared_secret);
    SessionKeys::expand(&hk, STREAM_INFO)
}

/// Deriva las claves de sesión para datagramas UDP (dominio separado).
pub fn derive_datagram_keys(
    ticket: &[u8; crate::consts::TICKET_LENGTH],
    shared_secret: &[u8; KEY_LENGTH],
) -> Result<SessionKeys, CryptError> {
    let hk = Hkdf::<Sha256>::new(Some(ticket), shared_secret);
    SessionKeys::expand(&hk, DATAGRAM_INFO)
}

/// Avance simétrico de clave (ratchet) para el rekeying periódico:
/// `HKDF-SHA256(salt = nil, ikm = clave_actual, info = "crypt-rekey")`.
pub fn ratchet_key(key: &[u8; KEY_LENGTH]) -> Result<[u8; KEY_LENGTH], CryptError> {
    let hk = Hkdf::<Sha256>::new(None, key);
    let mut next = [0u8; KEY_LENGTH];
    hk.expand(b"crypt-rekey", &mut next)
        .map_err(|_| CryptError::KeyDerivation)?;
    Ok(next)
}
