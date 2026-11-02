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

use codec::{Vp9Decoder, Vp9Encoder, VideoDecoder, VideoEncoder, VideoFrame};

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
    let decoded = dec.decode(&packet).expect("decode");
    assert_eq!((decoded.width, decoded.height), (w, h));

    // Lossy + color conversion: compare averages with generous tolerance.
    let avg = |d: &[u8], c: usize| -> f64 {
        d.iter().skip(c).step_by(4).map(|&v| f64::from(v)).sum::<f64>()
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
