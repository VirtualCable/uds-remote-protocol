//! Cifrado de stream (TCP): un paquete por mensaje, orden estricto.
//!
//! Formato en wire (estudio §6.1):
//!
//! ```text
//! | seq (8B BE) | len (2B BE) | channel_id (2B, cifrado) | ciphertext | tag (16B) |
//! ```
//!
//! - `len` cubre todo lo que sigue a la cabecera: `channel_id + ciphertext + tag`.
//! - Nonce: `seq` big-endian en los primeros 8B de 12 (resto ceros).
//! - AAD: la cabecera completa en claro (`seq || len`, 10B), de modo que una
//!   manipulación del framing se detecta antes de desincronizar el stream.
//! - Anti-replay estricto: se rechaza cualquier `seq` menor que el siguiente
//!   esperado. Claves unidireccionales (ver [`crate::keys`]).

use aes_gcm::aead::{AeadInOut, KeyInit, consts::U12};
use aes_gcm::{Aes256Gcm, Nonce, Tag};

use crate::consts::{CHANNEL_ID_SIZE, KEY_LENGTH, MAX_PLAINTEXT, STREAM_HEADER_SIZE, TAG_LENGTH};
use crate::error::CryptError;

fn nonce_from_seq(seq: u64) -> Nonce<U12> {
    let mut arr = [0u8; 12];
    arr[..8].copy_from_slice(&seq.to_be_bytes());
    Nonce::from(arr)
}

fn cipher_from_key(key: &[u8; KEY_LENGTH]) -> Aes256Gcm {
    Aes256Gcm::new_from_slice(key).unwrap_or_else(|_| unreachable!("KEY_LENGTH = 32 = AES-256"))
}

/// Cifrador de stream en una dirección (emisor).
pub struct StreamCrypt {
    cipher: Aes256Gcm,
    seq: u64,
}

impl StreamCrypt {
    /// Crea el cifrador con la clave de la dirección de envío.
    #[must_use]
    pub fn new(key: &[u8; KEY_LENGTH]) -> Self {
        Self {
            cipher: cipher_from_key(key),
            seq: 0,
        }
    }

    /// `seq` del próximo paquete a cifrar.
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Cifra `plaintext` para `channel_id` y devuelve el paquete completo en
    /// formato wire (cabecera en claro + channel_id cifrado + tag).
    pub fn encrypt(&mut self, channel_id: u16, plaintext: &[u8]) -> Result<Vec<u8>, CryptError> {
        if plaintext.len() > MAX_PLAINTEXT {
            return Err(CryptError::PacketTooLarge(plaintext.len()));
        }
        let seq = self.seq;
        let len = u16::try_from(CHANNEL_ID_SIZE + plaintext.len() + TAG_LENGTH)
            .map_err(|_| CryptError::PacketTooLarge(plaintext.len()))?;

        let mut packet = Vec::with_capacity(STREAM_HEADER_SIZE + len as usize);
        packet.extend_from_slice(&seq.to_be_bytes());
        packet.extend_from_slice(&len.to_be_bytes());
        packet.extend_from_slice(&channel_id.to_be_bytes());
        packet.extend_from_slice(plaintext);

        let (head, body) = packet.split_at_mut(STREAM_HEADER_SIZE);
        let tag = self
            .cipher
            .encrypt_inout_detached(&nonce_from_seq(seq), head, body.into())
            .map_err(|_| CryptError::Aead)?;
        packet.extend_from_slice(&tag);

        self.seq += 1;
        Ok(packet)
    }
}

/// Descifrador de stream en una dirección (receptor).
pub struct StreamDecrypt {
    cipher: Aes256Gcm,
    next_seq: u64,
}

impl StreamDecrypt {
    /// Crea el descifrador con la clave de la dirección de recepción.
    #[must_use]
    pub fn new(key: &[u8; KEY_LENGTH]) -> Self {
        Self {
            cipher: cipher_from_key(key),
            next_seq: 0,
        }
    }

    /// Próximo `seq` esperado.
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Verifica y descifra un paquete completo. Devuelve `(channel_id, plaintext)`.
    pub fn decrypt(&mut self, packet: &[u8]) -> Result<(u16, Vec<u8>), CryptError> {
        if packet.len() < STREAM_HEADER_SIZE + CHANNEL_ID_SIZE + TAG_LENGTH {
            return Err(CryptError::PacketTooShort(packet.len()));
        }
        let seq = u64::from_be_bytes(packet[..8].try_into().expect("slice de 8 bytes"));
        if seq < self.next_seq {
            return Err(CryptError::Replay {
                got: seq,
                expected: self.next_seq,
            });
        }
        let declared = u16::from_be_bytes(packet[8..10].try_into().expect("slice de 2 bytes"));
        if packet.len() != STREAM_HEADER_SIZE + declared as usize {
            // El framing no cuadra; el tag (que cubre `len`) fallaría igualmente,
            // pero rechazar aquí evita leer de más en un stream TCP.
            return Err(CryptError::Aead);
        }

        let (body, tag) = packet[STREAM_HEADER_SIZE..].split_at(declared as usize - TAG_LENGTH);
        let mut buf = body.to_vec();
        let tag: &Tag = tag.try_into().map_err(|_| CryptError::Aead)?;
        self.cipher
            .decrypt_inout_detached(
                &nonce_from_seq(seq),
                &packet[..STREAM_HEADER_SIZE],
                buf.as_mut_slice().into(),
                tag,
            )
            .map_err(|_| CryptError::Aead)?;

        self.next_seq = seq + 1;
        let channel_id = u16::from_be_bytes(buf[..2].try_into().expect("slice de 2 bytes"));
        buf.drain(..CHANNEL_ID_SIZE);
        Ok((channel_id, buf))
    }
}
