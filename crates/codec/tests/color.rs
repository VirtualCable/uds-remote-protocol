//! BGRA→I420 color conversion tests (BT.601 limited range, 2x2 box
//! subsampling — the same layout libyuv's `ARGBToI420` produces, so the
//! future SIMD swap is drop-in).

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
