//! Stream encryption (TCP): one packet per message, strict ordering.
//!
//! Wire format (design doc §6.1):
//!
//! ```text
//! | seq (8B BE) | len (2B BE) | channel_id (2B, encrypted) | ciphertext | tag (16B) |
//! ```
//!
//! - `len` covers everything after the header: `channel_id + ciphertext + tag`.
//! - Nonce: `seq` big-endian in the first 8B of 12 (rest zeros).
//! - AAD: the full cleartext header (`seq || len`, 10B), so any tampering
//!   with the framing is detected before the stream desynchronizes.
//! - Strict anti-replay: any `seq` lower than the next expected one is
//!   rejected. Unidirectional keys (see [`crate::keys`]).

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

/// Stream encryptor in one direction (sender).
pub struct StreamCrypt {
    cipher: Aes256Gcm,
    seq: u64,
}

impl StreamCrypt {
    /// Creates the encryptor with the sending-direction key.
    #[must_use]
    pub fn new(key: &[u8; KEY_LENGTH]) -> Self {
        Self::with_seq(key, 0)
    }

    /// Creates the encryptor resuming from a known `seq` (recovery, §4.3).
    /// Never pass a `seq` lower than one already used with this key: the
    /// `(key, nonce)` pair must never repeat.
    #[must_use]
    pub fn with_seq(key: &[u8; KEY_LENGTH], seq: u64) -> Self {
        Self {
            cipher: cipher_from_key(key),
            seq,
        }
    }

    /// `seq` of the next packet to encrypt.
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Encrypts `plaintext` for `channel_id` and returns the full packet in
    /// wire format (cleartext header + encrypted channel_id + tag).
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

/// Stream decryptor in one direction (receiver).
pub struct StreamDecrypt {
    cipher: Aes256Gcm,
    next_seq: u64,
}

impl StreamDecrypt {
    /// Creates the decryptor with the receiving-direction key.
    #[must_use]
    pub fn new(key: &[u8; KEY_LENGTH]) -> Self {
        Self::with_seq(key, 0)
    }

    /// Creates the decryptor resuming from a known `next_seq` (recovery, §4.3).
    #[must_use]
    pub fn with_seq(key: &[u8; KEY_LENGTH], next_seq: u64) -> Self {
        Self {
            cipher: cipher_from_key(key),
            next_seq,
        }
    }

    /// Next expected `seq`.
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Verifies and decrypts a full packet. Returns `(channel_id, plaintext)`.
    pub fn decrypt(&mut self, packet: &[u8]) -> Result<(u16, Vec<u8>), CryptError> {
        if packet.len() < STREAM_HEADER_SIZE + CHANNEL_ID_SIZE + TAG_LENGTH {
            return Err(CryptError::PacketTooShort(packet.len()));
        }
        let seq = u64::from_be_bytes(packet[..8].try_into().expect("8-byte slice"));
        if seq < self.next_seq {
            return Err(CryptError::Replay {
                got: seq,
                expected: self.next_seq,
            });
        }
        let declared = u16::from_be_bytes(packet[8..10].try_into().expect("2-byte slice"));
        if packet.len() != STREAM_HEADER_SIZE + declared as usize {
            // The framing does not add up; the tag (which covers `len`) would fail
            // anyway, but rejecting here avoids over-reading a TCP stream.
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
        let channel_id = u16::from_be_bytes(buf[..2].try_into().expect("2-byte slice"));
        buf.drain(..CHANNEL_ID_SIZE);
        Ok((channel_id, buf))
    }
}
