// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Ladder encoder: drives the [`Ladder`] state machine with real codecs
//! (feature `codec-ladder`, design doc §8.5).
//!
//! Wraps a [`JpegRectsEncoder`] (`Level::Low`, base regime) and a
//! [`Vp9Encoder`] (`Level::High`, video regime). Every frame is diffed once
//! (shared by the JPEG encoder via [`JpegRectsEncoder::encode_with_dirty`]
//! and by the changed-area metric), the metric feeds the ladder, and on a
//! switch the new codec's first frame goes as a keyframe and
//! [`VideoEncoder::take_codec_change`] reports the new codec id so the
//! server can re-announce `DisplayInfo` before the fragments.
//!
//! Congestion control can force a level with [`LadderEncoder::force_level`]
//! (bypasses the ladder's damping, does not feed its backoff).

// Frame-geometry conversions and the area percentage are provably in range.
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation
)]

use std::time::Instant;

use crate::jpeg_rects::JpegRectsEncoder;
use crate::ladder::{Ladder, LadderPolicy, Level};
use crate::rects;
use crate::vpx::Vp9Encoder;
use crate::{CodecError, EncodedPacket, Result, VideoCodecType, VideoEncoder, VideoFrame};

/// Composite encoder switching between rects-JPEG and VP9 (§8.5).
pub struct LadderEncoder {
    width: u32,
    height: u32,
    ladder: Ladder,
    jpeg: JpegRectsEncoder,
    vp9: Vp9Encoder,
    /// Previous frame, kept for the changed-area metric (also fed to the
    /// JPEG encoder through `encode_with_dirty`).
    prev: Option<Vec<u8>>,
    pending_change: Option<VideoCodecType>,
    /// Force a full frame on the next encode (keyframe request or switch).
    force_full: bool,
    /// VP9 target bitrate, reference for the bandwidth up-trigger.
    vp9_kbps: u32,
    /// EMA of the JPEG egress bit rate while in `Level::Low` (bits/s).
    jpeg_bps_ema: Option<f64>,
    /// Timestamp of the previous Low-level encode (for the bit-rate EMA).
    last_low_encode: Option<Instant>,
}

/// JPEG egress above `BW_UP_RATIO` × the VP9 target maps to the ladder's up
/// threshold (2× is the point where the video regime is plainly cheaper).
/// The ratio is fixed for now; the area policy knobs stay configurable.
const BW_UP_RATIO: f64 = 2.0;

/// Smoothing for the JPEG bit-rate EMA. Reacts faster than the area EMA
/// (0.05): the sample is already a rate over a frame interval, and traffic
/// bursts are what this trigger exists to catch.
const BW_EMA_ALPHA: f64 = 0.2;

/// Maps the JPEG bit-rate EMA to an area-equivalent percentage for the
/// ladder: `BW_UP_RATIO` × `vp9_kbps` → `up_threshold_pct`, linear.
fn bw_pressure_pct(jpeg_bps_ema: Option<f64>, vp9_kbps: u32, up_threshold_pct: f64) -> f64 {
    let Some(bps) = jpeg_bps_ema else { return 0.0 };
    let vp9_rate_bps = f64::from(vp9_kbps) * 1000.0;
    if vp9_rate_bps <= 0.0 {
        return 0.0;
    }
    bps / (vp9_rate_bps * BW_UP_RATIO) * up_threshold_pct
}

impl LadderEncoder {
    /// Creates the composite encoder: JPEG at `jpeg_quality`, VP9 targeting
    /// `vp9_kbps`, decisions driven by `policy`. The VP9 leg runs at
    /// `cpu_used` 8 with 4 row-MT threads (the ladder is the low-latency
    /// path; see the §18.15 CPU sweep).
    pub fn new(
        width: u32,
        height: u32,
        policy: LadderPolicy,
        vp9_kbps: u32,
        jpeg_quality: i32,
    ) -> Result<Self> {
        Self::with_tuning(width, height, policy, vp9_kbps, jpeg_quality, 8, 4)
    }

    /// Full tuning knob set: explicit VP9 `cpu_used` and row-MT thread count
    /// (clamp to what the host can spare; 2 threads on small VMs).
    pub fn with_tuning(
        width: u32,
        height: u32,
        policy: LadderPolicy,
        vp9_kbps: u32,
        jpeg_quality: i32,
        vp9_cpu_used: i32,
        threads: u32,
    ) -> Result<Self> {
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
            ladder: Ladder::new(policy, Instant::now()),
            jpeg: JpegRectsEncoder::new(width, height, jpeg_quality)?,
            vp9: Vp9Encoder::with_tuning(width, height, vp9_kbps, Some(vp9_cpu_used), Some(threads))?,
            prev: None,
            pending_change: None,
            force_full: false,
            vp9_kbps,
            jpeg_bps_ema: None,
            last_low_encode: None,
        })
    }

    /// Ladder tuning in use (live reference, e.g. to inspect the EMA).
    #[must_use]
    pub fn ladder(&self) -> &Ladder {
        &self.ladder
    }

    /// Forces a level from outside (congestion control). Bypasses the
    /// ladder's damping and does not feed its backoff; no-op when already
    /// there. The next frame goes as a keyframe of the new codec.
    pub fn force_level(&mut self, level: Level) {
        if self.ladder.force(Instant::now(), level).is_some() {
            self.begin_level(level);
        }
    }

    fn check_dims(&self, frame: &VideoFrame<'_>) -> Result<()> {
        if frame.width != self.width || frame.height != self.height {
            return Err(CodecError::BadDimensions {
                width: frame.width,
                height: frame.height,
                stride: frame.stride,
                data_len: frame.data.len(),
            });
        }
        Ok(())
    }

    /// Marks the pending switch: keyframe + codec-change notification.
    fn begin_level(&mut self, level: Level) {
        self.force_full = true;
        let codec = match level {
            Level::Low => VideoCodecType::JpegRects,
            Level::High => {
                // The peer rebuilds its decoder on the codec switch, so the
                // first VP9 frame of every visit must be a keyframe — the
                // encoder's initial force_key only covers the first one.
                self.vp9.request_keyframe();
                VideoCodecType::Vp9
            }
        };
        self.pending_change = Some(codec);
    }
}

impl VideoEncoder for LadderEncoder {
    fn encode(&mut self, frame: &VideoFrame<'_>) -> Result<EncodedPacket> {
        self.check_dims(frame)?;

        // One diff per frame, shared by the metric and the JPEG encoder.
        let mut dirty = if self.force_full {
            self.force_full = false;
            vec![rects::Rect {
                left: 0,
                top: 0,
                right: self.width as i32,
                bottom: self.height as i32,
            }]
        } else {
            rects::dirty_rects(self.prev.as_deref(), frame.data, self.width, self.height, frame.stride)
        };
        let area_pct = 100.0 * dirty.iter().map(rects::Rect::area).sum::<i64>() as f64
            / (f64::from(self.width) * f64::from(self.height));

        // The ladder sees the worst of two pressures: changed area and JPEG
        // bandwidth. A small window playing video changes little area but
        // costs far more than the VP9 regime — the bandwidth term (only
        // meaningful in Low, where egress is area-proportional) covers it.
        // In High the down path stays area-only (hysteresis band).
        let sample = if self.ladder.level() == Level::Low {
            area_pct.max(bw_pressure_pct(
                self.jpeg_bps_ema,
                self.vp9_kbps,
                self.ladder.policy().up_threshold_pct,
            ))
        } else {
            area_pct
        };

        if let Some(switch) = self.ladder.update(Instant::now(), sample) {
            self.begin_level(switch.to);
            // The just-computed diff predates the switch; the new codec's
            // first frame is a full frame (keyframe).
            self.force_full = false;
            dirty = vec![rects::Rect {
                left: 0,
                top: 0,
                right: self.width as i32,
                bottom: self.height as i32,
            }];
        }

        let packet = match self.ladder.level() {
            Level::Low => self.jpeg.encode_with_dirty(frame, &dirty)?,
            // VP9 always gets the whole frame, even when nothing changed:
            // its rate control and reference chain must stay alive.
            Level::High => self.vp9.encode(frame)?,
        };
        if self.ladder.level() == Level::Low {
            let now = Instant::now();
            if let Some(prev_t) = self.last_low_encode.replace(now) {
                let dt = now.duration_since(prev_t).as_secs_f64();
                if dt > 0.0 {
                    let bps = packet.data.len() as f64 * 8.0 / dt;
                    self.jpeg_bps_ema = Some(match self.jpeg_bps_ema {
                        None => bps,
                        Some(ema) => ema + BW_EMA_ALPHA * (bps - ema),
                    });
                }
            }
        }
        self.prev = Some(frame.data.to_vec());
        Ok(packet)
    }

    fn request_keyframe(&mut self) {
        self.force_full = true;
        // force_full only feeds the JPEG dirty rect; in the video regime a
        // recovery needs a real VP9 keyframe (§12.2).
        self.vp9.request_keyframe();
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        // Forward to both: whichever is active applies it now, the other
        // picks it up when it becomes active.
        self.jpeg.set_bitrate(kbps)?;
        self.vp9.set_bitrate(kbps)
    }

    fn codec_type(&self) -> VideoCodecType {
        match self.ladder.level() {
            Level::Low => VideoCodecType::JpegRects,
            Level::High => VideoCodecType::Vp9,
        }
    }

    fn take_codec_change(&mut self) -> Option<VideoCodecType> {
        self.pending_change.take()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::many_single_char_names)] // (w, h) and frame bytes read better
    #![allow(clippy::chunks_exact_to_as_chunks, clippy::elidable_lifetime_names)]
    use super::*;
    use crate::{VideoDecoder, vpx::Vp9Decoder};
    use std::time::Duration;

    #[test]
    fn bw_pressure_maps_ratio_to_threshold() {
        // No estimate yet, or a degenerate VP9 target: no pressure.
        assert_eq!(bw_pressure_pct(None, 2000, 13.0), 0.0);
        assert_eq!(bw_pressure_pct(Some(1e6), 0, 13.0), 0.0);
        // Exactly BW_UP_RATIO × VP9 target → exactly the up threshold.
        let at_ratio = 2.0 * 2_000_000.0;
        let pct = bw_pressure_pct(Some(at_ratio), 2000, 13.0);
        assert!((pct - 13.0).abs() < 1e-9);
        // Linear: half the ratio → half the threshold.
        let half = bw_pressure_pct(Some(at_ratio / 2.0), 2000, 13.0);
        assert!((half - 6.5).abs() < 1e-9);
    }

    fn policy() -> LadderPolicy {
        LadderPolicy {
            up_threshold_pct: 12.0,
            down_threshold_pct: 7.0,
            hold_up: Duration::from_millis(20),
            hold_down: Duration::from_millis(40),
            min_dwell: Duration::from_millis(30),
            backoff: 1.5,
            dwell_max: Duration::from_millis(200),
            ema_alpha: 0.5,
        }
    }

    fn solid(w: u32, h: u32, px: [u8; 4]) -> Vec<u8> {
        let mut v = vec![0u8; w as usize * h as usize * 4];
        for chunk in v.chunks_exact_mut(4) {
            chunk.copy_from_slice(&px);
        }
        v
    }

    /// Fills the lower half with a colour (50 % of the area changes).
    fn half_changed(w: u32, h: u32, top: [u8; 4], bottom: [u8; 4]) -> Vec<u8> {
        let mut v = solid(w, h, top);
        for row in (h / 2)..h {
            for x in 0..w {
                v[(row * w + x) as usize * 4..][..4].copy_from_slice(&bottom);
            }
        }
        v
    }

    fn vf<'a>(w: u32, h: u32, data: &'a [u8]) -> VideoFrame<'a> {
        VideoFrame { width: w, height: h, stride: w as usize * 4, data }
    }

    /// Feeds frames until the codec becomes `want` or the tries run out.
    fn drive_until(
        enc: &mut LadderEncoder,
        w: u32,
        h: u32,
        frame: &[u8],
        want: VideoCodecType,
        tries: u32,
    ) -> bool {
        for _ in 0..tries {
            enc.encode(&vf(w, h, frame)).expect("encode");
            if enc.codec_type() == want {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn quiet_desktop_stays_jpeg_and_silent() {
        let (w, h) = (128u32, 64u32);
        let mut enc = LadderEncoder::new(w, h, policy(), 1000, 80).unwrap();
        let f = solid(w, h, [10, 20, 30, 255]);
        let p1 = enc.encode(&vf(w, h, &f)).unwrap();
        assert!(!p1.data.is_empty(), "first frame is a full rect");
        let p2 = enc.encode(&vf(w, h, &f)).unwrap();
        assert!(p2.data.is_empty(), "no changes -> empty packet");
        assert_eq!(enc.codec_type(), VideoCodecType::JpegRects);
        assert_eq!(enc.take_codec_change(), None);
    }

    #[test]
    fn big_sustained_change_switches_to_vp9_with_keyframe() {
        let (w, h) = (128u32, 64u32);
        let mut enc = LadderEncoder::new(w, h, policy(), 1000, 80).unwrap();
        let quiet = solid(w, h, [10, 20, 30, 255]);
        enc.encode(&vf(w, h, &quiet)).unwrap();

        let mut toggling = true;
        let mut vp9_keyframe_seen = false;
        assert!(drive_until_state(&mut enc, w, h, &mut toggling, &mut vp9_keyframe_seen));
        assert_eq!(enc.codec_type(), VideoCodecType::Vp9);
        assert!(vp9_keyframe_seen, "first VP9 frame after the switch is a keyframe");
        // The change was reported exactly once.
        assert_eq!(enc.take_codec_change(), None);
    }

    fn drive_until_state(
        enc: &mut LadderEncoder,
        w: u32,
        h: u32,
        toggling: &mut bool,
        vp9_keyframe_seen: &mut bool,
    ) -> bool {
        for _ in 0..200 {
            let f = if *toggling {
                half_changed(w, h, [10, 20, 30, 255], [200, 100, 50, 255])
            } else {
                half_changed(w, h, [10, 20, 30, 255], [50, 100, 200, 255])
            };
            *toggling = !*toggling;
            let p = enc.encode(&vf(w, h, &f)).expect("encode");
            if enc.codec_type() == VideoCodecType::Vp9 {
                *vp9_keyframe_seen |= p.keyframe;
                if let Some(change) = enc.take_codec_change() {
                    assert_eq!(change, VideoCodecType::Vp9);
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn return_to_quiet_switches_back_to_jpeg() {
        let (w, h) = (128u32, 64u32);
        let mut enc = LadderEncoder::new(w, h, policy(), 1000, 80).unwrap();
        let quiet = solid(w, h, [10, 20, 30, 255]);
        enc.encode(&vf(w, h, &quiet)).unwrap();
        // Alternate two "video" frames: a static frame only changes once.
        let vid_a = half_changed(w, h, [1, 2, 3, 255], [4, 5, 6, 255]);
        let vid_b = half_changed(w, h, [1, 2, 3, 255], [6, 5, 4, 255]);
        let mut vp9 = false;
        for i in 0..300 {
            let f = if i % 2 == 0 { &vid_a } else { &vid_b };
            enc.encode(&vf(w, h, f)).unwrap();
            if enc.codec_type() == VideoCodecType::Vp9 {
                vp9 = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(vp9, "sustained 50 % change must reach VP9");
        while enc.take_codec_change().is_some() {}

        assert!(drive_until(&mut enc, w, h, &quiet, VideoCodecType::JpegRects, 600));
        // The back-switch announced JPEG and the frame is a full rect.
        assert_eq!(enc.take_codec_change(), Some(VideoCodecType::JpegRects));
    }

    #[test]
    fn force_level_bypasses_dwell_and_marks_keyframe() {
        let (w, h) = (128u32, 64u32);
        let mut enc = LadderEncoder::new(w, h, policy(), 1000, 80).unwrap();
        let f = solid(w, h, [10, 20, 30, 255]);
        enc.encode(&vf(w, h, &f)).unwrap();

        enc.force_level(Level::High);
        assert_eq!(enc.codec_type(), VideoCodecType::Vp9);
        assert_eq!(enc.take_codec_change(), Some(VideoCodecType::Vp9));
        let p = enc.encode(&vf(w, h, &f)).unwrap();
        assert!(p.keyframe, "first frame after force is a VP9 keyframe");

        // A VP9 decoder accepts the stream from the forced switch.
        let mut dec = Vp9Decoder::new().unwrap();
        let d = dec.decode(&p).unwrap();
        assert_eq!((d.width, d.height), (w, h));
    }

    #[test]
    fn vp9_regime_always_recovers_with_keyframes() {
        // Regression: a lost frame in the video regime could never heal
        // because keyframe requests (and re-entries into High) only forced
        // a full JPEG dirty rect, never a VP9 keyframe — the encoder's
        // initial force_key only covered the very first VP9 frame ever.
        let (w, h) = (128u32, 64u32);
        let mut enc = LadderEncoder::new(w, h, policy(), 1000, 80).unwrap();
        let f = solid(w, h, [10, 20, 30, 255]);
        enc.encode(&vf(w, h, &f)).unwrap();

        // First visit to High: the encoder's initial frame is a keyframe.
        enc.force_level(Level::High);
        let p = enc.encode(&vf(w, h, &f)).unwrap();
        assert!(p.keyframe, "first VP9 frame must be a keyframe");
        let p = enc.encode(&vf(w, h, &f)).unwrap();
        assert!(!p.keyframe, "second frame is a delta");

        // A keyframe request in the video regime must reach the VP9 encoder.
        enc.request_keyframe();
        let p = enc.encode(&vf(w, h, &f)).unwrap();
        assert!(p.keyframe, "keyframe request in VP9 regime must emit a keyframe");

        // Re-entering High later must also start with a keyframe: the peer
        // rebuilds its decoder on the codec switch and a delta would be
        // undecodable ("Unspecified internal error", stuck until fallback).
        enc.force_level(Level::Low);
        enc.encode(&vf(w, h, &f)).unwrap();
        enc.force_level(Level::High);
        let p = enc.encode(&vf(w, h, &f)).unwrap();
        assert!(p.keyframe, "re-entering VP9 must emit a keyframe");
        let mut dec = Vp9Decoder::new().unwrap();
        dec.decode(&p).expect("a fresh decoder accepts the re-entry keyframe");
    }

    #[test]
    fn set_bitrate_reaches_both_codecs() {
        let (w, h) = (128u32, 64u32);
        let mut enc = LadderEncoder::new(w, h, policy(), 1000, 80).unwrap();
        enc.set_bitrate(300).unwrap(); // must not error on either codec
        let f = solid(w, h, [10, 20, 30, 255]);
        enc.encode(&vf(w, h, &f)).unwrap();
    }
}
