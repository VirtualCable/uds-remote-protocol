//! VP8 vs VP9 benchmark (design doc §8: CPU cost vs compression ratio).
//!
//! Ignored by default — run on demand, in release, with:
//! `cargo test -p codec --release --features "codec-vp8 codec-vp9" --test codec_bench -- --ignored --nocapture`

#![cfg(all(feature = "codec-vp8", feature = "codec-vp9"))]
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use std::time::Instant;

use codec::{VideoEncoder, VideoFrame};

/// Moving pattern: gradient + shifting bar (encode load representative of a
/// desktop with a changing region).
fn frame_at(width: u32, height: u32, tick: u32) -> Vec<u8> {
    let mut data = vec![0u8; (width * height * 4) as usize];
    let bar_x = (tick as usize * 7) % width as usize;
    for y in 0..height as usize {
        for x in 0..width as usize {
            let off = (y * width as usize + x) * 4;
            if x >= bar_x && x < bar_x + 40 {
                data[off] = 255;
                data[off + 1] = 255;
                data[off + 2] = 255;
            } else {
                data[off] = (x % 256) as u8;
                data[off + 1] = (y % 256) as u8;
                data[off + 2] = 128;
            }
            data[off + 3] = 255;
        }
    }
    data
}

fn bench<E: VideoEncoder>(name: &str, mut enc: E, frames: &[Vec<u8>], w: u32, h: u32) {
    let mut total_bytes = 0usize;
    let start = Instant::now();
    for (i, data) in frames.iter().enumerate() {
        let frame = VideoFrame {
            width: w,
            height: h,
            stride: (w * 4) as usize,
            data,
        };
        let packet = enc.encode(&frame).expect("encode");
        total_bytes += packet.data.len();
        if i == 0 {
            assert!(packet.keyframe);
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let fps = frames.len() as f64 / elapsed;
    println!(
        "{name}: {n} frames {w}x{h} in {elapsed:.2}s = {fps:.1} fps ({ms:.1} ms/frame), \
         {kb:.0} KiB total, {kbf:.1} KiB/frame",
        n = frames.len(),
        ms = elapsed * 1000.0 / frames.len() as f64,
        kb = total_bytes as f64 / 1024.0,
        kbf = total_bytes as f64 / 1024.0 / frames.len() as f64,
    );
}

#[test]
#[ignore = "benchmark: run explicitly in release"]
fn vp8_vs_vp9_1080p() {
    let (w, h, n, kbps) = (1920u32, 1080u32, 90u32, 4000u32);
    let frames: Vec<Vec<u8>> = (0..n).map(|t| frame_at(w, h, t)).collect();
    println!("--- {n} frames {w}x{h} @ {kbps} kbps target ---");
    bench(
        "VP8",
        codec::Vp8Encoder::new(w, h, kbps).expect("vp8"),
        &frames,
        w,
        h,
    );
    bench(
        "VP9",
        codec::Vp9Encoder::new(w, h, kbps).expect("vp9"),
        &frames,
        w,
        h,
    );
}

/// Mean PSNR (dB) over B/G/R between the source BGRA frames and their
/// decoded counterparts. Relative reference only: the BGRA→I420→BGRA
/// roundtrip affects both codecs equally.
fn psnr_vs_source(src: &[Vec<u8>], dec: &[codec::DecodedFrame]) -> f64 {
    let mut sum_sq = 0f64;
    let mut count = 0u64;
    for (s, d) in src.iter().zip(dec.iter()) {
        for (sp, dp) in s.as_chunks::<4>().0.iter().zip(d.data.as_chunks::<4>().0.iter()) {
            for c in 0..3 {
                let diff = f64::from(sp[c]) - f64::from(dp[c]);
                sum_sq += diff * diff;
                count += 1;
            }
        }
    }
    let mse = sum_sq / count.max(1) as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

/// Encode+decode sweep: speed, real emitted bitrate and quality (PSNR).
fn sweep(name: &str, preset: codec::CodecPreset, frames: &[Vec<u8>], w: u32, h: u32) {
    let mut enc = preset.build_encoder(w, h).expect("encoder");
    let mut dec = codec::build_decoder(preset.codec).expect("decoder");
    let mut total_bytes = 0usize;
    let mut decoded = Vec::with_capacity(frames.len());
    let start = Instant::now();
    for data in frames {
        let frame = VideoFrame {
            width: w,
            height: h,
            stride: (w * 4) as usize,
            data,
        };
        let packet = enc.encode(&frame).expect("encode");
        total_bytes += packet.data.len();
        decoded.push(dec.decode(&packet).expect("decode"));
    }
    let elapsed = start.elapsed().as_secs_f64();
    let fps = frames.len() as f64 / elapsed;
    // Real bitrate had this clip been served at 30 fps.
    let real_kbps = total_bytes as f64 * 8.0 / 1000.0 / (frames.len() as f64 / 30.0);
    let psnr = psnr_vs_source(frames, &decoded);
    println!(
        "{name:<28} target {:>5} kbps | encode {fps:>5.1} fps | real {real_kbps:>6.0} kbps @30fps | PSNR {psnr:>5.2} dB",
        preset.params.bitrate_kbps,
    );
}

#[test]
#[ignore = "benchmark: run explicitly in release"]
fn bitrate_sweep_1080p() {
    let (w, h, n) = (1920u32, 1080u32, 90u32);
    let frames: Vec<Vec<u8>> = (0..n).map(|t| frame_at(w, h, t)).collect();
    println!("--- {n} frames {w}x{h}, bitrate sweep (balanced presets) ---");
    for kbps in [500u32, 1000, 2000, 4000, 8000] {
        for name in ["vp9-balanced", "vp8-balanced"] {
            let preset = codec::CodecPreset::by_name(name)
                .expect("preset")
                .with_bitrate(kbps);
            sweep(name, preset, &frames, w, h);
        }
    }
}
