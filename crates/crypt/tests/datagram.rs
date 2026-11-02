// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Datagram encryption tests (design doc §6.2): wire format
//! `|token 16B|seq 8B BE|channel_id encrypted|payload|tag 16B|`, AAD = token‖seq,
//! sliding-window anti-replay and loss estimation from sequence gaps.

use crypt::{CryptError, DatagramCrypt, DatagramDecrypt, consts, derive_datagram_keys};

const TOKEN: [u8; consts::TOKEN_LENGTH] = [0xA5; consts::TOKEN_LENGTH];
const TICKET: [u8; consts::TICKET_LENGTH] = [0x11; consts::TICKET_LENGTH];
const SECRET: [u8; consts::KEY_LENGTH] = [0x22; consts::KEY_LENGTH];

fn pair() -> (DatagramCrypt, DatagramDecrypt) {
    let keys = derive_datagram_keys(&TICKET, &SECRET).unwrap();
    (
        DatagramCrypt::new(&keys.c2s, &TOKEN),
        DatagramDecrypt::new(&keys.c2s, &TOKEN),
    )
}

#[test]
fn datagram_roundtrip() {
    let (mut tx, mut rx) = pair();
    let dgram = tx.encrypt(7, b"audio-opus-packet").unwrap();
    let (channel, plain) = rx.decrypt(&dgram).unwrap();
    assert_eq!(channel, 7);
    assert_eq!(plain, b"audio-opus-packet");
}

#[test]
fn wire_format_layout() {
    let (mut tx, _rx) = pair();
    let dgram = tx.encrypt(3, b"abcd").unwrap();
    // token(16) | seq(8) | channel_id(2, encrypted) | payload(4) | tag(16)
    assert_eq!(dgram.len(), 16 + 8 + 2 + 4 + 16);
    assert_eq!(&dgram[..16], &TOKEN);
    assert_eq!(&dgram[16..24], &0u64.to_be_bytes()); // first seq = 0
    // channel_id is encrypted: it must NOT be visible in cleartext
    assert_ne!(&dgram[24..26], &3u16.to_be_bytes());
}

#[test]
fn wrong_token_is_rejected() {
    let (mut tx, _rx) = pair();
    let keys = derive_datagram_keys(&TICKET, &SECRET).unwrap();
    let mut rx = DatagramDecrypt::new(&keys.c2s, &[0x5A; consts::TOKEN_LENGTH]);
    let dgram = tx.encrypt(1, b"data").unwrap();
    assert!(matches!(rx.decrypt(&dgram), Err(CryptError::BadToken)));
}

#[test]
fn tampered_token_breaks_aad() {
    let (mut tx, mut rx) = pair();
    let mut dgram = tx.encrypt(1, b"data").unwrap();
    dgram[0] ^= 0xFF; // token tampered in transit
    assert!(rx.decrypt(&dgram).is_err());
}

#[test]
fn out_of_order_within_window_is_accepted() {
    let (mut tx, mut rx) = pair();
    let d0 = tx.encrypt(1, b"zero").unwrap();
    let d1 = tx.encrypt(1, b"one").unwrap();
    let d2 = tx.encrypt(1, b"two").unwrap();
    // Arriving out of order: 2, 0, 1 — all must be accepted
    assert_eq!(rx.decrypt(&d2).unwrap().1, b"two");
    assert_eq!(rx.decrypt(&d0).unwrap().1, b"zero");
    assert_eq!(rx.decrypt(&d1).unwrap().1, b"one");
}

#[test]
fn duplicate_is_rejected_as_replay() {
    let (mut tx, mut rx) = pair();
    let d = tx.encrypt(1, b"only once").unwrap();
    rx.decrypt(&d).unwrap();
    assert!(matches!(rx.decrypt(&d), Err(CryptError::Replay { .. })));
}

#[test]
fn too_old_is_rejected() {
    let (mut tx, mut rx) = pair();
    let old = tx.encrypt(1, b"old").unwrap();
    // Move the window past 64 packets
    for i in 0..70 {
        let d = tx.encrypt(1, format!("p{i}").as_bytes()).unwrap();
        rx.decrypt(&d).unwrap();
    }
    assert!(matches!(rx.decrypt(&old), Err(CryptError::TooOld { .. })));
}

#[test]
fn lost_estimate_counts_gaps() {
    let (mut tx, mut rx) = pair();
    let mut dgrams = Vec::new();
    for i in 0..10 {
        dgrams.push(tx.encrypt(1, format!("p{i}").as_bytes()).unwrap());
    }
    // Sequences 3 and 7 are "lost": 0,1,2,4,5,6,8,9 arrive
    for (i, d) in dgrams.iter().enumerate() {
        if i != 3 && i != 7 {
            rx.decrypt(d).unwrap();
        }
    }
    assert_eq!(rx.lost_estimate(), 2);
    assert_eq!(rx.received(), 8);
}

#[test]
fn received_counts_only_accepted_datagrams() {
    let (mut tx, mut rx) = pair();
    let d0 = tx.encrypt(1, b"a").unwrap();
    let d1 = tx.encrypt(1, b"b").unwrap();
    rx.decrypt(&d0).unwrap();
    rx.decrypt(&d1).unwrap();
    // Replay does not count as received.
    assert!(rx.decrypt(&d1).is_err());
    // Foreign tokens do not count either.
    let keys = derive_datagram_keys(&TICKET, &SECRET).unwrap();
    let mut foreign = DatagramCrypt::new(&keys.c2s, &[0x5A; consts::TOKEN_LENGTH]);
    let f = foreign.encrypt(1, b"foreign").unwrap();
    assert!(rx.decrypt(&f).is_err());
    assert_eq!(rx.received(), 2);
}

#[test]
fn late_arrival_discounts_loss_estimate() {
    let (mut tx, mut rx) = pair();
    let d0 = tx.encrypt(1, b"first").unwrap();
    let d1 = tx.encrypt(1, b"late one").unwrap();
    let d2 = tx.encrypt(1, b"next").unwrap();
    rx.decrypt(&d0).unwrap(); // anchor the window at seq 0
    rx.decrypt(&d2).unwrap(); // seq 1 counted as lost for now
    assert_eq!(rx.lost_estimate(), 1);
    rx.decrypt(&d1).unwrap(); // it was jitter, not loss
    assert_eq!(rx.lost_estimate(), 0);
}

#[test]
fn wrong_key_fails() {
    let (mut tx, _rx) = pair();
    let other = derive_datagram_keys(&[0x99; consts::TICKET_LENGTH], &SECRET).unwrap();
    let mut rx = DatagramDecrypt::new(&other.c2s, &TOKEN);
    let d = tx.encrypt(1, b"unreadable").unwrap();
    assert!(rx.decrypt(&d).is_err());
}

#[test]
fn short_datagram_is_rejected() {
    let (_tx, mut rx) = pair();
    assert!(matches!(
        rx.decrypt(&[0u8; 10]),
        Err(CryptError::PacketTooShort(_))
    ));
}

#[test]
fn payload_above_max_is_rejected() {
    let (mut tx, _rx) = pair();
    let big = vec![0u8; consts::MAX_DATAGRAM_PAYLOAD + 1];
    assert!(matches!(
        tx.encrypt(1, &big),
        Err(CryptError::PacketTooLarge(_))
    ));
}

#[test]
fn max_fragment_payload_fits_mtu_budget() {
    // The budget itself is a compile-time assertion in `consts.rs`; here we
    // check a max-size payload actually round-trips.
    let (mut tx, mut rx) = pair();
    let payload = vec![7u8; consts::MAX_DATAGRAM_PAYLOAD];
    let d = tx.encrypt(2, &payload).unwrap();
    assert_eq!(rx.decrypt(&d).unwrap().1, payload);
}

// ---- Zero-allocation API (hot path, design doc §12.6) ----

#[test]
fn encrypt_into_matches_allocating_encrypt() {
    let (mut tx_a, _rx) = pair();
    let (mut tx_b, _rx) = pair();
    let payload = b"zero alloc";
    let dgram_a = tx_a.encrypt(5, payload).unwrap();
    let mut out = [0u8; 2048];
    let n = tx_b.encrypt_into(5, payload, &mut out).unwrap();
    assert_eq!(&out[..n], &dgram_a[..]);
}

#[test]
fn decrypt_into_matches_allocating_decrypt_and_reuses_buffer() {
    let (mut tx, mut rx) = pair();
    let mut out = [0u8; 2048];
    for i in 0..10u8 {
        let payload = [i; 100];
        let dgram = tx.encrypt(9, &payload).unwrap();
        let (channel, n) = rx.decrypt_into(&dgram, &mut out).unwrap();
        assert_eq!(channel, 9);
        assert_eq!(&out[..n], &payload);
    }
}

#[test]
fn encrypt_into_reports_small_buffer() {
    let (mut tx, _rx) = pair();
    let mut out = [0u8; 8];
    assert!(matches!(
        tx.encrypt_into(1, b"some payload", &mut out),
        Err(CryptError::BufferTooSmall { .. })
    ));
}

#[test]
fn decrypt_into_reports_small_buffer() {
    let (mut tx, mut rx) = pair();
    let dgram = tx.encrypt(1, b"some payload").unwrap();
    let mut out = [0u8; 4];
    assert!(matches!(
        rx.decrypt_into(&dgram, &mut out),
        Err(CryptError::BufferTooSmall { .. })
    ));
}

#[test]
fn decrypt_into_still_enforces_security_checks() {
    let (mut tx, mut rx) = pair();
    let mut out = [0u8; 2048];
    let dgram = tx.encrypt(1, b"guarded").unwrap();
    // Tampered ciphertext fails AEAD even with a big enough buffer.
    let mut bad = dgram.clone();
    let last = bad.len() - 20;
    bad[last] ^= 1;
    assert!(matches!(
        rx.decrypt_into(&bad, &mut out),
        Err(CryptError::Aead)
    ));
    // And the good one still passes.
    assert!(rx.decrypt_into(&dgram, &mut out).is_ok());
}
