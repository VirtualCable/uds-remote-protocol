// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

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

/// Process CPU time (user+system) in milliseconds, Linux `/proc`.
/// Benchmark-only: needed because ROW_MT makes libvpx multi-threaded, so
/// wall time per frame underestimates real CPU cost.
#[cfg(target_os = "linux")]
fn process_cpu_ms() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("proc stat");
    let after_paren = stat.rsplit(')').next().expect("comm paren");
    let fields: Vec<&str> = after_paren.split_whitespace().collect();
    // Fields 14/15 overall => indices 11/12 after removing pid+comm.
    let ticks: f64 = fields[11].parse::<f64>().unwrap_or(0.0)
        + fields[12].parse::<f64>().unwrap_or(0.0);
    ticks * 10.0 // clock ticks are 10 ms on Linux (USER_HZ=100)
}

/// Process CPU time (user+kernel) in milliseconds, Windows `GetProcessTimes`.
#[cfg(windows)]
fn process_cpu_ms() -> f64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: all out-params point to valid FILETIME locals.
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    }
    .expect("GetProcessTimes");
    let ticks = |ft: FILETIME| (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    // FILETIME ticks are 100 ns.
    (ticks(kernel) + ticks(user)) as f64 / 10_000.0
}

#[cfg(any(target_os = "linux", windows))]
fn cpu_since(c0: f64) -> f64 {
    process_cpu_ms() - c0
}

/// Encode+decode sweep: speed, real emitted bitrate, quality (PSNR) and
/// CPU cost (encode/decode ms per frame, and % of one core serving 30 fps).
fn sweep(name: &str, preset: codec::CodecPreset, frames: &[Vec<u8>], w: u32, h: u32) {
    let mut enc = preset.build_encoder(w, h).expect("encoder");
    let mut dec = codec::build_decoder(preset.codec).expect("decoder");
    let mut total_bytes = 0usize;
    let mut decoded = Vec::with_capacity(frames.len());
    let mut enc_cpu = 0.0;
    let mut dec_cpu = 0.0;
    let start = Instant::now();
    for data in frames {
        let frame = VideoFrame {
            width: w,
            height: h,
            stride: (w * 4) as usize,
            data,
        };
        #[cfg(any(target_os = "linux", windows))]
        let c0 = process_cpu_ms();
        let packet = enc.encode(&frame).expect("encode");
        #[cfg(any(target_os = "linux", windows))]
        {
            enc_cpu += cpu_since(c0);
        }
        total_bytes += packet.data.len();
        #[cfg(any(target_os = "linux", windows))]
        let c1 = process_cpu_ms();
        decoded.push(dec.decode(&packet).expect("decode"));
        #[cfg(any(target_os = "linux", windows))]
        {
            dec_cpu += cpu_since(c1);
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let fps = frames.len() as f64 / elapsed;
    // Real bitrate had this clip been served at 30 fps.
    let real_kbps = total_bytes as f64 * 8.0 / 1000.0 / (frames.len() as f64 / 30.0);
    let psnr = psnr_vs_source(frames, &decoded);
    let n = frames.len() as f64;
    let (enc_ms, dec_ms) = (enc_cpu / n, dec_cpu / n);
    // % of one CPU core if the server streamed 30 fps with these costs.
    let enc_pct = enc_ms * 30.0 / 10.0;
    let dec_pct = dec_ms * 30.0 / 10.0;
    println!(
        "{name:<22} {:>5}k | enc {enc_ms:>5.1} ms/f ({enc_pct:>4.0}%) | dec {dec_ms:>4.1} ms/f ({dec_pct:>3.0}%) | real {real_kbps:>5.0}k | {psnr:>5.2} dB | wall {fps:>4.1} fps",
        preset.params.bitrate_kbps,
    );
}

#[test]
#[ignore = "benchmark: run explicitly in release"]
fn bitrate_sweep_1080p() {
    let (w, h, n) = (1920u32, 1080u32, 90u32);
    let frames: Vec<Vec<u8>> = (0..n).map(|t| frame_at(w, h, t)).collect();
    println!("--- {n} frames {w}x{h}: default presets + bitrate sweep (CPU = % of one core @30fps) ---");
    // Defaults as shipped in the catalog.
    for name in [
        "vp9-bandwidth-saver",
        "vp9-balanced",
        "vp9-low-latency",
        "vp8-balanced",
        "vp8-low-latency",
        "vp8-quality",
    ] {
        let preset = codec::CodecPreset::by_name(name).expect("preset");
        sweep(name, preset, &frames, w, h);
    }
    // VP9 quality-vs-bitrate reference at the balanced speed.
    for kbps in [500u32, 1000, 2000, 4000, 8000] {
        let preset = codec::CodecPreset::by_name("vp9-balanced")
            .expect("preset")
            .with_bitrate(kbps);
        sweep("vp9-balanced", preset, &frames, w, h);
    }
}

/// VP8 across its whole cpu_used range (-16..16): is there a point where
/// the CPU cost collapses while quality stays acceptable? That would be
/// VP8's only remaining niche (ultra-low CPU); at quality settings it is
/// dominated by vp9-low-latency on both axes (2026-09-30 data).
#[test]
#[ignore = "benchmark: run explicitly in release"]
fn vp8_cpu_sweep_1080p() {
    let (w, h, n) = (1920u32, 1080u32, 90u32);
    let frames: Vec<Vec<u8>> = (0..n).map(|t| frame_at(w, h, t)).collect();
    println!("--- {n} frames {w}x{h}: VP8 cpu_used sweep @2.5M vs VP9 cpu 8 ---");
    // Reference point.
    sweep(
        "vp9-low-latency(ref)",
        codec::CodecPreset::by_name("vp9-low-latency").expect("preset"),
        &frames,
        w,
        h,
    );
    for cpu in [-14, -10, -6, 0, 4, 8, 12, 16] {
        let preset = codec::CodecPreset {
            name: "vp8-sweep",
            codec: codec::VideoCodecType::Vp8,
            params: codec::CodecParams {
                bitrate_kbps: 2500,
                cpu_used: Some(cpu),
                quality: None,
            },
        };
        sweep("vp8", preset, &frames, w, h);
    }
}
