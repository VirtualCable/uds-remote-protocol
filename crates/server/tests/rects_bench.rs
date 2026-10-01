//! Rects-vs-full-frame benchmark (design doc §8.5): with the real desktop
//! (DXGI, Windows) or the synthetic pattern elsewhere, encode each frame via
//! full-frame VP9 vs per-rect JPEG (libjpeg-turbo) / WebP (libwebp), measuring
//! encode ms/frame, real kbps at 30 fps and PSNR over the changed pixels.
//!
//! Run: `cargo test -p server --release --features windows-capture --test rects_bench -- --ignored --nocapture`
//! (drop the feature outside Windows; the synthetic source is used there).

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::chunks_exact_to_as_chunks,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::unnecessary_wraps,
    clippy::unused_enumerate_index
)]

use std::time::Instant;

use server::capture::Rect;
use server::synthetic::SyntheticSource;

const FRAMES: u32 = 150;
const SYNTH_W: u32 = 1600;
const SYNTH_H: u32 = 900;
const TILE: u32 = 64;

/// Frames from DXGI (Windows, real dirty rects) or the synthetic pattern
/// (elsewhere; dirty rects computed by 64 px tile diffing).
enum Src {
    Synthetic(SyntheticSource),
    #[cfg(all(windows, feature = "windows-capture"))]
    Dxgi(server::dxgi::DxgiSource),
}

struct Frame {
    width: u32,
    height: u32,
    stride: usize,
    data: Vec<u8>,
    dirty: Vec<Rect>,
}

impl Src {
    fn new() -> Self {
        #[cfg(all(windows, feature = "windows-capture"))]
        {
            Self::Dxgi(server::dxgi::DxgiSource::new(0).expect("dxgi"))
        }
        #[cfg(not(all(windows, feature = "windows-capture")))]
        {
            Self::Synthetic(SyntheticSource::new(SYNTH_W, SYNTH_H))
        }
    }

    fn next(&mut self, prev: Option<&[u8]>) -> Option<Frame> {
        match self {
            Self::Synthetic(s) => {
                let f = s.next_frame();
                let data = f.data.to_vec();
                let dirty = prev.map_or_else(
                    || {
                        vec![Rect {
                            left: 0,
                            top: 0,
                            right: f.width as i32,
                            bottom: f.height as i32,
                        }]
                    },
                    |p| tile_diff(p, &data, f.width, f.height, f.stride),
                );
                Some(Frame {
                    width: f.width,
                    height: f.height,
                    stride: f.stride,
                    data,
                    dirty,
                })
            }
            #[cfg(all(windows, feature = "windows-capture"))]
            Self::Dxgi(s) => {
                use server::capture::FrameSource as _;
                let f = s
                    .next_frame(std::time::Duration::from_millis(100))
                    .ok()
                    .flatten()?;
                if f.dirty_rects.is_empty() {
                    return None;
                }
                Some(Frame {
                    width: f.width,
                    height: f.height,
                    stride: f.stride,
                    data: f.data,
                    dirty: f.dirty_rects,
                })
            }
        }
    }
}

/// 64 px tile diff between two tight BGRA frames; merges contiguous dirty
/// tiles horizontally into strips.
fn tile_diff(prev: &[u8], cur: &[u8], width: u32, height: u32, stride: usize) -> Vec<Rect> {
    let (w, h) = (width as usize, height as usize);
    let tiles_x = w.div_ceil(TILE as usize);
    let tiles_y = h.div_ceil(TILE as usize);
    let mut dirty = vec![false; tiles_x * tiles_y];
    for ty in 0..tiles_y {
        let y0 = ty * TILE as usize;
        let rows = (TILE as usize).min(h - y0);
        for tx in 0..tiles_x {
            let x0 = tx * TILE as usize;
            let cols = (TILE as usize).min(w - x0) * 4;
            let is_dirty = (0..rows).any(|r| {
                let off = (y0 + r) * stride + x0 * 4;
                prev[off..off + cols] != cur[off..off + cols]
            });
            if is_dirty {
                dirty[ty * tiles_x + tx] = true;
            }
        }
    }
    let mut rects = Vec::new();
    for ty in 0..tiles_y {
        let mut tx = 0;
        while tx < tiles_x {
            if !dirty[ty * tiles_x + tx] {
                tx += 1;
                continue;
            }
            let mut end = tx;
            while end + 1 < tiles_x && dirty[ty * tiles_x + end + 1] {
                end += 1;
            }
            rects.push(Rect {
                left: (tx * TILE as usize) as i32,
                top: (ty * TILE as usize) as i32,
                right: (((end + 1) * TILE as usize).min(w)) as i32,
                bottom: (((ty + 1) * TILE as usize).min(h)) as i32,
            });
            tx = end + 1;
        }
    }
    rects
}

// --- Rect merging, ported from rdphtml5's RectSender::merge_rects ---------

fn area(r: &Rect) -> i64 {
    i64::from(r.right - r.left) * i64::from(r.bottom - r.top)
}

fn intersects(a: &Rect, b: &Rect) -> bool {
    a.left <= b.right && b.left <= a.right && a.top <= b.bottom && b.top <= a.bottom
}

fn contains(a: &Rect, b: &Rect) -> bool {
    b.left >= a.left && b.right <= a.right && b.top >= a.top && b.bottom <= a.bottom
}

fn union(a: &Rect, b: &Rect) -> Rect {
    Rect {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    }
}

fn bounding_all(rects: &[Rect]) -> Rect {
    rects.iter().skip(1).fold(rects[0], |acc, r| union(&acc, r))
}

/// Dedup + merge when intersection or union area < 1.15x the area sum;
/// degenerate cases collapse to the bounding box.
fn merge_rects(mut rects: Vec<Rect>) -> Vec<Rect> {
    rects.retain(|r| !r.is_empty());
    if rects.len() <= 1 {
        return rects;
    }
    if rects.len() > 500 {
        return vec![bounding_all(&rects)];
    }
    let mut i = 0;
    while i < rects.len() {
        if (0..rects.len()).any(|j| i != j && contains(&rects[j], &rects[i])) {
            rects.remove(i);
        } else {
            i += 1;
        }
    }
    let mut changed = true;
    while changed && rects.len() > 1 {
        changed = false;
        let mut i = 0;
        while i < rects.len() {
            let mut j = i + 1;
            while j < rects.len() {
                let merged = union(&rects[i], &rects[j]);
                if intersects(&rects[i], &rects[j])
                    || area(&merged) < (area(&rects[i]) + area(&rects[j])) * 115 / 100
                {
                    rects[i] = merged;
                    rects.remove(j);
                    changed = true;
                } else {
                    j += 1;
                }
            }
            i += 1;
        }
    }
    if rects.len() > 64 {
        return vec![bounding_all(&rects)];
    }
    rects
}

/// Clamps to the frame and crops a tight BGRA copy of `rect`.
fn crop(f: &Frame, rect: &Rect) -> Option<(Vec<u8>, u32, u32)> {
    let l = rect.left.clamp(0, f.width as i32) as usize;
    let t = rect.top.clamp(0, f.height as i32) as usize;
    let r = rect.right.clamp(0, f.width as i32) as usize;
    let b = rect.bottom.clamp(0, f.height as i32) as usize;
    let (w, h) = (r - l, b - t);
    if w == 0 || h == 0 {
        return None;
    }
    let mut out = vec![0u8; w * h * 4];
    for row in 0..h {
        let src = (t + row) * f.stride + l * 4;
        out[row * w * 4..(row + 1) * w * 4].copy_from_slice(&f.data[src..src + w * 4]);
    }
    Some((out, w as u32, h as u32))
}

// --- Per-path accumulators ------------------------------------------------

#[derive(Default)]
struct Acc {
    ms: f64,
    bytes: u64,
    se: f64,
    px: u64,
}

impl Acc {
    fn psnr(&self) -> f64 {
        if self.se == 0.0 {
            return f64::INFINITY;
        }
        let mse = self.se / (self.px.max(1) * 3) as f64;
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }

    fn report(&self, name: &str, frames: u64) {
        let ms_f = self.ms / frames as f64;
        let kbps = self.bytes as f64 * 8.0 * 30.0 / frames as f64 / 1000.0;
        println!(
            "{name:<14} | {ms_f:>6.2} ms/f | {kbps:>7.0} kbps@30 | {:>6.2} dB",
            self.psnr()
        );
    }
}

fn mse_acc(acc: &mut Acc, a: &[u8], b: &[u8]) {
    for (x, y) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        for ch in 0..3 {
            let d = f64::from(x[ch]) - f64::from(y[ch]);
            acc.se += d * d;
        }
        acc.px += 1;
    }
}

/// MSE between a 4-byte-per-pixel BGRA source and a 3-byte-per-pixel RGB
/// decoded image (WebP decodes to RGB, so R/B are swapped).
fn mse_acc_rgb(acc: &mut Acc, a: &[u8], b: &[u8]) {
    for (x, y) in a.chunks_exact(4).zip(b.chunks_exact(3)) {
        for ch in 0..3 {
            let d = f64::from(x[ch]) - f64::from(y[2 - ch]);
            acc.se += d * d;
        }
        acc.px += 1;
    }
}

fn jpeg_roundtrip(
    acc: &mut Acc,
    compressor: &mut turbojpeg::Compressor,
    src: &[u8],
    w: u32,
    h: u32,
) {
    let t = Instant::now();
    let image = turbojpeg::Image {
        pixels: src,
        width: w as usize,
        pitch: w as usize * 4,
        height: h as usize,
        format: turbojpeg::PixelFormat::BGRA,
    };
    let jpeg = compressor.compress_to_owned(image).expect("jpeg encode");
    acc.ms += t.elapsed().as_secs_f64() * 1000.0;
    acc.bytes += jpeg.len() as u64;
    let decoded = turbojpeg::decompress(&jpeg, turbojpeg::PixelFormat::BGRA)
        .expect("jpeg decode")
        .pixels;
    mse_acc(acc, src, &decoded);
}

fn webp_roundtrip(acc: &mut Acc, bgra: &[u8], w: u32, h: u32, quality: f32) {
    // webp crate encodes RGB/RGBA; our rects are opaque BGRA -> RGB
    // (swapping R/B; also 25 % less input data).
    let mut rgb = vec![0u8; w as usize * h as usize * 3];
    for (d, s) in rgb.chunks_exact_mut(3).zip(bgra.chunks_exact(4)) {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
    }
    let t = Instant::now();
    let encoder = webp::Encoder::from_rgb(&rgb, w, h);
    let mut config = webp::WebPConfig::new().expect("webp config");
    config.method = 0; // fastest (rdphtml5 uses 0/1 for realtime)
    config.thread_level = i32::from(w * h >= 40_000);
    config.lossless = 0;
    config.quality = quality;
    let mem = encoder.encode_advanced(&config).expect("webp encode");
    acc.ms += t.elapsed().as_secs_f64() * 1000.0;
    acc.bytes += mem.len() as u64;
    let decoded = webp::Decoder::new(&mem)
        .decode()
        .expect("webp decode")
        .to_image()
        .to_rgb8();
    mse_acc_rgb(acc, bgra, decoded.as_raw());
}

#[test]
#[ignore = "benchmark: run explicitly in release"]
fn rects_vs_fullframe() {
    let mut src = Src::new();
    // VP9 encoder/decoder are built lazily at the first frame's real
    // geometry (DXGI reports the actual output size).
    let mut vp9: Option<Box<dyn codec::VideoEncoder>> = None;
    let mut vp9_dec: Option<Box<dyn codec::VideoDecoder>> = None;

    let mut jpeg80c = turbojpeg::Compressor::new().expect("jpeg");
    jpeg80c.set_quality(80).expect("q");
    let mut jpeg50c = turbojpeg::Compressor::new().expect("jpeg");
    jpeg50c.set_quality(50).expect("q");

    let mut acc_vp9 = Acc::default();
    let mut acc_j80 = Acc::default();
    let mut acc_j50 = Acc::default();
    let mut acc_w75 = Acc::default();
    let mut acc_w40 = Acc::default();
    let mut merge_ms = 0.0f64;
    let mut rect_total = 0u64;
    let mut area_total = 0u64;
    let mut dims = (0u32, 0u32);

    let mut prev: Option<Vec<u8>> = None;
    let mut n = 0u64;
    while n < u64::from(FRAMES) {
        let Some(frame) = src.next(prev.as_deref()) else {
            continue;
        };
        dims = (frame.width, frame.height);
        if vp9.is_none() {
            vp9 = Some(
                codec::CodecPreset::by_name("vp9-low-latency")
                    .expect("preset")
                    .build_encoder(frame.width, frame.height)
                    .expect("vp9 encoder"),
            );
            vp9_dec = Some(codec::build_decoder(codec::VideoCodecType::Vp9).expect("vp9 decoder"));
        }
        let t = Instant::now();
        let rects = merge_rects(frame.dirty.clone());
        merge_ms += t.elapsed().as_secs_f64() * 1000.0;
        rect_total += rects.len() as u64;
        let frame_area = u64::from(frame.width) * u64::from(frame.height);
        area_total += rects.iter().map(|r| area(r) as u64).sum::<u64>().min(frame_area);

        // VP9: full frame, PSNR measured over the changed rects.
        let vf = codec::VideoFrame {
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            data: &frame.data,
        };
        let t = Instant::now();
        let pkt = vp9.as_mut().expect("inited").encode(&vf).expect("vp9 encode");
        acc_vp9.ms += t.elapsed().as_secs_f64() * 1000.0;
        acc_vp9.bytes += pkt.data.len() as u64;
        let decoded = vp9_dec
            .as_mut()
            .expect("inited")
            .decode(&pkt)
            .expect("vp9 decode");
        for r in &rects {
            if let Some((src_rect, w, h)) = crop(&frame, r) {
                let row_bytes = w as usize * 4;
                let mut dec_rect = vec![0u8; src_rect.len()];
                let left = r.left.max(0) as usize;
                let top = r.top.max(0) as usize;
                for row in 0..h as usize {
                    let off = (top + row) * decoded.stride + left * 4;
                    dec_rect[row * row_bytes..(row + 1) * row_bytes]
                        .copy_from_slice(&decoded.data[off..off + row_bytes]);
                }
                mse_acc(&mut acc_vp9, &src_rect, &dec_rect);
            }
        }

        // Rect codecs: per merged rect.
        for r in &rects {
            let Some((rgb, w, h)) = crop(&frame, r) else {
                continue;
            };
            jpeg_roundtrip(&mut acc_j80, &mut jpeg80c, &rgb, w, h);
            jpeg_roundtrip(&mut acc_j50, &mut jpeg50c, &rgb, w, h);
            webp_roundtrip(&mut acc_w75, &rgb, w, h, 75.0);
            webp_roundtrip(&mut acc_w40, &rgb, w, h, 40.0);
        }

        prev = Some(frame.data);
        n += 1;
    }

    let (w, h) = dims;
    println!("--- {n} frames {w}x{h} (rects merged from dirty) ---");
    println!(
        "rects/frame {:.1} | changed area {:.1}% | merge {:.3} ms/f",
        rect_total as f64 / n as f64,
        area_total as f64 * 100.0 / (n * u64::from(w) * u64::from(h)) as f64,
        merge_ms / n as f64,
    );
    acc_vp9.report("vp9-low-latency", n);
    acc_j80.report("jpeg-q80", n);
    acc_j50.report("jpeg-q50", n);
    acc_w75.report("webp-q75-m0", n);
    acc_w40.report("webp-q40-m0", n);
}
