//! Tests del crate `crypt`: derivación de claves y cifrado de stream (TCP).

use crypt::{CryptError, SessionKeys, StreamCrypt, StreamDecrypt, derive_session_keys};

fn ticket() -> [u8; 48] {
    [0xA5; 48]
}

fn secret() -> [u8; 32] {
    [0x3C; 32]
}

fn pair() -> (StreamCrypt, StreamDecrypt) {
    let keys = derive_session_keys(&ticket(), &secret()).unwrap();
    (StreamCrypt::new(&keys.c2s), StreamDecrypt::new(&keys.c2s))
}

#[test]
fn derive_is_deterministic() {
    let a = derive_session_keys(&ticket(), &secret()).unwrap();
    let b = derive_session_keys(&ticket(), &secret()).unwrap();
    assert_eq!(a, b);
}

#[test]
fn derive_keys_are_directional() {
    let keys = derive_session_keys(&ticket(), &secret()).unwrap();
    assert_ne!(keys.c2s, keys.s2c);
}

#[test]
fn derive_depends_on_ticket_and_secret() {
    let base = derive_session_keys(&ticket(), &secret()).unwrap();
    let other_ticket = derive_session_keys(&[0x5A; 48], &secret()).unwrap();
    let other_secret = derive_session_keys(&ticket(), &[0xC3; 32]).unwrap();
    assert_ne!(base, other_ticket);
    assert_ne!(base, other_secret);
}

#[test]
fn roundtrip_returns_channel_and_plaintext() {
    let (mut enc, mut dec) = pair();
    let packet = enc.encrypt(7, b"hola uds").unwrap();
    let (channel, plaintext) = dec.decrypt(&packet).unwrap();
    assert_eq!(channel, 7);
    assert_eq!(plaintext, b"hola uds");
}

#[test]
fn seq_increments_per_packet() {
    let (mut enc, _dec) = pair();
    assert_eq!(enc.seq(), 0);
    let p1 = enc.encrypt(0, b"a").unwrap();
    let p2 = enc.encrypt(0, b"b").unwrap();
    assert_eq!(enc.seq(), 2);
    assert_ne!(p1, p2);
}

#[test]
fn replay_is_rejected() {
    let (mut enc, mut dec) = pair();
    let p = enc.encrypt(0, b"unico").unwrap();
    dec.decrypt(&p).unwrap();
    let err = dec.decrypt(&p).unwrap_err();
    assert!(matches!(err, CryptError::Replay { .. }));
}

#[test]
fn out_of_order_is_rejected() {
    let (mut enc, mut dec) = pair();
    let p1 = enc.encrypt(0, b"uno").unwrap();
    let p2 = enc.encrypt(0, b"dos").unwrap();
    dec.decrypt(&p2).unwrap();
    let err = dec.decrypt(&p1).unwrap_err();
    assert!(matches!(err, CryptError::Replay { .. }));
}

#[test]
fn tampered_ciphertext_fails() {
    let (mut enc, mut dec) = pair();
    let mut p = enc.encrypt(0, b"secreto").unwrap();
    let mid = p.len() / 2;
    p[mid] ^= 0xFF;
    assert!(dec.decrypt(&p).is_err());
}

#[test]
fn tampered_len_fails_before_desync() {
    let (mut enc, mut dec) = pair();
    let mut p = enc.encrypt(0, b"framing").unwrap();
    // len esta en los bytes 8..10 de la cabecera en claro y forma parte del AAD
    p[9] ^= 0x01;
    assert!(dec.decrypt(&p).is_err());
}

#[test]
fn wrong_key_fails() {
    let (mut enc, _dec) = pair();
    let p = enc.encrypt(0, b"datos").unwrap();
    let keys = derive_session_keys(&ticket(), &[0xFF; 32]).unwrap();
    let mut bad = StreamDecrypt::new(&keys.c2s);
    assert!(bad.decrypt(&p).is_err());
}

#[test]
fn oversized_plaintext_is_rejected() {
    let (mut enc, _dec) = pair();
    let huge = vec![0u8; 65_518];
    let err = enc.encrypt(0, &huge).unwrap_err();
    assert!(matches!(err, CryptError::PacketTooLarge { .. }));
}

#[test]
fn truncated_packet_is_rejected() {
    let (mut enc, mut dec) = pair();
    let p = enc.encrypt(0, b"cortame").unwrap();
    let err = dec.decrypt(&p[..p.len() - 5]).unwrap_err();
    assert!(matches!(err, CryptError::PacketTooShort { .. } | CryptError::Aead));
}

#[test]
fn session_keys_debug_does_not_leak() {
    let keys = SessionKeys {
        c2s: [1; 32],
        s2c: [2; 32],
    };
    let dbg = format!("{keys:?}");
    assert!(!dbg.contains('1'));
}
