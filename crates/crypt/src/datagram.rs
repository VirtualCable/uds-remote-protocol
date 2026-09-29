//! Datagram encryption (UDP): self-contained packets, loss- and
//! reorder-tolerant (design doc §6.2).
//!
//! Wire format:
//!
//! ```text
//! | token (16B) | seq (8B BE) | channel_id (2B, encrypted) | ciphertext | tag (16B) |
//! ```
//!
//! - `token` binds the datagram to a session (demultiplexing at the server,
//!   and at a future gateway relay) and travels in the clear.
//! - Nonce: `seq` big-endian in the first 8 of 12 bytes (rest zeroes).
//! - AAD: the full cleartext header (`token || seq`, 24B).
//! - Anti-replay: 64-entry sliding window (reorder-tolerant), plus a packet
//!   loss estimate derived from sequence gaps (congestion signal, §12.2).
//! - `seq` starts at 0; the MSB is reserved as `key_phase` for future
//!   rekeying (§6.3) and is always 0 for now.

use aes_gcm::aead::{AeadInOut, KeyInit, consts::U12};
use aes_gcm::{Aes256Gcm, Nonce, Tag};

use crate::consts::{
    CHANNEL_ID_SIZE, DATAGRAM_HEADER_SIZE, KEY_LENGTH, MAX_DATAGRAM_PAYLOAD, TAG_LENGTH,
    TOKEN_LENGTH,
};
use crate::error::CryptError;

fn nonce_from_seq(seq: u64) -> Nonce<U12> {
    let mut arr = [0u8; 12];
    arr[..8].copy_from_slice(&seq.to_be_bytes());
    Nonce::from(arr)
}

fn cipher_from_key(key: &[u8; KEY_LENGTH]) -> Aes256Gcm {
    Aes256Gcm::new_from_slice(key).unwrap_or_else(|_| unreachable!("KEY_LENGTH = 32 = AES-256"))
}

/// One-way datagram encryptor (sender side).
pub struct DatagramCrypt {
    cipher: Aes256Gcm,
    token: [u8; TOKEN_LENGTH],
    seq: u64,
}

impl DatagramCrypt {
    /// Creates the encryptor with the send-direction key and session token.
    #[must_use]
    pub fn new(key: &[u8; KEY_LENGTH], token: &[u8; TOKEN_LENGTH]) -> Self {
        Self {
            cipher: cipher_from_key(key),
            token: *token,
            seq: 0,
        }
    }

    /// `seq` of the next datagram to encrypt.
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Encrypts `plaintext` for `channel_id`, returning the complete wire
    /// datagram (cleartext header + encrypted channel_id + tag).
    pub fn encrypt(&mut self, channel_id: u16, plaintext: &[u8]) -> Result<Vec<u8>, CryptError> {
        if plaintext.len() > MAX_DATAGRAM_PAYLOAD {
            return Err(CryptError::PacketTooLarge(plaintext.len()));
        }
        let seq = self.seq;

        let mut dgram =
            Vec::with_capacity(DATAGRAM_HEADER_SIZE + CHANNEL_ID_SIZE + plaintext.len() + TAG_LENGTH);
        dgram.extend_from_slice(&self.token);
        dgram.extend_from_slice(&seq.to_be_bytes());
        dgram.extend_from_slice(&channel_id.to_be_bytes());
        dgram.extend_from_slice(plaintext);

        let (head, body) = dgram.split_at_mut(DATAGRAM_HEADER_SIZE);
        let tag = self
            .cipher
            .encrypt_inout_detached(&nonce_from_seq(seq), head, body.into())
            .map_err(|_| CryptError::Aead)?;
        dgram.extend_from_slice(&tag);

        self.seq += 1;
        Ok(dgram)
    }
}

/// One-way datagram decryptor (receiver side).
pub struct DatagramDecrypt {
    cipher: Aes256Gcm,
    token: [u8; TOKEN_LENGTH],
    window: ReplayWindow,
}

impl DatagramDecrypt {
    /// Creates the decryptor with the receive-direction key and session token.
    #[must_use]
    pub fn new(key: &[u8; KEY_LENGTH], token: &[u8; TOKEN_LENGTH]) -> Self {
        Self {
            cipher: cipher_from_key(key),
            token: *token,
            window: ReplayWindow::default(),
        }
    }

    /// Estimate of lost packets so far, derived from sequence gaps
    /// (congestion signal, §12.2). Late arrivals discount themselves.
    #[must_use]
    pub fn lost_estimate(&self) -> u64 {
        self.window.lost
    }

    /// Verifies and decrypts a complete datagram.
    /// Returns `(channel_id, plaintext)`.
    pub fn decrypt(&mut self, dgram: &[u8]) -> Result<(u16, Vec<u8>), CryptError> {
        if dgram.len() < DATAGRAM_HEADER_SIZE + CHANNEL_ID_SIZE + TAG_LENGTH {
            return Err(CryptError::PacketTooShort(dgram.len()));
        }
        if dgram[..TOKEN_LENGTH] != self.token {
            return Err(CryptError::BadToken);
        }
        let seq = u64::from_be_bytes(
            dgram[TOKEN_LENGTH..DATAGRAM_HEADER_SIZE]
                .try_into()
                .expect("8-byte slice"),
        );

        let (body, tag) = dgram[DATAGRAM_HEADER_SIZE..].split_at(dgram.len() - DATAGRAM_HEADER_SIZE - TAG_LENGTH);
        let mut buf = body.to_vec();
        let tag: &Tag = tag.try_into().map_err(|_| CryptError::Aead)?;
        self.cipher
            .decrypt_inout_detached(
                &nonce_from_seq(seq),
                &dgram[..DATAGRAM_HEADER_SIZE],
                buf.as_mut_slice().into(),
                tag,
            )
            .map_err(|_| CryptError::Aead)?;

        // Only mark the window after AEAD verification, so a forged packet
        // can never burn a replay slot.
        self.window.accept(seq)?;

        let channel_id = u16::from_be_bytes(buf[..2].try_into().expect("2-byte slice"));
        buf.drain(..CHANNEL_ID_SIZE);
        Ok((channel_id, buf))
    }
}

/// 64-entry sliding anti-replay window with loss estimation.
///
/// Bit `i` of `bitmap` tracks whether `highest - i` was received.
#[derive(Default)]
struct ReplayWindow {
    highest: Option<u64>,
    bitmap: u64,
    lost: u64,
}

impl ReplayWindow {
    const SIZE: u64 = 64;

    fn accept(&mut self, seq: u64) -> Result<(), CryptError> {
        match self.highest {
            None => {
                self.highest = Some(seq);
                self.bitmap = 1;
            }
            Some(highest) if seq > highest => {
                // Sequence numbers skipped between `highest` and `seq` have
                // never been seen: count them as lost (a late arrival would
                // discount itself below).
                self.lost += seq - highest - 1;
                let shift = (seq - highest).min(Self::SIZE);
                self.bitmap = (self.bitmap << shift) | 1;
                self.highest = Some(seq);
            }
            Some(highest) => {
                let offset = highest - seq;
                if offset >= Self::SIZE {
                    return Err(CryptError::TooOld {
                        seq,
                        window_base: highest + 1 - Self::SIZE,
                    });
                }
                let mask = 1 << offset;
                if self.bitmap & mask != 0 {
                    return Err(CryptError::Replay {
                        got: seq,
                        expected: highest + 1,
                    });
                }
                self.bitmap |= mask;
                // Late arrival: it was jitter, not loss.
                self.lost = self.lost.saturating_sub(1);
            }
        }
        Ok(())
    }
}
