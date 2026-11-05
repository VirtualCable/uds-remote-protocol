// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Pixel format conversion for the virtual camera: the shared slot carries
//! tight BGRA (what the VP9 decoder produces); the filter converts to
//! whatever media type the consuming application negotiated — NV12 (the
//! common camera format) or RGB24 (bottom-up DIB, the DShow classic).
//!
//! Pure Rust, no Windows dependencies: unit tested on the host platform.

// Pixel math uses fixed-range integer casts throughout (BT.601 with clamped
// intermediates); pedantic cast lints add noise here.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::unnecessary_cast,
    clippy::cast_lossless,
    clippy::many_single_char_names
)]

/// Bytes of one NV12 frame (`w*h*3/2`).
#[must_use]
pub fn nv12_len(width: u32, height: u32) -> usize {
    (width as usize) * (height as usize) * 3 / 2
}

/// Bytes of one RGB24 frame (`w*h*3`).
#[must_use]
pub fn rgb24_len(width: u32, height: u32) -> usize {
    (width as usize) * (height as usize) * 3
}

/// Fills `out` with a black NV12 frame (studio swing: Y=16, U=V=128).
pub fn nv12_black(width: u32, height: u32, out: &mut [u8]) {
    let lum = (width as usize) * (height as usize);
    debug_assert!(out.len() >= nv12_len(width, height));
    out[..lum].fill(16);
    out[lum..lum * 3 / 2].fill(128);
}

/// Fills `out` with a black RGB24 frame.
pub fn rgb24_black(width: u32, height: u32, out: &mut [u8]) {
    debug_assert!(out.len() >= rgb24_len(width, height));
    out[..rgb24_len(width, height)].fill(0);
}

/// BT.601 studio-swing luma for one RGB triplet.
#[inline]
fn luma601(r: u32, g: u32, b: u32) -> u8 {
    (16 + ((66 * r + 129 * g + 25 * b + 128) >> 8)) as u8
}

/// BT.601 studio-swing chroma for a 2x2 averaged RGB.
#[inline]
fn chroma601(r: u32, g: u32, b: u32) -> (u8, u8) {
    let u = (128 + ((-38 * r as i32 - 74 * g as i32 + 112 * b as i32 + 128) >> 8)) as i32;
    let v = (128 + ((112 * r as i32 - 94 * g as i32 - 18 * b as i32 + 128) >> 8)) as i32;
    (u.clamp(0, 255) as u8, v.clamp(0, 255) as u8)
}

/// Converts tight BGRA to NV12 (Y plane then interleaved UV, 2x2 averaged).
///
/// Odd dimensions: the last column/row samples reuse the previous one (the
/// webcam negotiates even sizes anyway; this only guards the math).
pub fn bgra_to_nv12(bgra: &[u8], width: u32, height: u32, out: &mut [u8]) {
    let w = width as usize;
    let h = height as usize;
    debug_assert!(bgra.len() >= w * h * 4);
    debug_assert!(out.len() >= nv12_len(width, height));

    // Y plane.
    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            let p = (row + x) * 4;
            let (b, g, r) = (bgra[p] as u32, bgra[p + 1] as u32, bgra[p + 2] as u32);
            out[row + x] = luma601(r, g, b);
        }
    }

    // Interleaved UV plane.
    let uv_base = w * h;
    for cy in 0..h.div_ceil(2) {
        for cx in 0..w.div_ceil(2) {
            // Average the 2x2 block (clamped at the edges).
            let mut r = 0u32;
            let mut g = 0u32;
            let mut b = 0u32;
            let mut n = 0u32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let (x, y) = (2 * cx + dx, 2 * cy + dy);
                    if x < w && y < h {
                        let p = (y * w + x) * 4;
                        b += bgra[p] as u32;
                        g += bgra[p + 1] as u32;
                        r += bgra[p + 2] as u32;
                        n += 1;
                    }
                }
            }
            let (u, v) = chroma601(r / n, g / n, b / n);
            let q = uv_base + (cy * w.div_ceil(2) + cx) * 2;
            out[q] = u;
            out[q + 1] = v;
        }
    }
}

/// Converts tight BGRA to RGB24 as a **bottom-up** DIB (positive biHeight
/// convention in `VIDEOINFOHEADER`).
pub fn bgra_to_rgb24_bottom_up(bgra: &[u8], width: u32, height: u32, out: &mut [u8]) {
    let w = width as usize;
    let h = height as usize;
    debug_assert!(bgra.len() >= w * h * 4);
    debug_assert!(out.len() >= rgb24_len(width, height));
    for y in 0..h {
        let src = y * w * 4;
        let dst = (h - 1 - y) * w * 3;
        for x in 0..w {
            let (s, d) = (src + x * 4, dst + x * 3);
            out[d] = bgra[s];
            out[d + 1] = bgra[s + 1];
            out[d + 2] = bgra[s + 2];
        }
    }
}

/// Nearest-neighbor rescale of a tight BGRA frame into a new tight buffer.
///
/// Used when the consumer negotiates a geometry different from the
/// producer's: the shared slot carries the native webcam frame and the
/// filter serves whatever size the application asked for.
#[must_use]
pub fn scale_bgra_nearest(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    debug_assert!(src.len() >= sw * sh * 4);
    let mut out = vec![0u8; dw * dh * 4];
    for y in 0..dh {
        let sy = (y * sh / dh).min(sh - 1);
        for x in 0..dw {
            let sx = (x * sw / dw).min(sw - 1);
            let (s, d) = ((sy * sw + sx) * 4, (y * dw + x) * 4);
            out[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_nearest_downsamples() {
        // 4x4 with each quadrant a solid color: B=10 (TL), B=20 (TR),
        // B=30 (BL), B=40 (BR).
        let mut src = vec![0u8; 4 * 4 * 4];
        for y in 0..4 {
            for x in 0..4 {
                let v = 10 * (1 + x / 2 + 2 * (y / 2)) as u8;
                src[(y * 4 + x) * 4] = v;
            }
        }
        let out = scale_bgra_nearest(&src, 4, 4, 2, 2);
        assert_eq!(out[0], 10); // TL
        assert_eq!(out[4], 20); // TR
        assert_eq!(out[8], 30); // BL
        assert_eq!(out[12], 40); // BR
    }

    #[test]
    fn scale_nearest_identity() {
        let src: Vec<u8> = (0..16).collect();
        let out = scale_bgra_nearest(&src, 2, 2, 2, 2);
        assert_eq!(out, src);
    }

    #[test]
    fn nv12_len_matches_planes() {
        assert_eq!(nv12_len(2, 2), 6);
        assert_eq!(nv12_len(1280, 720), 1280 * 720 * 3 / 2);
    }

    #[test]
    fn black_frames_have_expected_fill() {
        let mut nv = vec![0xAA; nv12_len(4, 4)];
        nv12_black(4, 4, &mut nv);
        assert_eq!(&nv[..16], &[16u8; 16]);
        assert_eq!(&nv[16..], &[128u8; 8]);
        let mut rgb = vec![0xAA; rgb24_len(4, 4)];
        rgb24_black(4, 4, &mut rgb);
        assert!(rgb.iter().all(|&b| b == 0));
    }

    #[test]
    fn white_bgra_gives_max_luma() {
        // BGRA white.
        let bgra = [255u8, 255, 255, 0, 255, 255, 255, 0, 255, 255, 255, 0, 255, 255, 255, 0];
        let mut nv = vec![0u8; nv12_len(2, 2)];
        bgra_to_nv12(&bgra, 2, 2, &mut nv);
        assert_eq!(&nv[..4], &[235u8; 4]); // studio-swing white
        assert_eq!(&nv[4..], &[128u8, 128]); // neutral chroma
    }

    #[test]
    fn rgb24_is_bottom_up() {
        // 2x2: top row red, bottom row blue (BGRA).
        let bgra = [
            255, 0, 0, 0, 255, 0, 0, 0, // top row: blue in BGRA? no: B=255 → blue
            0, 0, 255, 0, 0, 0, 255, 0, // bottom row: R=255 → red
        ];
        let mut rgb = vec![0u8; rgb24_len(2, 2)];
        bgra_to_rgb24_bottom_up(&bgra, 2, 2, &mut rgb);
        // First bytes of the DIB are the BOTTOM source row (red).
        assert_eq!(&rgb[..3], &[0, 0, 255]);
        // Last bytes are the TOP source row (blue).
        assert_eq!(&rgb[9..12], &[255, 0, 0]);
    }
}
