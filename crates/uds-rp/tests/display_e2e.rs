//! Channel-level E2E test of the display pipeline (RawCodec):
//!
//! synthetic frame → encode → fragment → encrypt (server keys)
//!   → [simulated network: in-order, reordered, partial loss]
//!   → decrypt (client keys) → reassemble → decode → pixel-compare.

use uds_rp::codec::{
    DecodedFrame, EncodedPacket, RawDecoder, RawEncoder, VideoDecoder, VideoEncoder, VideoFrame,
};
use uds_rp::core::channel;
use uds_rp::core::fragment::{self, Fragmenter, Reassembler};
use uds_rp::crypt::{DatagramCrypt, DatagramDecrypt, derive_datagram_keys, derive_udp_token};
use uds_rp::server::synthetic::SyntheticSource;

const TICKET: [u8; 48] = [0x42; 48];
const KEY: [u8; 32] = [0x07; 32];

/// Server→client wire keys and the session token, as the real handshake
/// derives them.
fn wire() -> (DatagramCrypt, DatagramDecrypt) {
    let keys = derive_datagram_keys(&TICKET, &KEY).expect("keys");
    let token = derive_udp_token(&TICKET, &KEY).expect("token");
    (
        DatagramCrypt::new(&keys.s2c, &token),
        DatagramDecrypt::new(&keys.s2c, &token),
    )
}

fn decode_frame(data: Vec<u8>) -> DecodedFrame {
    RawDecoder::new()
        .decode(&EncodedPacket {
            data,
            keyframe: true,
        })
        .expect("decode")
}

/// Pushes one frame through the whole pipeline and returns the decoded
/// BGRA pixels. `frag_filter` decides, per fragment index, whether the
/// fragment "arrives" (simulated loss).
fn roundtrip(
    frame: &VideoFrame<'_>,
    frame_id: u32,
    frag_filter: impl Fn(usize) -> bool,
) -> Option<DecodedFrame> {
    let (mut tx, mut rx) = wire();
    let mut reasm = Reassembler::new();

    let packet = RawEncoder::new().encode(frame).expect("encode");
    let frags = Fragmenter::split(0, frame_id, 1_000, &packet.data);
    let mut decoded = None;
    for (i, frag) in frags.iter().enumerate() {
        if !frag_filter(i) {
            continue; // lost in the "network"
        }
        let dgram = tx.encrypt(channel::DISPLAY, frag).expect("encrypt");
        let (ch, plain) = rx.decrypt(&dgram).expect("decrypt");
        assert_eq!(ch, channel::DISPLAY);
        if let Some(complete) = reasm.push(&plain).expect("reassemble") {
            assert_eq!(complete.capture_ts_ms, 1_000, "capture ts survives the wire");
            decoded = Some(decode_frame(complete.data));
        }
    }
    decoded
}

#[test]
fn display_frame_survives_the_full_pipeline() {
    let mut src = SyntheticSource::new(320, 240);
    let frame = src.next_frame();
    let original = frame.data.to_vec();

    let decoded = roundtrip(&frame, 0, |_| true).expect("frame complete");
    assert_eq!((decoded.width, decoded.height), (320, 240));
    assert_eq!(decoded.data, original, "pixel-perfect roundtrip");
}

#[test]
fn fragments_are_mtu_sized() {
    let mut src = SyntheticSource::new(320, 240);
    let frame = src.next_frame();
    let packet = RawEncoder::new().encode(&frame).expect("encode");
    let frags = Fragmenter::split(0, 0, 0, &packet.data);
    // 320*240*4 + 8 bytes of raw header = 307208 B → 264 fragments of ≤1168 B
    // of payload (12-byte header since capture_ts_ms, §9.1).
    assert_eq!(frags.len(), 264);
    assert!(
        frags
            .iter()
            .all(|f| f.len() <= fragment::HEADER_SIZE + fragment::fragment_payload_max())
    );
}

#[test]
fn a_lost_fragment_drops_the_whole_frame() {
    let mut src = SyntheticSource::new(320, 240);
    let frame = src.next_frame();
    // Drop fragment 7: the frame must never reach the decoder.
    assert!(roundtrip(&frame, 0, |i| i != 7).is_none());
}

#[test]
fn moderately_reordered_fragments_still_reassemble() {
    let mut src = SyntheticSource::new(320, 240);
    let frame = src.next_frame();
    let original = frame.data.to_vec();
    let (mut tx, mut rx) = wire();
    let mut reasm = Reassembler::new();

    let packet = RawEncoder::new().encode(&frame).expect("encode");
    let frags = Fragmenter::split(0, 0, 0, &packet.data);
    // Encrypt in order, deliver reversed within 32-datagram windows: a
    // realistic reorder distance, inside the anti-replay window (64).
    let dgrams: Vec<_> = frags
        .iter()
        .map(|f| tx.encrypt(channel::DISPLAY, f).expect("encrypt"))
        .collect();
    let mut decoded = None;
    for window in dgrams.chunks(32) {
        for dgram in window.iter().rev() {
            let (_, plain) = rx.decrypt(dgram).expect("decrypt");
            if let Some(complete) = reasm.push(&plain).expect("reassemble") {
                decoded = Some(decode_frame(complete.data));
            }
        }
    }
    assert_eq!(decoded.expect("complete").data, original);
}
