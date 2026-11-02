// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Codec trait tests with the always-available `RawCodec` (no compression,
//! reference implementation used by channel-level E2E tests).

use codec::{
    CodecError, DecodedFrame, EncodedPacket, RawDecoder, RawEncoder, VideoCodecType, VideoDecoder,
    VideoEncoder, VideoFrame,
};

/// 4x2 BGRA test pattern with padded stride (as GPU rows come).
fn test_frame() -> (Vec<u8>, VideoFrame<'static>) {
    let (w, h, stride) = (4usize, 2usize, 20usize); // 4 px * 4 B = 16, padded to 20
    let mut data = vec![0u8; stride * h];
    for y in 0..h {
        for x in 0..w {
            let px = u8::try_from(y * w + x).unwrap();
            let off = y * stride + x * 4;
            data[off] = px; // B
            data[off + 1] = 255 - px; // G
            data[off + 2] = px.wrapping_mul(3); // R
            data[off + 3] = 255; // A
        }
    }
    let frame = VideoFrame {
        width: 4,
        height: 2,
        stride,
        data: Box::leak(data.clone().into_boxed_slice()),
    };
    (data, frame)
}

#[test]
fn raw_roundtrip_strips_stride_padding() {
    let (data, frame) = test_frame();
    let mut enc = RawEncoder::new();
    let mut dec = RawDecoder::new();

    let packet: EncodedPacket = enc.encode(&frame).unwrap();
    assert_eq!(enc.codec_type(), VideoCodecType::Raw);
    // Every raw frame is independent: always a keyframe.
    assert!(packet.keyframe);

    let decoded: DecodedFrame = dec.decode(&packet).unwrap();
    assert_eq!((decoded.width, decoded.height), (4, 2));
    // Tight stride in the decoded frame (padding is wire overhead).
    assert_eq!(decoded.stride, 16);
    assert_eq!(decoded.data.len(), 16 * 2);
    for y in 0..2usize {
        let src = &data[y * 20..y * 20 + 16];
        let dst = &decoded.data[y * 16..y * 16 + 16];
        assert_eq!(dst, src, "row {y} must survive the round trip");
    }
}

#[test]
fn raw_set_bitrate_is_accepted_noop() {
    let mut enc = RawEncoder::new();
    enc.set_bitrate(2_000).unwrap();
    enc.request_keyframe(); // must not panic
}

#[test]
fn raw_rejects_inconsistent_dimensions() {
    let frame = VideoFrame {
        width: 4,
        height: 2,
        stride: 16,
        data: &[0u8; 8], // way too small for 4x2 BGRA
    };
    let mut enc = RawEncoder::new();
    assert!(matches!(
        enc.encode(&frame),
        Err(CodecError::BadDimensions { .. })
    ));
}

#[test]
fn raw_decoder_rejects_garbage() {
    let mut dec = RawDecoder::new();
    let packet = EncodedPacket {
        data: vec![1, 2, 3],
        keyframe: true,
    };
    assert!(dec.decode(&packet).is_err());
}

#[test]
fn codec_types_have_wire_ids() {
    // Negotiation needs stable numeric IDs (capabilities, §8).
    assert_eq!(VideoCodecType::Raw.wire_id(), 0);
    assert_eq!(VideoCodecType::Vp8.wire_id(), 1);
    assert_eq!(VideoCodecType::Vp9.wire_id(), 2);
    assert_eq!(VideoCodecType::from_wire_id(2), Some(VideoCodecType::Vp9));
    assert_eq!(VideoCodecType::from_wire_id(255), None);
}
