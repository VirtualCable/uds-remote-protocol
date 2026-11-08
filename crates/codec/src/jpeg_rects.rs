// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! JPEG-over-dirty-rects codec (feature `codec-jpeg`, design doc §8.5).
//!
//! The base regime of the codec ladder: each frame is encoded as a batch of
//! independently-coded JPEG rectangles covering what changed since the
//! previous frame (tile diff + merge, see [`crate::rects`]). Encoder CPU is
//! minimal (libjpeg-turbo SIMD) and idle desktops emit empty packets; the
//! ladder switches to VP9 when the sustained changed area makes JPEG's
//! per-rect bandwidth uncompetitive (measured crossing: ~5 % of the screen).
//!
//! Wire format of an [`EncodedPacket`] (all integers little-endian):
//! `u32 width | u32 height | u16 rect_count`, then per rect
//! `u16 x | u16 y | u16 w | u16 h | u32 jpeg_len | jpeg bytes`.
//! An empty packet (zero bytes) means "no changes"; it is never produced —
//! the sender skips those frames instead.
//!
//! Loss handling: every rect is independent and idempotent, so a lost
//! datagram leaves a stale region only until its next update; `keyframe`
//! requests force a full-screen rect.

// Rect geometry math is full of small, provably in-range integer conversions.
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::many_single_char_names
)]

use crate::rects::{self, Rect};
use crate::{
    CodecError, DecodedFrame, EncodedPacket, Result, VideoCodecType, VideoDecoder, VideoEncoder,
    VideoFrame,
};

const HEADER_LEN: usize = 10; // u32 w | u32 h | u16 count
const RECT_HEADER_LEN: usize = 12; // u16 x,y,w,h | u32 len

/// JPEG encoder over dirty rects (libjpeg-turbo).
pub struct JpegRectsEncoder {
    width: u32,
    height: u32,
    quality: i32,
    /// True once the quality was set from outside (§18.69): the automatic
    /// bitrate→quality mapping in `set_bitrate` then steps aside.
    quality_explicit: bool,
    prev: Option<Vec<u8>>,
    force_full: bool,
}

impl JpegRectsEncoder {
    /// Creates an encoder for `width`x`height` frames at JPEG `quality` (1-100).
    pub fn new(width: u32, height: u32, quality: i32) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(CodecError::BadDimensions {
                width,
                height,
                stride: 0,
                data_len: 0,
            });
        }
        Ok(Self {
            width,
            height,
            quality: quality.clamp(1, 100),
            quality_explicit: false,
            prev: None,
            force_full: false,
        })
    }

    /// Sets the JPEG quality (1-100) at runtime (§18.69): takes effect on the
    /// next encoded rect and disables the automatic bitrate→quality mapping.
    pub fn set_quality(&mut self, quality: i32) {
        self.quality = quality.clamp(1, 100);
        self.quality_explicit = true;
    }

    /// Encodes `frame` against a caller-computed dirty-rect set (used by the
    /// codec ladder, which needs the rects itself for the changed-area
    /// metric). An empty set yields an empty packet ("no changes"). Updates
    /// the internal previous-frame copy so later [`VideoEncoder::encode`]
    /// calls stay consistent.
    pub fn encode_with_dirty(&mut self, frame: &VideoFrame<'_>, dirty: &[Rect]) -> Result<EncodedPacket> {
        let prev = frame.data.to_vec();

        if dirty.is_empty() {
            self.prev = Some(prev);
            return Ok(EncodedPacket {
                data: Vec::new(),
                keyframe: false,
            });
        }

        let mut compressor =
            turbojpeg::Compressor::new().map_err(|e| CodecError::Backend(e.to_string()))?;
        compressor
            .set_quality(self.quality)
            .map_err(|e| CodecError::Backend(e.to_string()))?;

        let full = dirty.len() == 1
            && dirty[0].left == 0
            && dirty[0].top == 0
            && dirty[0].right == self.width as i32
            && dirty[0].bottom == self.height as i32;

        let mut out = Vec::with_capacity(HEADER_LEN + dirty.len() * RECT_HEADER_LEN);
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        #[allow(clippy::cast_possible_truncation)] // merge() caps at MAX_RECTS
        out.extend_from_slice(&(dirty.len() as u16).to_le_bytes());
        for r in dirty {
            let Some((pixels, w, h)) = crop(frame, r) else {
                continue;
            };
            let jpeg = compressor
                .compress_to_owned(turbojpeg::Image {
                    pixels: &pixels,
                    width: w,
                    pitch: w * 4,
                    height: h,
                    format: turbojpeg::PixelFormat::BGRA,
                })
                .map_err(|e| CodecError::Backend(e.to_string()))?;
            #[allow(clippy::cast_possible_truncation)] // screen dims and jpeg sizes fit
            {
                out.extend_from_slice(&(r.left.max(0) as u16).to_le_bytes());
                out.extend_from_slice(&(r.top.max(0) as u16).to_le_bytes());
                out.extend_from_slice(&(w as u16).to_le_bytes());
                out.extend_from_slice(&(h as u16).to_le_bytes());
                out.extend_from_slice(&(jpeg.len() as u32).to_le_bytes());
            }
            out.extend_from_slice(&jpeg);
        }
        self.prev = Some(prev);
        Ok(EncodedPacket {
            data: out,
            keyframe: full,
        })
    }
}

/// Crops a tight BGRA copy of `rect` out of `frame`.
fn crop(frame: &VideoFrame<'_>, rect: &Rect) -> Option<(Vec<u8>, usize, usize)> {
    let l = rect.left.clamp(0, frame.width as i32) as usize;
    let t = rect.top.clamp(0, frame.height as i32) as usize;
    let r = rect.right.clamp(0, frame.width as i32) as usize;
    let b = rect.bottom.clamp(0, frame.height as i32) as usize;
    let (w, h) = (r.checked_sub(l)?, b.checked_sub(t)?);
    if w == 0 || h == 0 {
        return None;
    }
    let mut out = vec![0u8; w * h * 4];
    for row in 0..h {
        let src = (t + row) * frame.stride + l * 4;
        out[row * w * 4..(row + 1) * w * 4].copy_from_slice(&frame.data[src..src + w * 4]);
    }
    Some((out, w, h))
}

impl VideoEncoder for JpegRectsEncoder {
    fn encode(&mut self, frame: &VideoFrame<'_>) -> Result<EncodedPacket> {
        if frame.width != self.width || frame.height != self.height {
            return Err(CodecError::BadDimensions {
                width: frame.width,
                height: frame.height,
                stride: frame.stride,
                data_len: frame.data.len(),
            });
        }
        let dirty = if self.force_full {
            self.force_full = false;
            vec![Rect {
                left: 0,
                top: 0,
                right: self.width as i32,
                bottom: self.height as i32,
            }]
        } else {
            rects::dirty_rects(self.prev.as_deref(), frame.data, self.width, self.height, frame.stride)
        };
        self.encode_with_dirty(frame, &dirty)
    }

    fn request_keyframe(&mut self) {
        self.force_full = true;
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        // Rects has no rate control: the congestion controller jumps presets
        // (§8.4/§8.5) instead of tuning this knob. Map pressure onto quality
        // as a graceful degradation: below ~1.5 Mbps the base preset's
        // bandwidth is already unreachable, so step quality down.
        if !self.quality_explicit {
            self.quality = if kbps < 1500 { 50 } else { 80 };
        }
        Ok(())
    }

    fn codec_type(&self) -> VideoCodecType {
        VideoCodecType::JpegRects
    }
}

/// JPEG-over-rects decoder: blits decoded rects onto a persistent canvas.
pub struct JpegRectsDecoder {
    width: u32,
    height: u32,
    canvas: Vec<u8>,
}

impl JpegRectsDecoder {
    /// Creates a decoder; the canvas is (re)sized from each packet header.
    #[must_use]
    pub fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            canvas: Vec::new(),
        }
    }
}

impl Default for JpegRectsDecoder {
    fn default() -> Self {
        Self::new()
    }
}

fn take<'a>(buf: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    if buf.len() < n {
        return Err(CodecError::Malformed);
    }
    let (head, tail) = buf.split_at(n);
    *buf = tail;
    Ok(head)
}

fn u16_at(buf: &mut &[u8]) -> Result<u16> {
    Ok(u16::from_le_bytes(take(buf, 2)?.try_into().expect("2 bytes")))
}

fn u32_at(buf: &mut &[u8]) -> Result<u32> {
    Ok(u32::from_le_bytes(take(buf, 4)?.try_into().expect("4 bytes")))
}

/// Upper bound for the geometry a packet header may declare: anything
/// bigger is a mis-parse (or an attack), not a real display. 8192² RGBA is
/// 256 MiB — already far beyond any realistic remote desktop.
const MAX_FRAME_DIM: u32 = 8192;

impl VideoDecoder for JpegRectsDecoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<DecodedFrame> {
        let mut buf: &[u8] = &packet.data;
        if buf.is_empty() {
            // "No changes" packet: return the canvas as-is (must exist).
            if self.canvas.is_empty() {
                return Err(CodecError::Malformed);
            }
        } else {
            let width = u32_at(&mut buf)?;
            let height = u32_at(&mut buf)?;
            let count = u16_at(&mut buf)? as usize;
            if width == 0 || height == 0 || width > MAX_FRAME_DIM || height > MAX_FRAME_DIM {
                return Err(CodecError::Malformed);
            }
            if width != self.width || height != self.height {
                self.width = width;
                self.height = height;
                self.canvas = vec![0u8; width as usize * height as usize * 4];
            }
            let stride = width as usize * 4;
            for _ in 0..count {
                let x = u16_at(&mut buf)? as usize;
                let y = u16_at(&mut buf)? as usize;
                let w = u16_at(&mut buf)? as usize;
                let h = u16_at(&mut buf)? as usize;
                let len = u32_at(&mut buf)? as usize;
                let jpeg = take(&mut buf, len)?;
                let rect = turbojpeg::decompress(jpeg, turbojpeg::PixelFormat::BGRA)
                    .map_err(|e| CodecError::Backend(e.to_string()))?;
                if rect.width != w || rect.height != h {
                    return Err(CodecError::Malformed);
                }
                if x + w > self.width as usize || y + h > self.height as usize {
                    return Err(CodecError::Malformed);
                }
                for row in 0..h {
                    let dst = (y + row) * stride + x * 4;
                    self.canvas[dst..dst + w * 4]
                        .copy_from_slice(&rect.pixels[row * w * 4..(row + 1) * w * 4]);
                }
            }
        }
        Ok(DecodedFrame {
            width: self.width,
            height: self.height,
            stride: self.width as usize * 4,
            data: self.canvas.clone(),
        })
    }

    fn codec_type(&self) -> VideoCodecType {
        VideoCodecType::JpegRects
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::chunks_exact_to_as_chunks)] // pixel-quad iteration reads better
    use super::*;

    fn frame(width: u32, height: u32, px: [u8; 4]) -> Vec<u8> {
        let mut v = vec![0u8; width as usize * height as usize * 4];
        for chunk in v.chunks_exact_mut(4) {
            chunk.copy_from_slice(&px);
        }
        v
    }

    #[test]
    fn roundtrip_two_frames() {
        let (w, h) = (256u32, 128u32);
        let mut enc = JpegRectsEncoder::new(w, h, 80).unwrap();
        let mut dec = JpegRectsDecoder::new();

        let f1 = frame(w, h, [10, 20, 30, 255]);
        let vf1 = VideoFrame { width: w, height: h, stride: w as usize * 4, data: &f1 };
        let p1 = enc.encode(&vf1).unwrap();
        assert!(p1.keyframe); // first frame is a full-screen rect
        let d1 = dec.decode(&p1).unwrap();
        assert_eq!((d1.width, d1.height), (w, h));
        // JPEG is lossy: allow a small per-channel error.
        assert!(d1.data.chunks_exact(4).all(|c| {
            (i16::from(c[0]) - 10).abs() <= 2
                && (i16::from(c[1]) - 20).abs() <= 2
                && (i16::from(c[2]) - 30).abs() <= 2
        }));

        // Change a 16x16 block; the rest must survive from the canvas.
        let mut f2 = f1.clone();
        for y in 64..80usize {
            for x in 64..80usize {
                f2[(y * w as usize + x) * 4..][..4].copy_from_slice(&[200, 100, 50, 255]);
            }
        }
        let vf2 = VideoFrame { width: w, height: h, stride: w as usize * 4, data: &f2 };
        let p2 = enc.encode(&vf2).unwrap();
        assert!(!p2.keyframe);
        assert!(p2.data.len() < p1.data.len());
        let d2 = dec.decode(&p2).unwrap();
        let changed = (72 * w as usize + 72) * 4;
        assert!((i16::from(d2.data[changed]) - 200).abs() <= 8);
        let untouched = (10 * w as usize + 10) * 4;
        assert!((i16::from(d2.data[untouched + 1]) - 20).abs() <= 2);
    }

    #[test]
    fn unchanged_frame_yields_empty_packet() {
        let mut enc = JpegRectsEncoder::new(64, 64, 80).unwrap();
        let f = frame(64, 64, [1, 2, 3, 255]);
        let vf = VideoFrame { width: 64, height: 64, stride: 256, data: &f };
        enc.encode(&vf).unwrap();
        let p = enc.encode(&vf).unwrap();
        assert_eq!(p.data, [] as [u8; 0]);
    }

    #[test]
    fn request_keyframe_forces_full_rect() {
        let mut enc = JpegRectsEncoder::new(64, 64, 80).unwrap();
        let f = frame(64, 64, [1, 2, 3, 255]);
        let vf = VideoFrame { width: 64, height: 64, stride: 256, data: &f };
        enc.encode(&vf).unwrap();
        enc.request_keyframe();
        let p = enc.encode(&vf).unwrap();
        assert!(p.keyframe);
    }

    #[test]
    fn decoder_rejects_truncated_packet() {
        let mut dec = JpegRectsDecoder::new();
        for len in [1usize, 5, 9, 11, 15] {
            let p = EncodedPacket { data: vec![0u8; len], keyframe: false };
            assert!(dec.decode(&p).is_err(), "len {len} must fail");
        }
    }

    #[test]
    fn decoder_rejects_out_of_bounds_rect() {
        let mut enc = JpegRectsEncoder::new(64, 64, 80).unwrap();
        let f = frame(64, 64, [1, 2, 3, 255]);
        let vf = VideoFrame { width: 64, height: 64, stride: 256, data: &f };
        let mut p = enc.encode(&vf).unwrap();
        // Corrupt the rect x so it overruns the canvas (x=65535).
        p.data[10] = 0xFF;
        p.data[11] = 0xFF;
        let mut dec = JpegRectsDecoder::new();
        assert!(dec.decode(&p).is_err());
    }
}
