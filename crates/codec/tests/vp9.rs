// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! VP9 (libvpx) encoder/decoder tests — feature `codec-vp9`.
//!
//! Roundtrip over a synthetic BGRA frame: encode → decode → geometry and
//! approximate color fidelity (lossy codec: compare with tolerance).
//! Run with: `cargo test -p codec --features codec-vp9`.

#![cfg(feature = "codec-vp9")]
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)] // test pattern math

use codec::{VideoDecoder, VideoEncoder, VideoFrame, Vp9Decoder, Vp9Encoder};

fn gradient_frame(width: u32, height: u32) -> Vec<u8> {
    let mut data = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let off = ((y * width + x) * 4) as usize;
            data[off] = (x % 256) as u8; // B
            data[off + 1] = (y % 256) as u8; // G
            data[off + 2] = 128; // R
            data[off + 3] = 255;
        }
    }
    data
}

#[test]
fn vp9_roundtrip_preserves_geometry_and_rough_color() {
    let (w, h) = (320, 240);
    let data = gradient_frame(w, h);
    let frame = VideoFrame {
        width: w,
        height: h,
        stride: (w * 4) as usize,
        data: &data,
    };

    let mut enc = Vp9Encoder::new(w, h, 1000).expect("encoder init");
    let packet = enc.encode(&frame).expect("encode");
    assert!(packet.keyframe, "first frame must be a keyframe");
    assert_ne!(packet.data.len(), 0);

    let mut dec = Vp9Decoder::new().expect("decoder init");
    let decoded = dec.decode(&packet).expect("decode").expect("frame");
    assert_eq!((decoded.width, decoded.height), (w, h));

    // Lossy + color conversion: compare averages with generous tolerance.
    let avg = |d: &[u8], c: usize| -> f64 {
        d.iter()
            .skip(c)
            .step_by(4)
            .map(|&v| f64::from(v))
            .sum::<f64>()
            / (d.len() / 4) as f64
    };
    for c in 0..3 {
        let (a, b) = (avg(&data, c), avg(&decoded.data, c));
        assert!((a - b).abs() < 8.0, "channel {c}: {a} vs {b}");
    }
}

#[test]
fn vp9_second_frame_is_delta_and_smaller() {
    let (w, h) = (320, 240);
    let data = gradient_frame(w, h);
    let frame = VideoFrame {
        width: w,
        height: h,
        stride: (w * 4) as usize,
        data: &data,
    };
    let mut enc = Vp9Encoder::new(w, h, 1000).expect("encoder init");
    let key = enc.encode(&frame).expect("key");
    let delta = enc.encode(&frame).expect("delta");
    assert!(!delta.keyframe, "second identical frame must be a delta");
    assert!(
        delta.data.len() < key.data.len(),
        "delta ({} B) should beat keyframe ({} B) on a static frame",
        delta.data.len(),
        key.data.len()
    );
}

#[test]
fn vp9_request_keyframe_forces_next_key() {
    let (w, h) = (160, 120);
    let data = gradient_frame(w, h);
    let frame = VideoFrame {
        width: w,
        height: h,
        stride: (w * 4) as usize,
        data: &data,
    };
    let mut enc = Vp9Encoder::new(w, h, 500).expect("encoder init");
    let _ = enc.encode(&frame).expect("first");
    enc.request_keyframe();
    let forced = enc.encode(&frame).expect("forced");
    assert!(forced.keyframe, "requested keyframe must be flagged");
}

#[test]
fn vp9_set_bitrate_is_hot() {
    let (w, h) = (160, 120);
    let data = gradient_frame(w, h);
    let frame = VideoFrame {
        width: w,
        height: h,
        stride: (w * 4) as usize,
        data: &data,
    };
    let mut enc = Vp9Encoder::new(w, h, 2000).expect("encoder init");
    enc.set_bitrate(250).expect("set_bitrate");
    let _ = enc.encode(&frame).expect("encode after bitrate change");
}

#[test]
fn vp9_decoder_rejects_garbage() {
    let mut dec = Vp9Decoder::new().expect("decoder init");
    let bad = codec::EncodedPacket {
        data: vec![0xDE; 64],
        keyframe: true,
    };
    assert!(dec.decode(&bad).is_err());
}

/// §18.78 regression: a tiny keyframe whose uncompressed-header
/// width/height-minus-1 fields declare 16383x16383 must be rejected by the
/// decoder dimension cap (init-time cfg cap in libvpx + post-decode
/// re-check), not honored with a multi-hundred-MiB allocation.
#[test]
fn vp9_decoder_rejects_crafted_dimensions() {
    let (w, h) = (64, 64);
    let data = gradient_frame(w, h);
    let frame = VideoFrame {
        width: w,
        height: h,
        stride: (w * 4) as usize,
        data: &data,
    };
    let mut enc = Vp9Encoder::new(w, h, 500).expect("encoder init");
    let packet = enc.encode(&frame).expect("encode");
    assert!(packet.keyframe);
    // Keyframe layout: marker(2b) profile(2b) show_existing(1b)
    // frame_type(1b) show_frame(1b) error_resilient(1b) sync(24b) at
    // bytes 1-3, then w-1 (16b MSB-first) and h-1 (16b MSB-first).
    let mut crafted = packet.data.clone();
    assert_eq!(&crafted[1..4], &[0x49, 0x83, 0x42], "VP9 sync code");
    let set_bits = |buf: &mut [u8], bit_off: usize, value: u32, nbits: usize| {
        for i in 0..nbits {
            let bit = (value >> (nbits - 1 - i)) & 1;
            let pos = bit_off + i;
            let mask = 1 << (7 - (pos % 8));
            if bit == 1 {
                buf[pos / 8] |= mask;
            } else {
                buf[pos / 8] &= !mask;
            }
        }
    };
    set_bits(&mut crafted, 36, 16383, 16);
    set_bits(&mut crafted, 52, 16383, 16);
    let mut dec = Vp9Decoder::new().expect("decoder init");
    let crafted = codec::EncodedPacket {
        data: crafted,
        keyframe: true,
    };
    assert!(
        dec.decode(&crafted).is_err(),
        "crafted 16383x16383 dimensions must be rejected by the cap"
    );
}
