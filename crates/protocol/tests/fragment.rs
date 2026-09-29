//! Frame fragmentation/reassembly tests (design doc §6.2): a video frame may
//! exceed the MTU, so it is split into self-contained datagrams with header
//! `|display_id 1B|frame_id 3B|frag_idx 2B|frag_count 2B|`. If any fragment
//! is missing, the whole frame is dropped — a half frame never reaches the
//! decoder.

use protocol::fragment::{Fragmenter, ReassembledFrame, Reassembler, fragment_payload_max};
use protocol::{ProtocolError, channel};

fn element(size: usize) -> Vec<u8> {
    (0..size).map(|i| u8::try_from(i % 256).unwrap()).collect()
}

#[test]
fn fragment_payload_respects_mtu_budget() {
    // The fragment payload plus the 8-byte header must fit the datagram
    // plaintext budget (channel display over UDP, §6.2).
    assert!(fragment_payload_max() + 8 <= crypt::consts::MAX_DATAGRAM_PAYLOAD);
}

#[test]
fn small_element_is_a_single_fragment() {
    let data = element(100);
    let frags = Fragmenter::split(0, 42, &data);
    assert_eq!(frags.len(), 1);
    let mut rx = Reassembler::new();
    let frame = rx.push(&frags[0]).unwrap().unwrap();
    assert_eq!(frame.display_id, 0);
    assert_eq!(frame.frame_id, 42);
    assert_eq!(frame.data, data);
}

#[test]
fn large_element_roundtrip() {
    let data = element(10_000);
    let frags = Fragmenter::split(0, 7, &data);
    assert!(frags.len() > 3);
    let mut rx = Reassembler::new();
    let mut out = None;
    for f in &frags {
        out = rx.push(f).unwrap().or(out);
    }
    assert_eq!(out.unwrap().data, data);
}

#[test]
fn out_of_order_fragments_reassemble() {
    let data = element(5_000);
    let mut frags = Fragmenter::split(0, 9, &data);
    frags.reverse();
    let mut rx = Reassembler::new();
    let mut out = None;
    for f in &frags {
        out = rx.push(f).unwrap().or(out);
    }
    assert_eq!(out.unwrap().data, data);
}

#[test]
fn missing_fragment_drops_whole_frame() {
    let data = element(5_000);
    let frags = Fragmenter::split(0, 3, &data);
    let mut rx = Reassembler::new();
    // Feed all but one fragment: the frame must never be delivered.
    for f in frags.iter().skip(1) {
        assert!(rx.push(f).unwrap().is_none());
    }
    assert!(!rx.is_frame_complete(3));
}

#[test]
fn interleaved_frames_stay_separate() {
    let a = element(3_000);
    let b = element(2_000);
    let fa = Fragmenter::split(0, 20, &a);
    let fb = Fragmenter::split(1, 21, &b); // different display too
    let mut rx = Reassembler::new();
    let mut got_a = None;
    let mut got_b = None;
    for (x, y) in fa.iter().zip(fb.iter()) {
        got_a = rx.push(x).unwrap().or(got_a);
        got_b = rx.push(y).unwrap().or(got_b);
    }
    // Flush remaining fragments of the longer one.
    for f in fa.iter().skip(fb.len()) {
        got_a = rx.push(f).unwrap().or(got_a);
    }
    let ra = got_a.unwrap();
    let rb = got_b.unwrap();
    assert_eq!((ra.display_id, ra.frame_id), (0, 20));
    assert_eq!((rb.display_id, rb.frame_id), (1, 21));
    assert_eq!(ra.data, a);
    assert_eq!(rb.data, b);
}

#[test]
fn old_incomplete_frames_are_evicted() {
    let mut rx = Reassembler::new();
    // Start frame 1 but never finish it.
    let partial = Fragmenter::split(0, 1, &element(3_000));
    rx.push(&partial[0]).unwrap();
    // Many newer frames push frame 1 out of the tracking window.
    let f = Fragmenter::split(0, 100, &element(100));
    rx.push(&f[0]).unwrap();
    assert!(!rx.tracks_frame(1), "stale partial frame must be evicted");
}

#[test]
fn garbage_fragment_is_rejected() {
    let mut rx = Reassembler::new();
    assert!(matches!(
        rx.push(&[0u8; 4]),
        Err(ProtocolError::Truncated)
    ));
    // frag_idx >= frag_count is inconsistent.
    let bad = Fragmenter::split(0, 5, &element(100))[0].clone();
    let mut bad = bad;
    let n = bad.len();
    // header: display(1) frame(3) idx(2) count(2); set idx=5 count=1
    bad[4..6].copy_from_slice(&5u16.to_be_bytes());
    bad[6..8].copy_from_slice(&1u16.to_be_bytes());
    let _ = n;
    assert!(rx.push(&bad).is_err());
}

#[test]
fn frame_ids_are_u24() {
    let data = element(10);
    // Frame id wraps at 2^24; the wire header carries 3 bytes.
    let frags = Fragmenter::split(0, 0x00FF_FFFF, &data);
    let mut rx = Reassembler::new();
    let frame: ReassembledFrame = rx.push(&frags[0]).unwrap().unwrap();
    assert_eq!(frame.frame_id, 0x00FF_FFFF);
}

#[test]
fn channel_display_exists() {
    // Sanity: fragments are meant for the lossy display channel.
    assert_eq!(channel::DISPLAY, 1);
}
