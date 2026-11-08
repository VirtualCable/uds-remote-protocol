// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Webcam codec micro-benchmark (design doc §18.57, Fase 4 F4.1).
//!
//! Measures DECODE cost server-side for the webcam channel candidates:
//! VP9 (our default) vs full-frame JPEG (the possible MJPEG backup). The
//! decision rule agreed with the user: if VP9 decode is reasonably fast,
//! only VP9 is implemented and MJPEG never exists.
//!
//! Run: `cargo run --release -p codec --features codec-vp9,codec-jpeg \
//!       --example bench_cam_decode`

// Benchmark scaffolding: synthetic sizes and timings never approach the
// ranges where these casts lose precision, and strict workspace lints only
// add noise here.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::inconsistent_digit_grouping,
    clippy::struct_field_names
)]

use std::time::Instant;

use codec::rects::Rect;
use codec::{JpegRectsDecoder, JpegRectsEncoder};
use codec::{VideoDecoder, VideoEncoder, VideoFrame};
use codec::{Vp9Decoder, Vp9Encoder};

/// Frames measured per resolution/codec (after 5 warmup frames).
const FRAMES: usize = 120;
/// Realistic webcam bitrate target for VP9 (kbps).
const VP9_KBPS: u32 = 1500;
/// JPEG quality comparable to a typical webcam MJPEG stream.
const JPEG_QUALITY: i32 = 70;

/// Builds a synthetic BGRA frame with a moving block and per-pixel noise,
/// so encoders do real work (a static frame encodes to ~nothing).
fn synth_frame(width: u32, height: u32, tick: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut data = vec![0u8; w * h * 4];
    // Moving 1/4-size block.
    let bw = w / 4;
    let bh = h / 4;
    let x0 = ((tick as usize) * 7) % (w - bw);
    let y0 = ((tick as usize) * 3) % (h - bh);
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            // Cheap deterministic noise (LCG) so the frame is not flat.
            let n = (x as u32)
                .wrapping_mul(1_664_525)
                .wrapping_add(y as u32)
                .wrapping_mul(2_143_223)
                .wrapping_add(tick.wrapping_mul(101_390_4223))
                >> 24;
            let in_block = x >= x0 && x < x0 + bw && y >= y0 && y < y0 + bh;
            let base = if in_block { 200u8 } else { 60 };
            data[i] = base.wrapping_add(n as u8 & 0x3F);
            data[i + 1] = base.wrapping_add((n >> 2) as u8 & 0x3F);
            data[i + 2] = base.wrapping_add((n >> 4) as u8 & 0x3F);
            data[i + 3] = 255;
        }
    }
    data
}

struct Stats {
    avg_ms: f64,
    p95_ms: f64,
    min_ms: f64,
    max_ms: f64,
}

fn stats(mut times: Vec<f64>) -> Stats {
    times.sort_by(f64::total_cmp);
    let avg = times.iter().sum::<f64>() / times.len() as f64;
    let p95 = times[(times.len() * 95 / 100).min(times.len() - 1)];
    Stats {
        avg_ms: avg,
        p95_ms: p95,
        min_ms: times[0],
        max_ms: times[times.len() - 1],
    }
}

fn bench_vp9(width: u32, height: u32) -> (Stats, f64) {
    let mut enc = Vp9Encoder::new(width, height, VP9_KBPS).expect("vp9 encoder");
    let mut dec = Vp9Decoder::new().expect("vp9 decoder");
    let mut times = Vec::with_capacity(FRAMES);
    let mut total_bytes = 0usize;
    for tick in 0..(FRAMES + 5) {
        let data = synth_frame(width, height, tick as u32);
        let frame = VideoFrame {
            width,
            height,
            stride: width as usize * 4,
            data: &data,
        };
        let packet = enc.encode(&frame).expect("encode");
        total_bytes += packet.data.len();
        let t0 = Instant::now();
        let decoded = dec.decode(&packet).expect("decode").expect("frame");
        let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(decoded.width, width);
        if tick >= 5 {
            times.push(elapsed);
        }
    }
    let mbps = total_bytes as f64 * 8.0 / (FRAMES as f64 / 30.0) / 1_000_000.0;
    (stats(times), mbps)
}

fn bench_jpeg(width: u32, height: u32) -> (Stats, f64) {
    let mut enc = JpegRectsEncoder::new(width, height, JPEG_QUALITY).expect("jpeg encoder");
    let mut dec = JpegRectsDecoder::new();
    let full = Rect {
        left: 0,
        top: 0,
        right: width as i32,
        bottom: height as i32,
    };
    let mut times = Vec::with_capacity(FRAMES);
    let mut total_bytes = 0usize;
    for tick in 0..(FRAMES + 5) {
        let data = synth_frame(width, height, tick as u32);
        let frame = VideoFrame {
            width,
            height,
            stride: width as usize * 4,
            data: &data,
        };
        let packet = enc
            .encode_with_dirty(&frame, std::slice::from_ref(&full))
            .expect("encode");
        total_bytes += packet.data.len();
        let t0 = Instant::now();
        let decoded = dec.decode(&packet).expect("decode").expect("frame");
        let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(decoded.width, width);
        if tick >= 5 {
            times.push(elapsed);
        }
    }
    let mbps = total_bytes as f64 * 8.0 / (FRAMES as f64 / 30.0) / 1_000_000.0;
    (stats(times), mbps)
}

fn report(name: &str, width: u32, height: u32, s: &Stats, mbps: f64) {
    println!(
        "{name} {width}x{height}: decode avg {:.2} ms | p95 {:.2} ms | min {:.2} | max {:.2} | ~{:.1} Mbps @30fps",
        s.avg_ms, s.p95_ms, s.min_ms, s.max_ms, mbps
    );
}

fn main() {
    for (w, h) in [(1280u32, 720u32), (1920, 1080)] {
        let (vp9, vp9_mbps) = bench_vp9(w, h);
        report("VP9 ", w, h, &vp9, vp9_mbps);
        let (jpeg, jpeg_mbps) = bench_jpeg(w, h);
        report("JPEG", w, h, &jpeg, jpeg_mbps);
    }
}
