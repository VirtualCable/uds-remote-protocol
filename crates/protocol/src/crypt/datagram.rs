// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

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

use crate::crypt::consts::{
    CHANNEL_ID_SIZE, DATAGRAM_HEADER_SIZE, KEY_LENGTH, MAX_DATAGRAM_PAYLOAD, TAG_LENGTH,
    TOKEN_LENGTH,
};
use crate::crypt::error::CryptError;

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
    ///
    /// Allocating convenience wrapper; the hot path uses
    /// [`DatagramCrypt::encrypt_into`] with a reused buffer (§12.6).
    pub fn encrypt(&mut self, channel_id: u16, plaintext: &[u8]) -> Result<Vec<u8>, CryptError> {
        let mut dgram =
            vec![0u8; DATAGRAM_HEADER_SIZE + CHANNEL_ID_SIZE + plaintext.len() + TAG_LENGTH];
        let n = self.encrypt_into(channel_id, plaintext, &mut dgram)?;
        debug_assert_eq!(n, dgram.len());
        Ok(dgram)
    }

    /// Encrypts `plaintext` for `channel_id` into `out`, returning the wire
    /// datagram length. No allocations: for the datagram hot path (§12.6).
    pub fn encrypt_into(
        &mut self,
        channel_id: u16,
        plaintext: &[u8],
        out: &mut [u8],
    ) -> Result<usize, CryptError> {
        if plaintext.len() > MAX_DATAGRAM_PAYLOAD {
            return Err(CryptError::PacketTooLarge(plaintext.len()));
        }
        let total = DATAGRAM_HEADER_SIZE + CHANNEL_ID_SIZE + plaintext.len() + TAG_LENGTH;
        if out.len() < total {
            return Err(CryptError::BufferTooSmall {
                needed: total,
                have: out.len(),
            });
        }
        let seq = self.seq;
        let dgram = &mut out[..total];
        dgram[..TOKEN_LENGTH].copy_from_slice(&self.token);
        dgram[TOKEN_LENGTH..DATAGRAM_HEADER_SIZE].copy_from_slice(&seq.to_be_bytes());
        dgram[DATAGRAM_HEADER_SIZE..DATAGRAM_HEADER_SIZE + CHANNEL_ID_SIZE]
            .copy_from_slice(&channel_id.to_be_bytes());
        dgram[DATAGRAM_HEADER_SIZE + CHANNEL_ID_SIZE
            ..DATAGRAM_HEADER_SIZE + CHANNEL_ID_SIZE + plaintext.len()]
            .copy_from_slice(plaintext);

        let (head, rest) = dgram.split_at_mut(DATAGRAM_HEADER_SIZE);
        let body_len = rest.len() - TAG_LENGTH;
        let (body, tag_slot) = rest.split_at_mut(body_len);
        let tag = self
            .cipher
            .encrypt_inout_detached(&nonce_from_seq(seq), head, body.into())
            .map_err(|_| CryptError::Aead)?;
        tag_slot.copy_from_slice(&tag);

        self.seq += 1;
        Ok(total)
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

    /// Valid datagrams accepted so far; combined with
    /// [`DatagramDecrypt::lost_estimate`] it gives a loss *rate*
    /// (`lost / (lost + received)`) over any window (§12.2).
    #[must_use]
    pub fn received(&self) -> u64 {
        self.window.accepted
    }

    /// Verifies and decrypts a complete datagram.
    /// Returns `(channel_id, plaintext)`.
    ///
    /// Allocating convenience wrapper; the hot path uses
    /// [`DatagramDecrypt::decrypt_into`] with a reused buffer (§12.6).
    pub fn decrypt(&mut self, dgram: &[u8]) -> Result<(u16, Vec<u8>), CryptError> {
        let mut out = vec![0u8; dgram.len()];
        let (channel_id, n) = self.decrypt_into(dgram, &mut out)?;
        out.truncate(n);
        Ok((channel_id, out))
    }

    /// Verifies and decrypts a complete datagram into `out`, returning
    /// `(channel_id, plaintext_len)` with the plaintext at `out[..n]`.
    /// No allocations: for the datagram hot path (§12.6).
    pub fn decrypt_into(
        &mut self,
        dgram: &[u8],
        out: &mut [u8],
    ) -> Result<(u16, usize), CryptError> {
        if dgram.len() < DATAGRAM_HEADER_SIZE + CHANNEL_ID_SIZE + TAG_LENGTH {
            return Err(CryptError::PacketTooShort(dgram.len()));
        }
        if dgram[..TOKEN_LENGTH] != self.token {
            return Err(CryptError::BadToken);
        }
        let body_len = dgram.len() - DATAGRAM_HEADER_SIZE - TAG_LENGTH;
        if out.len() < body_len {
            return Err(CryptError::BufferTooSmall {
                needed: body_len,
                have: out.len(),
            });
        }
        let seq = u64::from_be_bytes(
            dgram[TOKEN_LENGTH..DATAGRAM_HEADER_SIZE]
                .try_into()
                .expect("8-byte slice"),
        );

        let (body, tag) = dgram[DATAGRAM_HEADER_SIZE..].split_at(body_len);
        let tag: &Tag = tag.try_into().map_err(|_| CryptError::Aead)?;
        let plain = &mut out[..body_len];
        plain.copy_from_slice(body);
        self.cipher
            .decrypt_inout_detached(
                &nonce_from_seq(seq),
                &dgram[..DATAGRAM_HEADER_SIZE],
                plain.into(),
                tag,
            )
            .map_err(|_| CryptError::Aead)?;

        // Only mark the window after AEAD verification, so a forged packet
        // can never burn a replay slot.
        self.window.accept(seq)?;

        let channel_id = u16::from_be_bytes(plain[..2].try_into().expect("2-byte slice"));
        plain.copy_within(CHANNEL_ID_SIZE.., 0);
        Ok((channel_id, body_len - CHANNEL_ID_SIZE))
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
    accepted: u64,
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
                let shift = seq - highest;
                self.bitmap = if shift >= Self::SIZE {
                    // Gap wider than the window: every tracked bit is stale.
                    1
                } else {
                    (self.bitmap << shift) | 1
                };
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
        self.accepted += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ReplayWindow;

    #[test]
    fn gap_wider_than_window_resets_bitmap() {
        let mut w = ReplayWindow::default();
        w.accept(10).unwrap();
        // Jump more than SIZE entries ahead: must not panic on shift.
        w.accept(10 + ReplayWindow::SIZE + 5).unwrap();
        assert_eq!(w.bitmap, 1);
        // An old sequence inside the stale region is now simply too old.
        assert!(w.accept(11).is_err());
    }

    #[test]
    fn exact_window_gap_resets_bitmap() {
        let mut w = ReplayWindow::default();
        w.accept(0).unwrap();
        w.accept(ReplayWindow::SIZE).unwrap();
        assert_eq!(w.bitmap, 1);
    }
}
