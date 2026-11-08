// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com
//! mf-probe: lists the video-encoder MFTs registered in the system and,
//! with `--encode N`, runs N synthetic 1080p30 frames through the chosen
//! VP9 MFT measuring encode time (§18.75 H1). Read-only lab tool.

#[cfg(all(windows, feature = "mf-probe"))]
use codec::VideoEncoder;

fn main() {
    #[cfg(all(windows, feature = "mf-probe"))]
    {
        let encode_frames: u32 = std::env::args()
            .skip_while(|a| a != "--encode")
            .nth(1)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        match codec::mf::enumerate_encoders() {
            Ok(list) if list.is_empty() => println!("no video-encoder MFTs found"),
            Ok(list) => {
                for m in &list {
                    println!(
                        "{:5} async={:<5} d3d11={:<5} {}",
                        m.subtype, m.async_mft, m.d3d11_aware, m.name
                    );
                }
                println!("-- {} MFT(s)", list.len());
            }
            Err(e) => {
                eprintln!("enumeration failed: {e}");
                std::process::exit(1);
            }
        }
        if encode_frames > 0 {
            std::process::exit(encode_test(encode_frames));
        }
    }
    #[cfg(not(all(windows, feature = "mf-probe")))]
    {
        eprintln!("mf-probe needs Windows and the codec/codec-mf feature");
        std::process::exit(2);
    }
}

/// Encodes `frames` synthetic frames (gradient + moving square) and prints
/// timing/size stats. Exit code 0 on success.
#[cfg(all(windows, feature = "mf-probe"))]
fn encode_test(frames: u32) -> i32 {
    let (w, h, fps, kbps) = (1920u32, 1080u32, 30u32, 2_000u32);
    let mut enc = match codec::mf::MfEncoder::new_vp9(w, h, fps, kbps) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("MfEncoder init failed: {e}");
            return 1;
        }
    };
    println!("encoder ready: {}", enc.mft_name());
    let mut bgra = vec![0u8; (w * h * 4) as usize];
    let mut total_ms = 0f64;
    let mut max_ms = 0f64;
    let mut total_bytes = 0usize;
    let mut keyframes = 0u32;
    for i in 0..frames {
        // Gradient background + a white square moving left→right.
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
        let start = std::time::Instant::now();
        let packet = match enc.encode(&codec::VideoFrame {
            width: w,
            height: h,
            stride: (w * 4) as usize,
            data: &bgra,
        }) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("encode failed at frame {i}: {e}");
                return 1;
            }
        };
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        total_ms += ms;
        max_ms = max_ms.max(ms);
        total_bytes += packet.data.len();
        if packet.keyframe {
            keyframes += 1;
        }
    }
    println!(
        "encoded {} frames: avg {:.2} ms max {:.2} ms, {} bytes total ({} keyframes)",
        frames,
        total_ms / f64::from(frames),
        max_ms,
        total_bytes,
        keyframes
    );
    0
}
