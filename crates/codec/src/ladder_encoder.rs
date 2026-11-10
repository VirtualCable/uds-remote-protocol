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

use std::collections::VecDeque;
use std::time::{Duration, Instant};

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
    vp9: Vp9Backend,
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
    /// EMA of the JPEG cost per dirty pixel (bytes/px) measured on past
    /// Low-level delta frames — the cost model of the down path (§18.75.19).
    jpeg_bpp_ema: Option<f64>,
    /// EMA of the VP9 packet size (bytes) while in `Level::High`.
    vp9_bytes_ema: Option<f64>,
    /// Rate samples taken since entering `Level::Low` (burst-guard warm-up).
    low_rate_samples: u32,
    /// Consecutive samples currently over both burst gates (rate + area).
    burst_hot_run: u32,
    /// Recent samples within [`DOWN_MIN_WINDOW`] (down-path spike filter).
    recent_samples: VecDeque<(Instant, f64)>,
    /// Throttle for the dirty-region diagnostic line (lab diagnostics).
    last_dirty_diag: Option<Instant>,
}

/// Video-regime backend of the ladder (§18.75): libvpx VP9 software always,
/// Media Foundation VP9 (hardware when the driver provides it) on Windows
/// behind `codec-mf`, or NVENC H.264 on Windows behind `codec-nvenc` (only
/// when the client decodes H.264). The libvpx-only knob `cpu_used` is a
/// no-op on the hardware backends — they have no equivalent and the content
/// axis simply does not steer them.
enum Vp9Backend {
    Software(Vp9Encoder),
    #[cfg(all(windows, feature = "codec-mf"))]
    Mf(crate::mf::MfEncoder),
    /// NVENC H.264 (Windows + `codec-nvenc`): the video regime switches
    /// codec from VP9 to H.264 — only selected when the client advertised
    /// H.264 decode (§18.75.9).
    #[cfg(all(windows, feature = "codec-nvenc"))]
    Nvenc(crate::nvenc::NvencEncoder),
}

impl Vp9Backend {
    fn set_cpu_used(&mut self, cpu_used: i32) -> Result<()> {
        match self {
            Self::Software(e) => e.set_cpu_used(cpu_used),
            #[cfg(all(windows, feature = "codec-mf"))]
            Self::Mf(_) => Ok(()),
            #[cfg(all(windows, feature = "codec-nvenc"))]
            Self::Nvenc(_) => Ok(()),
        }
    }
}

impl VideoEncoder for Vp9Backend {
    fn encode(&mut self, frame: &VideoFrame<'_>) -> Result<EncodedPacket> {
        match self {
            Self::Software(e) => e.encode(frame),
            #[cfg(all(windows, feature = "codec-mf"))]
            Self::Mf(e) => e.encode(frame),
            #[cfg(all(windows, feature = "codec-nvenc"))]
            Self::Nvenc(e) => e.encode(frame),
        }
    }

    fn request_keyframe(&mut self) {
        match self {
            Self::Software(e) => e.request_keyframe(),
            #[cfg(all(windows, feature = "codec-mf"))]
            Self::Mf(e) => e.request_keyframe(),
            #[cfg(all(windows, feature = "codec-nvenc"))]
            Self::Nvenc(e) => e.request_keyframe(),
        }
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        match self {
            Self::Software(e) => e.set_bitrate(kbps),
            #[cfg(all(windows, feature = "codec-mf"))]
            Self::Mf(e) => e.set_bitrate(kbps),
            #[cfg(all(windows, feature = "codec-nvenc"))]
            Self::Nvenc(e) => e.set_bitrate(kbps),
        }
    }

    fn codec_type(&self) -> VideoCodecType {
        match self {
            #[cfg(all(windows, feature = "codec-nvenc"))]
            Self::Nvenc(_) => VideoCodecType::H264,
            _ => VideoCodecType::Vp9,
        }
    }
}

/// JPEG egress above `BW_UP_RATIO` × the VP9 target maps to the ladder's up
/// threshold (2× is the point where the video regime is plainly cheaper).
/// The ratio is fixed for now; the area policy knobs stay configurable.
const BW_UP_RATIO: f64 = 2.0;

/// Smoothing for the JPEG bit-rate EMA. Reacts faster than the area EMA
/// (0.05): the sample is already a rate over a frame interval, and traffic
/// bursts are what this trigger exists to catch.
const BW_EMA_ALPHA: f64 = 0.2;

/// Down-path cost veto (§18.75.19): the area metric is thresholded and
/// reads low on slow video, but the JPEG leg's cost follows the *exact*
/// byte diff (decode shimmer dirties nearly every tile), so on video the
/// Low regime is ~20× dearer than VP9 even at tiny "meaningful" change.
/// While in High, a switch down is vetoed unless the estimated JPEG frame
/// cost (measured bytes/dirty-pixel × this frame's exact dirty area) is
/// strictly below the measured VP9 frame cost times this margin.
const DOWN_COST_MARGIN: f64 = 1.0;

/// Burst guard (§18.75.19, §18.75.24): in Low, a SUSTAINED run of frames
/// whose instantaneous rate exceeds `BURST_MULT` × the up-trigger rate
/// (scene cut flooding the paced queue with 1080p JPEGs) switches back to
/// High immediately, no hold.
///
/// The rate alone cannot tell a scene cut from ordinary interactivity: at
/// high capture fps ANY content-bearing frame tops the rate (a menu with
/// its drop shadow is ~40-80 KB, i.e. >6 Mbps at 50 ms intervals), so the
/// guard is gated on the dirty area — a flood requires a big change —
/// on a warm-up of rate samples after entering Low (the first frames
/// after connect or a codec switch are full-screen by definition and
/// would trip it instantly, bypassing every damping layer via `force`),
/// and on PERSISTENCE: a menu-open animation dirties a third of the
/// screen for ~300-400 ms at >13 Mbps and the pacer absorbs it; only a
/// flood that keeps flooding (~8 hot frames, ~400 ms at 20 fps) is worth
/// short-cutting the bandwidth EMA's regular 2 s hold for.
const BURST_MULT: f64 = 1.5;

/// Minimum rate samples after entering `Level::Low` before the burst guard
/// may fire (connect/switch calibration window).
const BURST_WARMUP_SAMPLES: u32 = 3;

/// Consecutive hot samples (rate AND area over their gates) required to
/// fire the burst guard. Transient bursts (menu/tooltip animations) end
/// before this; scene cuts and video do not.
const BURST_PERSIST_SAMPLES: u32 = 8;

/// Area gate of the burst guard: only a change covering at least a third
/// of the screen can be a scene-cut flood. Menus, tooltips and window
/// drags are the JPEG leg's normal job and must never trip it.
const BURST_AREA_FRAC: f64 = 1.0 / 3.0;

/// EMA smoothing for the cost models (JPEG bytes/px, VP9 bytes/frame).
const COST_EMA_ALPHA: f64 = 0.2;

/// Down-path spike filter (§18.75.25): a periodically bursty source (a
/// log console scrolling at 1 Hz, a clock, a blinking caret) spikes the
/// area sample once a second, and the continuous `hold_down` then never
/// completes — the regime sticks in High while VP9 spends ~10× what the
/// JPEG leg would. While in High the ladder is fed the MINIMUM sample of
/// the last second instead of the raw one: periodic bursts have quiet
/// frames inside every window (min ≈ 0 → the regime can come down);
/// sustained video has none (min stays hot → it stays up). The up path
/// keeps the raw sample — going up is urgent.
const DOWN_MIN_WINDOW: Duration = Duration::from_secs(1);

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

/// Total area (pixels) covered by a dirty-rect list.
fn dirty_area_px(dirty: &[rects::Rect]) -> u64 {
    dirty.iter().map(|r| r.area().cast_unsigned()).sum()
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
            vp9: Vp9Backend::Software(Vp9Encoder::with_tuning(
                width,
                height,
                vp9_kbps,
                Some(vp9_cpu_used),
                Some(threads),
            )?),
            prev: None,
            pending_change: None,
            force_full: false,
            vp9_kbps,
            jpeg_bps_ema: None,
            last_low_encode: None,
            jpeg_bpp_ema: None,
            vp9_bytes_ema: None,
            low_rate_samples: 0,
            burst_hot_run: 0,
            recent_samples: VecDeque::new(),
            last_dirty_diag: None,
        })
    }

    /// Media Foundation VP9 leg instead of libvpx (Windows + `codec-mf`).
    /// With `hw_only` only a hardware MFT qualifies (the Microsoft Store
    /// software MFT is skipped); otherwise any registered MFT serves.
    /// `cpu_used` becomes a no-op on this backend.
    #[cfg(all(windows, feature = "codec-mf"))]
    pub fn with_mf_vp9(
        width: u32,
        height: u32,
        policy: LadderPolicy,
        vp9_kbps: u32,
        jpeg_quality: i32,
        hw_only: bool,
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
            vp9: Vp9Backend::Mf(crate::mf::MfEncoder::new_vp9(
                width, height, 30, vp9_kbps, hw_only,
            )?),
            prev: None,
            pending_change: None,
            force_full: false,
            vp9_kbps,
            jpeg_bps_ema: None,
            last_low_encode: None,
            jpeg_bpp_ema: None,
            vp9_bytes_ema: None,
            low_rate_samples: 0,
            burst_hot_run: 0,
            recent_samples: VecDeque::new(),
            last_dirty_diag: None,
        })
    }

    /// NVENC H.264 leg (Windows + `codec-nvenc`) instead of VP9. Only the
    /// server picks this, and only when the client advertised H.264 decode
    /// in the handshake (§18.75.9); otherwise it stays on a VP9 leg.
    #[cfg(all(windows, feature = "codec-nvenc"))]
    pub fn with_nvenc_h264(
        width: u32,
        height: u32,
        policy: LadderPolicy,
        h264_kbps: u32,
        jpeg_quality: i32,
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
            vp9: Vp9Backend::Nvenc(crate::nvenc::NvencEncoder::new(
                width, height, 30, h264_kbps,
            )?),
            prev: None,
            pending_change: None,
            force_full: false,
            vp9_kbps: h264_kbps,
            jpeg_bps_ema: None,
            last_low_encode: None,
            jpeg_bpp_ema: None,
            vp9_bytes_ema: None,
            low_rate_samples: 0,
            burst_hot_run: 0,
            recent_samples: VecDeque::new(),
            last_dirty_diag: None,
        })
    }

    /// Ladder tuning in use (live reference, e.g. to inspect the EMA).
    #[must_use]
    pub fn ladder(&self) -> &Ladder {
        &self.ladder
    }

    /// Sets the JPEG quality of the rects leg at runtime (§18.69, content
    /// quality axis); applies from the next encoded frame.
    pub fn set_jpeg_quality(&mut self, quality: i32) {
        self.jpeg.set_quality(quality);
    }

    /// Re-programs the VP9 leg's `cpu_used` at runtime (§18.69, content
    /// quality axis); applies from the next encoded frame.
    pub fn set_vp9_speed(&mut self, cpu_used: i32) -> Result<()> {
        self.vp9.set_cpu_used(cpu_used)
    }

    /// Sets the AQ mode (0 off .. 3 perceptual; §18.76) on the software VP9
    /// leg. No-op on the MF/NVENC legs (their AQ story, if any, is
    /// backend-specific and not wired yet).
    pub fn set_vp9_aq(&mut self, mode: u32) {
        if let Vp9Backend::Software(e) = &mut self.vp9 {
            if let Err(err) = e.set_aq_mode(mode) {
                tracing::warn!("VP9 AQ mode {mode} rejected: {err}");
            }
        }
    }

    /// Changed-area EMA as a fraction (0..1) for the content quality axis;
    /// `None` until the first frame feeds the ladder.
    #[must_use]
    pub fn change_ema_fraction(&self) -> Option<f64> {
        self.ladder.ema().map(|pct| (pct / 100.0).clamp(0.0, 1.0))
    }

    /// Forces a level from outside (congestion control). Bypasses the
    /// ladder's damping and does not feed its backoff; no-op when already
    /// there. The next frame goes as a keyframe of the new codec.
    pub fn force_level(&mut self, level: Level) {
        if self.ladder.force(Instant::now(), level).is_some() {
            tracing::info!(target: "ladder", to = ?level, "switch: external force (congestion)");
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

    /// Down-path cost veto (§18.75.19): true when the estimated JPEG cost
    /// of this frame's exact dirty area is not cheaper than the measured
    /// VP9 frame cost. Without measurements on both legs yet, no veto (the
    /// ladder starts in Low, so the JPEG cost model fills quickly).
    fn down_switch_vetoed(&self, dirty: &[rects::Rect]) -> bool {
        let (Some(bpp), Some(vp9_bytes)) = (self.jpeg_bpp_ema, self.vp9_bytes_ema) else {
            return false;
        };
        let est_jpeg = bpp * dirty_area_px(dirty).max(1) as f64;
        est_jpeg >= vp9_bytes * DOWN_COST_MARGIN
    }

    /// Marks the pending switch: keyframe + codec-change notification.
    fn begin_level(&mut self, level: Level) {
        if level == Level::Low {
            // Re-arm the Low-leg calibration: the first frames of a Low
            // visit are full-screen by definition, a stale
            // `last_low_encode` would measure a `dt` spanning the whole
            // High visit, and a stale bandwidth EMA (still hot from the
            // video content that drove the last up-switch) would start
            // the visit already pressing against the up-trigger —
            // together these were the flip-flop seen right after a
            // down-switch.
            self.low_rate_samples = 0;
            self.last_low_encode = None;
            self.jpeg_bps_ema = None;
            self.burst_hot_run = 0;
        }
        self.force_full = true;
        let codec = match level {
            Level::Low => VideoCodecType::JpegRects,
            Level::High => {
                // The peer rebuilds its decoder on the codec switch, so the
                // first VP9 frame of every visit must be a keyframe — the
                // encoder's initial force_key only covers the first one.
                self.vp9.request_keyframe();
                self.vp9.codec_type()
            }
        };
        self.pending_change = Some(codec);
    }
}

impl VideoEncoder for LadderEncoder {
    fn encode(&mut self, frame: &VideoFrame<'_>) -> Result<EncodedPacket> {
        self.check_dims(frame)?;

        // One diff per frame, shared by the metric and the JPEG encoder.
        // An "artificial" full frame (level switch, keyframe request, or
        // the very first frame ever) is protocol overhead, not a content
        // change: it must not feed the metric, or it bumps the area EMA
        // to the edge of the up threshold and the next menu pop flips the
        // regime right back (the flip-flop seen after a down-switch).
        let artificial_full = self.force_full || self.prev.is_none();
        let mut dirty = if self.force_full {
            self.force_full = false;
            vec![rects::Rect {
                left: 0,
                top: 0,
                right: self.width as i32,
                bottom: self.height as i32,
            }]
        } else {
            rects::dirty_rects(
                self.prev.as_deref(),
                frame.data,
                self.width,
                self.height,
                frame.stride,
            )
        };
        // Changed-area metric (§18.75.18): sampled and delta-thresholded,
        // NOT the exact encoding diff above — byte-exact diffing reports
        // ~100 % of the screen as "changed" on video (recompression
        // shimmer) and leaves the controllers without a working signal.
        let area_pct = if artificial_full {
            0.0
        } else {
            100.0
                * rects::changed_area_fraction(
                    self.prev.as_deref(),
                    frame.data,
                    self.width,
                    self.height,
                    frame.stride,
                )
                .unwrap_or(1.0)
        };

        // Lab diagnostics: every 5 s, what the diff is actually seeing —
        // how much of the screen is dirty and where the biggest rect sits.
        // Answers "what keeps changing on a static desktop?" without a
        // frame dump.
        if self
            .last_dirty_diag
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(5))
        {
            self.last_dirty_diag = Some(Instant::now());
            let biggest = dirty.iter().max_by_key(|r| r.area());
            tracing::info!(
                target: "ladder",
                level = ?self.ladder.level(),
                n_rects = dirty.len(),
                dirty_frac = format_args!(
                    "{:.3}",
                    dirty_area_px(&dirty) as f64
                        / (u64::from(self.width) * u64::from(self.height)) as f64
                ),
                biggest = biggest.map_or_else(
                    || "-".to_string(),
                    |r| format!("({},{})-({},{})", r.left, r.top, r.right, r.bottom)
                ),
                area_pct = format_args!("{area_pct:.2}"),
                "dirty diag"
            );
        }

        // The ladder sees the worst of two pressures: changed area and JPEG
        // bandwidth. A small window playing video changes little area but
        // costs far more than the VP9 regime — the bandwidth term (only
        // meaningful in Low, where egress is area-proportional) covers it.
        // In High the down path is area-driven but cost-vetoed (§18.75.19):
        // the area metric is thresholded and under-reads slow video, while
        // the JPEG cost follows the exact diff — without the veto the ladder
        // drops into a regime that is measurably dearer on the same content.
        let mut sample = area_pct;
        if self.ladder.level() == Level::Low {
            sample = sample.max(bw_pressure_pct(
                self.jpeg_bps_ema,
                self.vp9_kbps,
                self.ladder.policy().up_threshold_pct,
            ));
        } else if self.down_switch_vetoed(&dirty) {
            sample = sample.max(self.ladder.policy().up_threshold_pct);
        }

        // Down-path spike filter (§18.75.25): in High, feed the minimum
        // sample of the last second so periodic bursts (a scrolling log
        // console) cannot reset the down-hold forever; sustained video
        // keeps a hot minimum and stays up.
        let now = Instant::now();
        self.recent_samples.push_back((now, sample));
        while self
            .recent_samples
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > DOWN_MIN_WINDOW)
        {
            self.recent_samples.pop_front();
        }
        let fed = if self.ladder.level() == Level::High {
            self.recent_samples
                .iter()
                .map(|(_, s)| *s)
                .fold(sample, f64::min)
        } else {
            sample
        };

        if let Some(switch) = self.ladder.update(now, fed) {
            tracing::info!(
                target: "ladder",
                to = ?switch.to,
                sample = format_args!("{sample:.1}"),
                ema = format_args!("{:.1}", self.ladder.ema().unwrap_or(-1.0)),
                area_pct = format_args!("{area_pct:.2}"),
                jpeg_bps_ema = self.jpeg_bps_ema.unwrap_or(0.0),
                dwell_s = self.ladder.dwell().as_secs(),
                "switch: metric"
            );
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
        match self.ladder.level() {
            Level::Low => {
                let now = Instant::now();
                if let Some(prev_t) = self.last_low_encode.replace(now) {
                    let dt = now.duration_since(prev_t).as_secs_f64();
                    if dt > 0.0 {
                        let bps = packet.data.len() as f64 * 8.0 / dt;
                        self.jpeg_bps_ema = Some(match self.jpeg_bps_ema {
                            None => bps,
                            Some(ema) => ema + BW_EMA_ALPHA * (bps - ema),
                        });
                        // Burst guard: a scene cut at 1080p turns the JPEG
                        // leg into a ~25 Mbps flood; bail to the video
                        // regime at once instead of riding out the hold.
                        // Gated on warm-up (post-connect/switch calibration)
                        // and on a big dirty area — small-area bursts are
                        // ordinary interactivity, not floods.
                        self.low_rate_samples += 1;
                        let burst_rate =
                            f64::from(self.vp9_kbps) * 1000.0 * BW_UP_RATIO * BURST_MULT;
                        let screen_px = u64::from(self.width) * u64::from(self.height);
                        let hot = bps > burst_rate
                            && dirty_area_px(&dirty) as f64 >= screen_px as f64 * BURST_AREA_FRAC;
                        self.burst_hot_run = if hot { self.burst_hot_run + 1 } else { 0 };
                        if self.low_rate_samples >= BURST_WARMUP_SAMPLES
                            && self.burst_hot_run >= BURST_PERSIST_SAMPLES
                        {
                            tracing::info!(
                                target: "ladder",
                                bps = bps as u64,
                                burst_rate = burst_rate as u64,
                                hot_run = self.burst_hot_run,
                                dirty_frac = format_args!(
                                    "{:.2}",
                                    dirty_area_px(&dirty) as f64 / screen_px as f64
                                ),
                                "switch: burst guard"
                            );
                            self.force_level(Level::High);
                        }
                    }
                }
                // Cost model of the down veto: bytes per dirty pixel on
                // delta frames (full frames would bias it upward).
                let full = dirty_area_px(&dirty) >= u64::from(self.width) * u64::from(self.height);
                if !full {
                    let area = dirty_area_px(&dirty).max(1) as f64;
                    let bpp = packet.data.len() as f64 / area;
                    self.jpeg_bpp_ema = Some(match self.jpeg_bpp_ema {
                        None => bpp,
                        Some(ema) => ema + COST_EMA_ALPHA * (bpp - ema),
                    });
                }
            }
            Level::High => {
                let bytes = packet.data.len() as f64;
                self.vp9_bytes_ema = Some(match self.vp9_bytes_ema {
                    None => bytes,
                    Some(ema) => ema + COST_EMA_ALPHA * (bytes - ema),
                });
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
            Level::High => self.vp9.codec_type(),
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
        VideoFrame {
            width: w,
            height: h,
            stride: w as usize * 4,
            data,
        }
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
    fn down_switch_vetoed_when_jpeg_cost_exceeds_vp9() {
        // Cost model: JPEG measured at 0.5 bytes/px, VP9 at 1000 bytes/frame;
        // a half-screen dirty frame at 128x64 (4096 px) estimates 2048 bytes
        // of JPEG — dearer than VP9, so the down switch is vetoed.
        let (w, h) = (128u32, 64u32);
        let mut enc = LadderEncoder::new(w, h, policy(), 1000, 80).unwrap();
        enc.jpeg_bpp_ema = Some(0.5);
        enc.vp9_bytes_ema = Some(1000.0);
        let half = vec![rects::Rect {
            left: 0,
            top: 0,
            right: 128,
            bottom: 32,
        }];
        assert!(enc.down_switch_vetoed(&half));
        // Cheap JPEG (0.1 bytes/px -> 409 estimated) passes the veto.
        enc.jpeg_bpp_ema = Some(0.1);
        assert!(!enc.down_switch_vetoed(&half));
        // No measurements yet: no veto (bootstrap behaviour).
        enc.jpeg_bpp_ema = None;
        assert!(!enc.down_switch_vetoed(&half));
    }

    #[test]
    fn burst_guard_bails_to_high_immediately() {
        // In Low, sustained full-screen frames way over the burst rate flip
        // to High without any hold (scene cut flooding the paced queue).
        let (w, h) = (128u32, 64u32);
        let mut enc = LadderEncoder::new(w, h, policy(), 1, 80).unwrap();
        // vp9_kbps = 1 -> burst rate = 1000 * 2.0 * 1.5 = 3000 bps; any
        // non-trivial packet at ~2 ms intervals is hundreds of kbps.
        // Full-screen change every frame (100 % dirty passes the area
        // gate); the guard needs 8 consecutive hot samples.
        let a = solid(w, h, [10, 20, 30, 255]);
        enc.encode(&vf(w, h, &a)).unwrap();
        for i in 0..12 {
            std::thread::sleep(Duration::from_millis(2));
            let b = solid(w, h, [200, 100 + i, 50, 255]);
            enc.encode(&vf(w, h, &b)).unwrap();
        }
        assert_eq!(enc.ladder.level(), Level::High, "burst guard fired");
        assert_eq!(enc.codec_type(), VideoCodecType::Vp9);
    }

    #[test]
    fn burst_guard_ignores_small_area_bursts() {
        // A menu popping up: huge instantaneous rate but a small dirty
        // area. The guard must NOT fire — this is the JPEG leg's job.
        // Frame big enough that one 64x64 diff tile stays under the 1/3
        // area gate (640x480: one tile = 1.3 % of the screen).
        let (w, h) = (640u32, 480u32);
        let mut enc = LadderEncoder::new(w, h, policy(), 1, 80).unwrap();
        let base = solid(w, h, [10, 20, 30, 255]);
        enc.encode(&vf(w, h, &base)).unwrap();
        // Dirty a 32x16 patch (6 % of the screen) with new content every
        // frame, back to back: the instantaneous rate is astronomic, so
        // without the area gate the guard would fire on the 3rd sample.
        // No sleeps: the whole run stays inside the initial dwell, so the
        // metric/bw-pressure path (which WOULD legitimately escalate a
        // persistent hot spot) cannot interfere with the assertion.
        let mut patched = base.clone();
        let wu = w as usize;
        for i in 0..6u8 {
            for row in 8..24usize {
                for x in 8..40usize {
                    patched[(row * wu + x) * 4..][..4].copy_from_slice(&[
                        200,
                        100 + i * 20,
                        50,
                        255,
                    ]);
                }
            }
            enc.encode(&vf(w, h, &patched)).unwrap();
        }
        assert_eq!(
            enc.ladder.level(),
            Level::Low,
            "menu burst must not fire the guard"
        );
    }

    #[test]
    fn burst_guard_warmup_covers_connect() {
        // The frames right after entering Low are full-screen by
        // definition: even at burst rates the guard stays disarmed until
        // the warm-up samples are in.
        let (w, h) = (128u32, 64u32);
        let mut enc = LadderEncoder::new(w, h, policy(), 1, 80).unwrap();
        let a = solid(w, h, [10, 20, 30, 255]);
        enc.encode(&vf(w, h, &a)).unwrap();
        // First two delta frames at burst rate with full-screen change:
        // inside the warm-up (3 samples needed), no fire yet.
        for i in 0..2u8 {
            std::thread::sleep(Duration::from_millis(2));
            let b = solid(w, h, [200, 100 + i * 40, 50, 255]);
            enc.encode(&vf(w, h, &b)).unwrap();
            assert_eq!(
                enc.ladder.level(),
                Level::Low,
                "warm-up frame {i} must not fire"
            );
        }
    }

    #[test]
    fn down_switch_full_frame_does_not_feed_metric() {
        // Regression (flip-flop): the full frame that opens every Low
        // visit is protocol overhead, not content. With the test policy's
        // aggressive EMA (alpha 0.5), feeding its 100 % area would slam
        // the EMA against the up threshold and flip the regime right back.
        let (w, h) = (128u32, 64u32);
        let mut enc = LadderEncoder::new(w, h, policy(), 1000, 80).unwrap();
        let quiet = solid(w, h, [10, 20, 30, 255]);
        enc.encode(&vf(w, h, &quiet)).unwrap();
        enc.force_level(Level::High);
        // High visit on quiet content: real diffs are ~0, the EMA cools.
        for _ in 0..4 {
            enc.encode(&vf(w, h, &quiet)).unwrap();
        }
        let ema_before = enc.ladder().ema().expect("ema");
        enc.force_level(Level::Low);
        enc.encode(&vf(w, h, &quiet)).unwrap(); // artificial full frame
        let ema_after = enc.ladder().ema().expect("ema");
        assert!(
            ema_after <= ema_before + 1e-9,
            "artificial full frame bumped the EMA: {ema_before} -> {ema_after}"
        );
        assert_eq!(enc.ladder.level(), Level::Low);
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
        assert!(drive_until_state(
            &mut enc,
            w,
            h,
            &mut toggling,
            &mut vp9_keyframe_seen
        ));
        assert_eq!(enc.codec_type(), VideoCodecType::Vp9);
        assert!(
            vp9_keyframe_seen,
            "first VP9 frame after the switch is a keyframe"
        );
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

        assert!(drive_until(
            &mut enc,
            w,
            h,
            &quiet,
            VideoCodecType::JpegRects,
            600
        ));
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
        let d = dec.decode(&p).unwrap().expect("frame");
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
        assert!(
            p.keyframe,
            "keyframe request in VP9 regime must emit a keyframe"
        );

        // Re-entering High later must also start with a keyframe: the peer
        // rebuilds its decoder on the codec switch and a delta would be
        // undecodable ("Unspecified internal error", stuck until fallback).
        enc.force_level(Level::Low);
        enc.encode(&vf(w, h, &f)).unwrap();
        enc.force_level(Level::High);
        let p = enc.encode(&vf(w, h, &f)).unwrap();
        assert!(p.keyframe, "re-entering VP9 must emit a keyframe");
        let mut dec = Vp9Decoder::new().unwrap();
        dec.decode(&p)
            .expect("a fresh decoder accepts the re-entry keyframe");
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
