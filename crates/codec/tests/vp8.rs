//! VP8 (libvpx) encoder/decoder tests — feature `codec-vp8`.
//! Run with: `cargo test -p codec --features codec-vp8`.

#![cfg(feature = "codec-vp8")]
#![allow(clippy::cast_possible_truncation)]

use codec::{Vp8Decoder, Vp8Encoder, VideoDecoder, VideoEncoder, VideoFrame};

fn gradient_frame(width: u32, height: u32) -> Vec<u8> {
    let mut data = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let off = ((y * width + x) * 4) as usize;
            data[off] = (x % 256) as u8;
            data[off + 1] = (y % 256) as u8;
            data[off + 2] = 128;
            data[off + 3] = 255;
        }
    }
    data
}

#[test]
fn vp8_roundtrip_preserves_geometry() {
    let (w, h) = (320, 240);
    let data = gradient_frame(w, h);
    let frame = VideoFrame {
        width: w,
        height: h,
        stride: (w * 4) as usize,
        data: &data,
    };
    let mut enc = Vp8Encoder::new(w, h, 1000).expect("encoder init");
    let packet = enc.encode(&frame).expect("encode");
    assert!(packet.keyframe);
    let mut dec = Vp8Decoder::new().expect("decoder init");
    let decoded = dec.decode(&packet).expect("decode");
    assert_eq!((decoded.width, decoded.height), (w, h));
}

#[test]
fn vp8_request_keyframe_and_hot_bitrate() {
    let (w, h) = (160, 120);
    let data = gradient_frame(w, h);
    let frame = VideoFrame {
        width: w,
        height: h,
        stride: (w * 4) as usize,
        data: &data,
    };
    let mut enc = Vp8Encoder::new(w, h, 1000).expect("encoder init");
    let _ = enc.encode(&frame).expect("first");
    enc.request_keyframe();
    assert!(enc.encode(&frame).expect("forced").keyframe);
    enc.set_bitrate(250).expect("set_bitrate");
    let _ = enc.encode(&frame).expect("after bitrate change");
}
