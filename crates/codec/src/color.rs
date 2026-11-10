// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! BGRA → I420 color conversion (BT.601 limited range, 2x2 box-subsampled
//! chroma — the same layout libyuv's `ARGBToI420` produces, so a future
//! SIMD libyuv swap behind a feature is drop-in).
//!
//! Pure scalar Rust: no FFI, so `codec` stays WASM-compatible and the build
//! stays trivial on Linux/Windows. If profiling shows this is a bottleneck,
//! the libyuv backend replaces the internals, not the API.

use crate::error::{CodecError, Result};
use crate::video::VideoFrame;

/// A planar I420 (YUV 4:2:0) frame, tight strides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct I420Frame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Luma plane stride (= width).
    pub y_stride: usize,
    /// Chroma planes stride (= ceil(width/2)).
    pub uv_stride: usize,
    /// Luma plane (`y_stride * height` bytes).
    pub y: Vec<u8>,
    /// Cb plane (`uv_stride * ceil(height/2)` bytes).
    pub u: Vec<u8>,
    /// Cr plane (same shape as `u`).
    pub v: Vec<u8>,
}

// BT.601 limited-range coefficients, 8.8 fixed point:
// Y = 16 + (66R + 129G + 25B + 128) >> 8
// U = 128 + (-38R - 74G + 112B + 128) >> 8
// V = 128 + (112R - 94G - 18B + 128) >> 8
fn luma(r: i32, g: i32, b: i32) -> u8 {
    u8::try_from((16 + ((66 * r + 129 * g + 25 * b + 128) >> 8)).clamp(0, 255))
        .expect("clamped to u8")
}

fn chroma_byte(sum: i32) -> u8 {
    u8::try_from((128 + ((sum + 128) >> 8)).clamp(0, 255)).expect("clamped to u8")
}

fn chroma_u(r: i32, g: i32, b: i32) -> i32 {
    -38 * r - 74 * g + 112 * b
}

fn chroma_v(r: i32, g: i32, b: i32) -> i32 {
    112 * r - 94 * g - 18 * b
}

/// Converts a tight RGB24 frame (webcam capture format, §18.57) to a BGRA
/// [`VideoFrame`] buffer (alpha = 255), the encoder input shape. Returns
/// `CodecError::BadDimensions` when the input length does not match
/// `width*height*3`.
pub fn rgb24_to_bgra(rgb: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let pixels = width as usize * height as usize;
    if width == 0 || height == 0 || rgb.len() < pixels * 3 {
        return Err(CodecError::BadDimensions {
            width,
            height,
            stride: width as usize * 3,
            data_len: rgb.len(),
        });
    }
    let mut out = Vec::with_capacity(pixels * 4);
    for px in rgb.as_chunks::<3>().0 {
        // RGB24 → BGRA (channel swap, opaque alpha).
        out.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    Ok(out)
}

/// Converts a BGRA frame (explicit stride, see [`VideoFrame`]) to I420.
pub fn bgra_to_i420(frame: &VideoFrame<'_>) -> Result<I420Frame> {
    let (width, height) = (frame.width as usize, frame.height as usize);
    let row = width * 4;
    if width == 0
        || height == 0
        || frame.stride < row
        || frame.data.len() < frame.stride * (height - 1) + row
    {
        return Err(CodecError::BadDimensions {
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            data_len: frame.data.len(),
        });
    }

    let uv_stride = width.div_ceil(2);
    let uv_rows = height.div_ceil(2);
    let mut y_plane = vec![0u8; width * height];
    let mut u_plane = vec![0u8; uv_stride * uv_rows];
    let mut v_plane = vec![0u8; uv_stride * uv_rows];

    for cy in 0..uv_rows {
        for cx in 0..uv_stride {
            // 2x2 box (clamped at odd edges): average chroma, per-pixel luma.
            let mut sum_u = 0i32;
            let mut sum_v = 0i32;
            let mut count = 0i32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let (px, py) = (cx * 2 + dx, cy * 2 + dy);
                    if px >= width || py >= height {
                        continue;
                    }
                    let off = py * frame.stride + px * 4;
                    let (b, g, r) = (
                        i32::from(frame.data[off]),
                        i32::from(frame.data[off + 1]),
                        i32::from(frame.data[off + 2]),
                    );
                    y_plane[py * width + px] = luma(r, g, b);
                    sum_u += chroma_u(r, g, b);
                    sum_v += chroma_v(r, g, b);
                    count += 1;
                }
            }
            // Average, then fixed-point scale + offset (the +2 rounds).
            let avg_u = (sum_u + (count >> 1)) / count;
            let avg_v = (sum_v + (count >> 1)) / count;
            let idx = cy * uv_stride + cx;
            u_plane[idx] = chroma_byte(avg_u);
            v_plane[idx] = chroma_byte(avg_v);
        }
    }

    Ok(I420Frame {
        width: frame.width,
        height: frame.height,
        y_stride: width,
        uv_stride,
        y: y_plane,
        u: u_plane,
        v: v_plane,
    })
}

/// Converts a BGRA frame to tightly packed NV12 (Y plane, then interleaved
/// U/V). Shared by the hardware encoders that only accept NV12 input
/// (Media Foundation, NVENC).
pub fn bgra_to_nv12(frame: &VideoFrame<'_>) -> Result<Vec<u8>> {
    let i420 = bgra_to_i420(frame)?;
    let mut data = i420.y;
    data.reserve(i420.u.len() * 2);
    for (u, v) in i420.u.iter().zip(i420.v.iter()) {
        data.push(*u);
        data.push(*v);
    }
    Ok(data)
}

/// Converts an I420 frame back to tight BGRA (decode side; BT.601 limited
/// range, chroma replicated 2x2 — no interpolation, matching the cheap
/// subsampling of [`bgra_to_i420`]).
pub fn i420_to_bgra(frame: &I420Frame) -> Result<crate::video::DecodedFrame> {
    let (width, height) = (frame.width as usize, frame.height as usize);
    if width == 0 || height == 0 || frame.y_stride < width {
        return Err(CodecError::BadDimensions {
            width: frame.width,
            height: frame.height,
            stride: frame.y_stride,
            data_len: frame.y.len(),
        });
    }
    let mut data = vec![0u8; width * height * 4];
    let clamp = |v: i32| u8::try_from(v.clamp(0, 255)).expect("clamped to u8");
    for py in 0..height {
        for px in 0..width {
            let luma = i32::from(frame.y[py * frame.y_stride + px]);
            let cb = i32::from(frame.u[(py / 2) * frame.uv_stride + px / 2]) - 128;
            let cr = i32::from(frame.v[(py / 2) * frame.uv_stride + px / 2]) - 128;
            let c = luma - 16;
            let r = (298 * c + 409 * cr + 128) >> 8;
            let g = (298 * c - 100 * cb - 208 * cr + 128) >> 8;
            let b = (298 * c + 516 * cb + 128) >> 8;
            let off = (py * width + px) * 4;
            data[off] = clamp(b);
            data[off + 1] = clamp(g);
            data[off + 2] = clamp(r);
            data[off + 3] = 255;
        }
    }
    Ok(crate::video::DecodedFrame {
        width: frame.width,
        height: frame.height,
        stride: width * 4,
        data,
    })
}

/// Converts an NV12 (Y plane + interleaved UV plane) frame to tight BGRA.
/// Used by the Windows MFT decoder, whose output is NV12 on most hardware.
#[allow(clippy::many_single_char_names)] // Plane/stride names follow the NV12 spec.
pub fn nv12_to_bgra(
    y: &[u8],
    uv: &[u8],
    width: u32,
    height: u32,
    y_stride: usize,
    uv_stride: usize,
) -> Result<crate::video::DecodedFrame> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || y_stride < w || uv_stride < w.div_ceil(2) * 2 {
        return Err(CodecError::BadDimensions {
            width,
            height,
            stride: y_stride,
            data_len: y.len() + uv.len(),
        });
    }
    if y.len() < y_stride * h || uv.len() < uv_stride * h.div_ceil(2) {
        return Err(CodecError::BadDimensions {
            width,
            height,
            stride: y_stride,
            data_len: y.len() + uv.len(),
        });
    }
    let clamp = |v: i32| u8::try_from(v.clamp(0, 255)).expect("clamped to u8");
    let mut data = vec![0u8; w * h * 4];
    for py in 0..h {
        for px in 0..w {
            let luma = i32::from(y[py * y_stride + px]);
            let uv_off = (py / 2) * uv_stride + (px & !1);
            let cb = i32::from(uv[uv_off]) - 128;
            let cr = i32::from(uv[uv_off + 1]) - 128;
            let c = luma - 16;
            let r = (298 * c + 409 * cr + 128) >> 8;
            let g = (298 * c - 100 * cb - 208 * cr + 128) >> 8;
            let b = (298 * c + 516 * cb + 128) >> 8;
            let off = (py * w + px) * 4;
            data[off] = clamp(b);
            data[off + 1] = clamp(g);
            data[off + 2] = clamp(r);
            data[off + 3] = 255;
        }
    }
    Ok(crate::video::DecodedFrame {
        width,
        height,
        stride: w * 4,
        data,
    })
}
