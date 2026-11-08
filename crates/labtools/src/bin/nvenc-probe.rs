// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com
//! nvenc-probe: encodes N synthetic 1080p30 frames through the NVENC H.264
//! encoder (CPU NV12 feed) and writes the raw bitstream to a file so ffmpeg
//! can validate it decodes (§18.75.9 H3.5). Read-only lab tool.
//!
//! Usage: `nvenc-probe [--frames N] [--out file.h264]`
//! Env: `NVENC_KBPS` (default 2000).

#[cfg(all(windows, feature = "nvenc-probe"))]
use codec::VideoEncoder;

fn main() {
    #[cfg(all(windows, feature = "nvenc-probe"))]
    {
        let args: Vec<String> = std::env::args().collect();
        let frames: u32 = args
            .iter()
            .skip_while(|a| *a != "--frames")
            .nth(1)
            .and_then(|v| v.parse().ok())
            .unwrap_or(90);
        let out = args
            .iter()
            .skip_while(|a| *a != "--out")
            .nth(1)
            .cloned()
            .unwrap_or_else(|| "nvenc-probe.h264".to_string());
        std::process::exit(run(frames, &out));
    }
    #[cfg(not(all(windows, feature = "nvenc-probe")))]
    {
        eprintln!("nvenc-probe needs Windows and the labtools/nvenc-probe feature");
        std::process::exit(2);
    }
}

#[cfg(all(windows, feature = "nvenc-probe"))]
fn run(frames: u32, out: &str) -> i32 {
    let (w, h, fps) = (1920u32, 1080u32, 30u32);
    let kbps: u32 = std::env::var("NVENC_KBPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);

    if !codec::nvenc_available() {
        eprintln!("nvEncodeAPI64.dll not loadable on this machine");
        return 1;
    }
    let mut enc = match codec::NvencEncoder::new(w, h, fps, kbps) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("NvencEncoder init failed: {e}");
            return 1;
        }
    };
    println!("NVENC encoder ready: {w}x{h} @{fps}fps, {kbps} kbps");

    let mut bgra = vec![0u8; (w * h * 4) as usize];
    let mut stream = Vec::new();
    let mut keyframes = 0u32;
    let t0 = std::time::Instant::now();
    for i in 0..frames {
        fill_pattern(&mut bgra, w, h, i);
        if i == frames / 2 {
            enc.request_keyframe();
        }
        if i == frames / 3 {
            // Exercise the hot reconfigure path (CBR retarget + IDR).
            if let Err(e) = enc.set_bitrate(kbps / 2) {
                eprintln!("set_bitrate failed: {e}");
                return 1;
            }
            println!("frame {i}: bitrate retargeted to {} kbps", kbps / 2);
        }
        let frame = codec::VideoFrame {
            width: w,
            height: h,
            stride: (w * 4) as usize,
            data: &bgra,
        };
        match enc.encode(&frame) {
            Ok(packet) => {
                if packet.keyframe {
                    keyframes += 1;
                }
                if i < 3 || packet.keyframe {
                    println!(
                        "frame {i}: {} bytes{}",
                        packet.data.len(),
                        if packet.keyframe { " KEY" } else { "" }
                    );
                }
                stream.extend_from_slice(&packet.data);
            }
            Err(e) => {
                eprintln!("encode failed at frame {i}: {e}");
                return 1;
            }
        }
    }
    let elapsed = t0.elapsed();
    if let Err(e) = std::fs::write(out, &stream) {
        eprintln!("writing {out}: {e}");
        return 1;
    }
    println!(
        "-- {frames} frames in {:.2?} ({:.1} fps), {} bytes total, {keyframes} keyframes -> {out}",
        elapsed,
        f64::from(frames) / elapsed.as_secs_f64(),
        stream.len()
    );
    0
}

/// Gradient + moving square, same pattern as mf-probe.
#[cfg(all(windows, feature = "nvenc-probe"))]
#[allow(clippy::cast_possible_truncation, clippy::many_single_char_names)]
fn fill_pattern(bgra: &mut [u8], w: u32, h: u32, i: u32) {
    for (n, px) in bgra.chunks_exact_mut(4).enumerate() {
        let (x, y) = ((n as u32) % w, (n as u32) / w);
        px[0] = (x * 255 / w) as u8; // B
        px[1] = (y * 255 / h) as u8; // G
        px[2] = ((x + y) * 255 / (w + h)) as u8; // R
        px[3] = 255;
    }
    let sq = 200u32;
    let sx = (i * 40) % (w - sq);
    for y in 200..200 + sq {
        for x in sx..sx + sq {
            let o = ((y * w + x) * 4) as usize;
            bgra[o..o + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
    }
}
