// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! BGRA→I420 color conversion tests (BT.601 limited range, 2x2 box
//! subsampling — the same layout libyuv's `ARGBToI420` produces, so the
//! future SIMD swap is drop-in).
#![allow(clippy::cast_possible_truncation)] // test pattern math

use codec::color::{I420Frame, bgra_to_i420};
use codec::{CodecError, VideoFrame};

fn frame(width: u32, height: u32, stride: usize, fill: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
    let mut data = vec![0u8; stride * height as usize];
    for py in 0..height {
        for px in 0..width {
            let [b, g, r, a] = fill(px, py);
            let off = py as usize * stride + px as usize * 4;
            data[off] = b;
            data[off + 1] = g;
            data[off + 2] = r;
            data[off + 3] = a;
        }
    }
    data
}

#[test]
fn black_maps_to_luma_floor() {
    let data = frame(2, 2, 8, |_, _| [0, 0, 0, 255]);
    let f = VideoFrame {
        width: 2,
        height: 2,
        stride: 8,
        data: &data,
    };
    let out: I420Frame = bgra_to_i420(&f).unwrap();
    // BT.601 limited range: black -> Y=16, U=V=128.
    assert!(out.y.iter().all(|&y| y == 16));
    assert!(out.u.iter().all(|&u| u == 128));
    assert!(out.v.iter().all(|&v| v == 128));
}

#[test]
fn white_maps_to_luma_ceiling() {
    let data = frame(2, 2, 8, |_, _| [255, 255, 255, 255]);
    let f = VideoFrame {
        width: 2,
        height: 2,
        stride: 8,
        data: &data,
    };
    let out = bgra_to_i420(&f).unwrap();
    assert!(out.y.iter().all(|&y| y == 235));
    assert!(out.u.iter().all(|&u| u == 128));
    assert!(out.v.iter().all(|&v| v == 128));
}

#[test]
fn pure_red_matches_bt601() {
    let data = frame(2, 2, 8, |_, _| [0, 0, 255, 255]);
    let f = VideoFrame {
        width: 2,
        height: 2,
        stride: 8,
        data: &data,
    };
    let out = bgra_to_i420(&f).unwrap();
    // BT.601 limited: R=255 -> Y≈81, U≈90, V≈240 (tolerance ±2).
    assert!((79..=83).contains(&out.y[0]), "Y={}", out.y[0]);
    assert!((88..=92).contains(&out.u[0]), "U={}", out.u[0]);
    assert!((238..=242).contains(&out.v[0]), "V={}", out.v[0]);
}

#[test]
fn plane_sizes_and_strides_are_tight() {
    // 4x2 with padded stride 20 (GPU-style row alignment).
    let data = frame(4, 2, 20, |x, y| [u8::try_from(x * 10).unwrap(), u8::try_from(y * 100).unwrap(), 30, 255]);
    let f = VideoFrame {
        width: 4,
        height: 2,
        stride: 20,
        data: &data,
    };
    let out = bgra_to_i420(&f).unwrap();
    assert_eq!((out.y_stride, out.uv_stride), (4, 2));
    assert_eq!(out.y.len(), 4 * 2);
    assert_eq!(out.u.len(), 2);
    assert_eq!(out.v.len(), 2);
}

#[test]
fn stride_padding_does_not_leak_into_luma() {
    // Same image, two different strides: the luma must be identical.
    let tight = frame(2, 2, 8, |x, _| [u8::try_from(x).unwrap() * 100, 50, 200, 255]);
    let padded = frame(2, 2, 16, |x, _| [u8::try_from(x).unwrap() * 100, 50, 200, 255]);
    let a = bgra_to_i420(&VideoFrame {
        width: 2,
        height: 2,
        stride: 8,
        data: &tight,
    })
    .unwrap();
    let b = bgra_to_i420(&VideoFrame {
        width: 2,
        height: 2,
        stride: 16,
        data: &padded,
    })
    .unwrap();
    assert_eq!(a.y, b.y);
    assert_eq!(a.u, b.u);
    assert_eq!(a.v, b.v);
}

#[test]
fn odd_dimensions_round_up_chroma() {
    // 3x3: chroma planes must cover ceil(3/2) = 2 samples per axis.
    let data = frame(3, 3, 12, |_, _| [10, 20, 30, 255]);
    let f = VideoFrame {
        width: 3,
        height: 3,
        stride: 12,
        data: &data,
    };
    let out = bgra_to_i420(&f).unwrap();
    assert_eq!((out.width, out.height), (3, 3));
    assert_eq!(out.y.len(), 3 * 3);
    assert_eq!((out.uv_stride, out.u.len()), (2, 2 * 2));
}

#[test]
fn chroma_is_the_2x2_box_average() {
    // A 2x2 block with 4 distinct blues: U must reflect the average.
    let blues = [0u8, 40, 80, 120];
    let mut data = vec![0u8; 8 * 2];
    for (i, &b) in blues.iter().enumerate() {
        let off = (i / 2) * 8 + (i % 2) * 4;
        data[off] = b; // B
        data[off + 1] = 0; // G
        data[off + 2] = 0; // R
        data[off + 3] = 255;
    }
    let f = VideoFrame {
        width: 2,
        height: 2,
        stride: 8,
        data: &data,
    };
    let out = bgra_to_i420(&f).unwrap();
    // Average B = 60: U = 128 + (112*60 + 128)/256 ≈ 154 (tolerance ±3).
    assert!((151..=157).contains(&out.u[0]), "U={}", out.u[0]);
}

#[test]
fn bad_dimensions_are_rejected() {
    let f = VideoFrame {
        width: 4,
        height: 2,
        stride: 16,
        data: &[0u8; 10],
    };
    assert!(matches!(
        bgra_to_i420(&f),
        Err(CodecError::BadDimensions { .. })
    ));
}

// ---- I420 → BGRA (decode side) ----

use codec::color::i420_to_bgra;

#[test]
fn i420_roundtrip_preserves_luma() {
    // Grayscale ramp: survives the roundtrip nearly exactly (chroma of gray
    // is constant, luma lossless-ish).
    let data = frame(64, 32, 64 * 4, |x, y| {
        let v = ((x + y) % 256) as u8;
        [v, v, v, 255]
    });
    let src = VideoFrame {
        width: 64,
        height: 32,
        stride: 64 * 4,
        data: &data,
    };
    let i420 = bgra_to_i420(&src).expect("convert");
    let back = i420_to_bgra(&i420).expect("back");
    assert_eq!(back.width, 64);
    assert_eq!(back.height, 32);
    assert_eq!(back.stride, 64 * 4);
    for (px, py) in [(0, 0), (17, 5), (63, 31)] {
        let off = py * 64 * 4 + px * 4;
        let orig = data[off];
        let got = back.data[off];
        assert!(
            (i16::from(orig) - i16::from(got)).abs() <= 2,
            "luma drift at ({px},{py}): {orig} vs {got}"
        );
        assert_eq!(back.data[off + 3], 255, "alpha opaque");
    }
}

#[test]
fn i420_to_bgra_known_values() {
    // A flat mid-gray I420 frame must come back as that gray.
    let i420 = I420Frame {
        width: 4,
        height: 4,
        y_stride: 4,
        uv_stride: 2,
        y: vec![126; 16],
        u: vec![128; 4],
        v: vec![128; 4],
    };
    let back = i420_to_bgra(&i420).expect("back");
    assert!(back.data.as_chunks::<4>().0.iter().all(|&[b, g, r, a]| {
        b == g && g == r && (110..=142).contains(&r) && a == 255
    }));
}
