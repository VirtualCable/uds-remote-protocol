// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Quality controller (design doc §12.5): maps link-quality signals onto a
//! fixed ladder of quality levels.
//!
//! The controller is pure logic: the caller feeds one [`Sample`] per probe
//! (RTT, jitter, UDP loss rate) plus the current time, and gets back a
//! [`QualityEvent`] when the level changes. Down is fast (a few consecutive
//! bad samples), up is slow (sustained good samples plus a cooldown between
//! raises), so the level never oscillates.
//!
//! Levels (§12.5, resolution untouched until the last resort):
//!
//! | level | video bitrate | fps | audio bitrate |
//! |-------|---------------|-----|---------------|
//! | N0    | 100 %         | 30  | 100 %         |
//! | N1    | 50 %          | 30  | 75 %          |
//! | N2    | 25 %          | 30  | 62.5 %        |
//! | N3    | 25 %          | 20  | 50 %          |
//! | N4    | 25 %          | 15  | 37.5 %        |
//! | N5    | 10 %          | 15  | 25 %          |
//! | N6    | 10 %          | 10  | 18.75 % + reduced resolution |
//!
//! With the default ceilings that resolves to opus-music 64→48→40→32→24→16→12
//! kbps and opus-voice 32→24→20→16→12→8→6 kbps (floor 6 kbps, §12.4).
//!
//! The bitrates are *factors* of the negotiated ceiling, clamped to the
//! configured floors — absolute numbers belong to session policy, not to
//! this controller.
//!
//! Independent of the level ladder, in-band FEC reacts directly to the loss
//! signal with hysteresis (§12.5): on above [`FEC_ON_LOSS`], off below
//! [`FEC_OFF_LOSS`], unchanged in between, so it never flaps.
//!
//! Congestion vs content regime: when the level drops at or below
//! [`QualityPolicy::force_video_codec_at`], the event carries
//! `force_video_codec: true` — the server should force the codec ladder to
//! its VP9 level (real rate control) instead of squeezing JPEG quality to
//! illegibility (§8.5).
//!
//! The RTT baseline is not the first sample but the **minimum RTT of a
//! sliding window** (60 s): a session that starts on an already congested
//! network re-learns the real distance as soon as the link breathes, and a
//! permanently worse route expires out of the window instead of signalling
//! congestion forever. Absolute signals (loss, jitter) need no baseline.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Quality level: 0 (full) through 6 (degraded, reduced resolution).
pub type QualityLevel = u8;

/// Highest (best) level.
pub const LEVEL_FULL: QualityLevel = 0;
/// Lowest (most degraded) level.
pub const LEVEL_DEGRADED: QualityLevel = 6;

/// One link-quality measurement (one answered probe window).
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    /// Smoothed RTT in microseconds.
    pub rtt_micros: u64,
    /// RTT variation (jitter) in microseconds.
    pub jitter_micros: u64,
    /// UDP loss rate in the window (0.0-1.0), from seq gaps.
    pub loss: f64,
    /// Bandwidth utilization against the configured `budget_kbps`
    /// (sent / budget; §18.70), `None` when no budget is set. Economy mode:
    /// being over budget counts as a bad sample even on a perfect network,
    /// so the ladder settles below the budget, not below the ceiling.
    pub budget_util: Option<f64>,
}

/// Controller tuning.
#[derive(Debug, Clone)]
pub struct QualityPolicy {
    /// Seed for the RTT baseline in microseconds (first healthy sample).
    /// The effective baseline is the **minimum RTT of the sliding window**
    /// (`baseline_window`), so a session that starts on an already
    /// congested network re-learns the real distance as soon as the link
    /// breathes, and a permanently worse route eventually expires out of
    /// the window instead of signalling congestion forever.
    pub baseline_rtt_micros: u64,
    /// Sliding window for the RTT baseline (minimum inside it).
    pub baseline_window: Duration,
    /// Hard floor for the effective RTT baseline (microseconds). On LANs the
    /// window minimum collapses to a few hundred µs and the bad threshold
    /// (`baseline * factor + slack`) lands inside normal decode/scheduling
    /// noise, pinning the ladder at the bottom with phantom congestion
    /// (§18.71 BUG-C2: baselines of 9,0 / 14,6 / 0,4 ms measured on the same
    /// link). The floor keeps the threshold meaningful on fast links.
    pub baseline_floor_micros: u64,
    /// RTT / baseline above which a sample counts as bad (queue filling).
    pub rtt_bad_factor: f64,
    /// Absolute slack added to the RTT bad threshold (microseconds). On fast
    /// links the baseline collapses to a few hundred µs and 1.5x it flags
    /// normal scheduling noise as congestion; the threshold is
    /// `baseline * factor + slack`, so a LAN sample must exceed the baseline
    /// by a *real* queueing delay to count as bad.
    pub rtt_bad_slack_micros: u64,
    /// Jitter above which a sample counts as bad (microseconds).
    pub jitter_bad_micros: u64,
    /// Loss rate above which a sample counts as bad.
    pub loss_bad: f64,
    /// Consecutive bad samples to step down one level.
    pub bad_to_down: u32,
    /// Consecutive good samples to step up one level.
    pub good_to_up: u32,
    /// Minimum time between two raises.
    pub raise_cooldown: Duration,
    /// From this level down, events ask to force the VP9 ladder level.
    pub force_video_codec_at: QualityLevel,
    /// Lowest video bitrate we ever apply (kbps, §12.4 floor).
    pub video_floor_kbps: u32,
    /// Lowest audio bitrate we ever apply (kbps, §12.4 floor; 6 is the
    /// lowest sane Opus target).
    pub audio_floor_kbps: u32,
    /// The quality table (§18.69); `default_levels()` reproduces §12.5.
    pub levels: Vec<LevelSpec>,
    /// Hard fps floor (§18.69 "miner" guard): neither axis ever goes below
    /// this, so an externally saturated host still yields *some* picture.
    pub min_fps_guaranteed: u32,
    /// Economy mode (§18.70): target video budget in kbps for deployments
    /// billed per byte (cloud egress). `None` (default) keeps the classic
    /// behavior — best quality up to the ceiling. With a budget set, being
    /// over it counts as congestion evidence with hysteresis
    /// ([`BUDGET_HI_UTIL`]/[`BUDGET_LO_UTIL`]), so the ladder settles below
    /// the budget instead of below the ceiling. The floors still win when
    /// the budget is unreachable.
    pub budget_kbps: Option<u32>,
}

/// Budget hysteresis (§18.70): utilization (sent / budget) above this marks
/// the sample bad.
pub const BUDGET_HI_UTIL: f64 = 0.9;
/// Utilization below this clears the budget-bad state (sticky in between).
pub const BUDGET_LO_UTIL: f64 = 0.7;

impl QualityPolicy {
    /// Defaults from §12: 1.5x baseline RTT is queue buildup, 1 % loss is
    /// real loss (not Wi-Fi noise), two bad samples to react, ten seconds of
    /// stability (at ~1 probe/s) before recovering one level. The baseline
    /// is the minimum RTT of the last 60 s (sliding window).
    #[must_use]
    pub fn standard(baseline_rtt_micros: u64) -> Self {
        Self {
            baseline_rtt_micros: baseline_rtt_micros.max(1),
            baseline_window: Duration::from_secs(60),
            baseline_floor_micros: 3_000,
            rtt_bad_factor: 1.5,
            rtt_bad_slack_micros: 2_000,
            jitter_bad_micros: 15_000,
            loss_bad: 0.01,
            bad_to_down: 2,
            good_to_up: 10,
            raise_cooldown: Duration::from_secs(10),
            force_video_codec_at: 2,
            video_floor_kbps: 256,
            audio_floor_kbps: 6,
            levels: default_levels(),
            min_fps_guaranteed: 1,
            budget_kbps: None,
        }
    }
}

/// One row of the configurable quality table (§18.69): what the server
/// applies while sitting at this level. Factors multiply the negotiated
/// ceilings; `jpeg_quality` and `vp9_speed` are the content-axis actuators.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelSpec {
    /// Fraction of the video ceiling bitrate (0, 1].
    pub video_factor: f64,
    /// Capture fps ceiling at this level.
    pub fps: u32,
    /// Fraction of the audio ceiling bitrate (0, 1].
    pub audio_factor: f64,
    /// JPEG quality (1-100) for the rects regime.
    pub jpeg_quality: i32,
    /// libvpx `cpu_used` (0 best/slowest .. 8 fastest) for the VP9 regime.
    pub vp9_speed: i32,
}

/// The default table: the fixed §12.5 ladder as data (same factors/fps).
/// JPEG quality steps 80→50 (70 stays "quite good", 60 "reasonable", §18.69);
/// `vp9_speed` stays at the low-latency 8 on every level.
#[must_use]
pub fn default_levels() -> Vec<LevelSpec> {
    LADDER
        .iter()
        .zip([80, 75, 70, 65, 60, 55, 50])
        .map(|(&(vfactor, fps, afactor), jpeg_quality)| LevelSpec {
            video_factor: vfactor,
            fps,
            audio_factor: afactor,
            jpeg_quality,
            vp9_speed: 8,
        })
        .collect()
}

/// A decided level change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualityEvent {
    /// New level.
    pub level: QualityLevel,
    /// True when going down (congestion), false when recovering.
    pub degraded: bool,
    /// True when the codec ladder should be forced to its video regime
    /// (level at or below `force_video_codec_at`).
    pub force_video_codec: bool,
}

/// The level ladder: `(video_bitrate_factor, fps, audio_bitrate_factor)`.
/// Factors apply to the negotiated ceilings, clamped to the policy floors.
const LADDER: [(f64, u32, f64); 7] = [
    (1.00, 30, 1.0),     // N0 full
    (0.50, 30, 0.75),    // N1 video 50 %, audio 75 %
    (0.25, 30, 0.625),   // N2 video 25 % (+ force VP9), audio 62.5 %
    (0.25, 20, 0.5),     // N3 20 fps, audio 50 %
    (0.25, 15, 0.375),   // N4 15 fps, audio 37.5 %
    (0.10, 15, 0.25),    // N5 near the floors
    (0.10, 10, 0.1875),  // N6 degraded (reduced resolution, §12.3 last resort)
];

/// Loss rate above which in-band FEC is switched on (§12.5).
pub const FEC_ON_LOSS: f64 = 0.02;

/// Loss rate below which in-band FEC is switched off; the band between this
/// and [`FEC_ON_LOSS`] keeps the previous state (hysteresis, no flapping).
pub const FEC_OFF_LOSS: f64 = 0.005;

/// FEC hysteresis (§12.5): enable in-band FEC when the loss rate exceeds
/// [`FEC_ON_LOSS`], keep it until loss falls below [`FEC_OFF_LOSS`].
#[must_use]
pub fn fec_update(enabled: bool, loss: f64) -> bool {
    if enabled {
        loss >= FEC_OFF_LOSS
    } else {
        loss > FEC_ON_LOSS
    }
}

/// Resolved targets for a level: absolute bitrates from ceiling + floors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualityTargets {
    /// Target video bitrate in kbps.
    pub video_kbps: u32,
    /// Target display framerate.
    pub fps: u32,
    /// Target audio bitrate in kbps.
    pub audio_kbps: u32,
    /// True at the degraded level (reduced encode resolution allowed).
    pub degraded_resolution: bool,
    /// JPEG quality to apply in the rects regime at this level (§18.69).
    pub jpeg_quality: i32,
    /// libvpx `cpu_used` to apply in the VP9 regime at this level (§18.69).
    pub vp9_speed: i32,
}

/// Resolves a level into absolute targets given the negotiated ceilings.
/// The row comes from `policy.levels` (clamped to the table length).
#[must_use]
pub fn targets(
    level: QualityLevel,
    video_ceiling_kbps: u32,
    audio_ceiling_kbps: u32,
    policy: &QualityPolicy,
) -> QualityTargets {
    let idx = (level as usize).min(policy.levels.len().saturating_sub(1));
    let row = policy.levels.get(idx).copied().unwrap_or(LevelSpec {
        video_factor: 1.0,
        fps: 30,
        audio_factor: 1.0,
        jpeg_quality: 80,
        vp9_speed: 8,
    });
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // factors are 0..1
    QualityTargets {
        video_kbps: ((f64::from(video_ceiling_kbps) * row.video_factor) as u32)
            .max(policy.video_floor_kbps),
        fps: row.fps.max(policy.min_fps_guaranteed),
        audio_kbps: ((f64::from(audio_ceiling_kbps) * row.audio_factor) as u32)
            .max(policy.audio_floor_kbps),
        degraded_resolution: idx >= policy.levels.len().saturating_sub(1)
            && policy.levels.len() > 1,
        jpeg_quality: row.jpeg_quality,
        vp9_speed: row.vp9_speed,
    }
}

/// Combines the network-axis row and the content-axis row (§18.69): bitrate
/// targets always come from the network level; `fps` is the minimum of both
/// rows (never below `min_fps_guaranteed`); `jpeg_quality`/`vp9_speed` come
/// from the content level.
#[must_use]
pub fn resolve_targets(
    network_level: QualityLevel,
    content_level: QualityLevel,
    video_ceiling_kbps: u32,
    audio_ceiling_kbps: u32,
    policy: &QualityPolicy,
) -> QualityTargets {
    let net = targets(network_level, video_ceiling_kbps, audio_ceiling_kbps, policy);
    let content = targets(content_level, video_ceiling_kbps, audio_ceiling_kbps, policy);
    QualityTargets {
        fps: net.fps.min(content.fps).max(policy.min_fps_guaranteed),
        jpeg_quality: content.jpeg_quality,
        vp9_speed: content.vp9_speed,
        degraded_resolution: net.degraded_resolution || content.degraded_resolution,
        ..net
    }
}

/// Weights and thresholds for the content axis (§18.69): a weighted pressure
/// from the content-change EMA, the encoder load and the bandwidth
/// utilization drives its own level (fps, JPEG quality, VP9 speed), with the
/// same down-fast/up-slow mechanics as the network axis.
#[derive(Debug, Clone)]
pub struct PressurePolicy {
    /// Weight of the changed-area EMA (0..1 normalized inside).
    pub weight_change: f64,
    /// Weight of the encoder load (encode time / frame budget).
    pub weight_cpu: f64,
    /// Weight of the bandwidth utilization (sent / ceiling). Counts only
    /// while congestion is measured (RTT/jitter/loss), so a fat pipe in use
    /// on a healthy network never degrades the picture.
    pub weight_bandwidth: f64,
    /// Pressure above which a sample counts as bad.
    pub pressure_hi: f64,
    /// Pressure below which a sample counts as good (the band in between is
    /// neutral: neither streak advances).
    pub pressure_lo: f64,
    /// Consecutive bad samples to step down one level.
    pub bad_to_down: u32,
    /// Consecutive good samples to step up one level.
    pub good_to_up: u32,
    /// Minimum time between two raises.
    pub raise_cooldown: Duration,
}

impl Default for PressurePolicy {
    /// Defaults (§18.69): bandwidth-dominant weights reproducing today's
    /// behavior; hi/lo band 0.85/0.60; same stepping cadence as §12.5.
    fn default() -> Self {
        Self {
            weight_change: 0.2,
            weight_cpu: 0.3,
            weight_bandwidth: 0.5,
            pressure_hi: 0.85,
            pressure_lo: 0.60,
            bad_to_down: 2,
            good_to_up: 10,
            raise_cooldown: Duration::from_secs(10),
        }
    }
}

/// One content-axis measurement.
#[derive(Debug, Clone, Copy)]
pub struct PressureSample {
    /// Changed-area fraction, EMA over the last seconds (0..1).
    pub change: f64,
    /// Encoder load: encode time / frame budget, EMA (0..1+; clamped).
    pub cpu: f64,
    /// Bandwidth utilization: sent bitrate / ceiling (0..1+; clamped).
    /// Only counted when `congested` is set (§18.69).
    pub util: f64,
    /// True when the network axis currently sees congestion (RTT/jitter/loss
    /// above its thresholds): gates the utilization term.
    pub congested: bool,
}

impl PressurePolicy {
    /// Weighted, normalized pressure of a sample (0..1, clamped).
    #[must_use]
    pub fn pressure(&self, s: &PressureSample) -> f64 {
        let util = if s.congested { s.util } else { 0.0 };
        // The bandwidth weight only participates while its signal is active:
        // otherwise the gated term would still dilute the denominator and a
        // loaded encoder on a healthy network could never read as pressure.
        let wsum = self.weight_change
            + self.weight_cpu
            + if s.congested { self.weight_bandwidth } else { 0.0 };
        if wsum <= f64::EPSILON {
            return 0.0;
        }
        let p = self.weight_change.mul_add(
            s.change.clamp(0.0, 1.0),
            self.weight_cpu.mul_add(
                s.cpu.clamp(0.0, 1.0),
                self.weight_bandwidth * util.clamp(0.0, 1.0),
            ),
        ) / wsum;
        p.clamp(0.0, 1.0)
    }
}

/// Content-axis controller (§18.69): steps a level through the same
/// down-fast/up-slow mechanics as [`QualityController`], driven by the
/// weighted pressure instead of network signals.
#[derive(Debug)]
pub struct PressureController {
    policy: PressurePolicy,
    level: QualityLevel,
    /// Highest level the table allows (set from the table length).
    max_level: QualityLevel,
    bad_streak: u32,
    good_streak: u32,
    last_raise: Option<Instant>,
}

impl PressureController {
    /// Creates a controller at level 0. `max_level` is the last row of the
    /// quality table (the controller never steps past it).
    #[must_use]
    pub fn new(policy: PressurePolicy, max_level: QualityLevel) -> Self {
        Self {
            policy,
            level: LEVEL_FULL,
            max_level,
            bad_streak: 0,
            good_streak: 0,
            last_raise: None,
        }
    }

    /// Current level.
    #[must_use]
    pub fn level(&self) -> QualityLevel {
        self.level
    }

    /// Tuning in use.
    #[must_use]
    pub fn policy(&self) -> &PressurePolicy {
        &self.policy
    }

    /// Feeds one sample; returns the new level when it changes.
    pub fn update(&mut self, now: Instant, sample: PressureSample) -> Option<QualityLevel> {
        let p = self.policy.pressure(&sample);
        if p > self.policy.pressure_hi {
            self.bad_streak += 1;
            self.good_streak = 0;
            if self.bad_streak >= self.policy.bad_to_down && self.level < self.max_level {
                self.bad_streak = 0;
                self.level += 1;
                return Some(self.level);
            }
            return None;
        }
        if p < self.policy.pressure_lo {
            self.good_streak += 1;
            self.bad_streak = 0;
            if self.good_streak >= self.policy.good_to_up && self.level > LEVEL_FULL {
                let cooled = self
                    .last_raise
                    .is_none_or(|t| now.duration_since(t) >= self.policy.raise_cooldown);
                if cooled {
                    self.good_streak = 0;
                    self.last_raise = Some(now);
                    self.level -= 1;
                    return Some(self.level);
                }
            }
            return None;
        }
        // Neutral band: neither streak advances (hysteresis, no flapping).
        None
    }
}

/// Per-signal verdict of one sample (§18.71): the caller logs this to see
/// exactly which signal is holding the ladder down (baseline collapse,
/// jitter, loss or budget) instead of inferring it from level changes.
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // one flag per signal IS the design
pub struct SampleVerdict {
    /// RTT above `baseline * factor + slack`.
    pub rtt_bad: bool,
    /// Jitter above its threshold.
    pub jitter_bad: bool,
    /// Loss above its threshold.
    pub loss_bad: bool,
    /// Sticky budget-overrun state (§18.70).
    pub budget_bad: bool,
    /// The RTT bad threshold applied to this sample (microseconds).
    pub rtt_threshold_micros: u64,
}

impl SampleVerdict {
    /// True when any signal reads as bad.
    #[must_use]
    pub fn any_bad(&self) -> bool {
        self.rtt_bad || self.jitter_bad || self.loss_bad || self.budget_bad
    }

    /// Comma-separated names of the bad signals (`"-"` when the sample is
    /// good): meant for log lines.
    #[must_use]
    pub fn bad_signals(&self) -> String {
        let mut out = Vec::new();
        if self.rtt_bad {
            out.push("rtt");
        }
        if self.jitter_bad {
            out.push("jitter");
        }
        if self.loss_bad {
            out.push("loss");
        }
        if self.budget_bad {
            out.push("budget");
        }
        if out.is_empty() { "-".to_string() } else { out.join(",") }
    }
}

/// Link-quality controller. See module docs for the mechanics.
#[derive(Debug)]
pub struct QualityController {
    policy: QualityPolicy,
    level: QualityLevel,
    /// Consecutive bad samples (reset by any good sample).
    bad_streak: u32,
    /// Consecutive good samples (reset by any bad sample).
    good_streak: u32,
    /// Last time the level went up (cooldown between raises).
    last_raise: Option<Instant>,
    /// Sliding window of `(when, rtt_micros)`; the effective baseline is the
    /// minimum RTT inside it (re-learned as the link improves, expired when
    /// the link permanently worsens).
    rtt_window: VecDeque<(Instant, u64)>,
    /// Effective RTT baseline: minimum of `rtt_window` (seeded from the
    /// policy, never below 1 µs).
    baseline_rtt_micros: u64,
    /// Lowest RTT ever reported (diagnostics).
    rtt_min_observed: u64,
    /// Highest RTT ever reported (diagnostics).
    rtt_max_observed: u64,
    /// Highest level the configured table allows (§18.69).
    max_level: QualityLevel,
    /// Sticky budget-bad state (§18.70): set above `BUDGET_HI_UTIL`, cleared
    /// below `BUDGET_LO_UTIL`, kept in between (hysteresis, no flapping).
    budget_bad: bool,
    /// The verdict of the last fed sample (post-hysteresis), for logging.
    last_verdict: Option<SampleVerdict>,
}

impl QualityController {
    /// Creates a controller at [`LEVEL_FULL`]; the deepest level is the last
    /// row of `policy.levels` (§18.69).
    #[must_use]
    pub fn new(policy: QualityPolicy) -> Self {
        let seed = policy.baseline_rtt_micros.max(1);
        #[allow(clippy::cast_possible_truncation)] // clamped to LEVEL_DEGRADED (6)
        let max_level = policy
            .levels
            .len()
            .saturating_sub(1)
            .min(LEVEL_DEGRADED as usize) as QualityLevel;
        Self {
            policy,
            level: LEVEL_FULL,
            bad_streak: 0,
            good_streak: 0,
            last_raise: None,
            rtt_window: VecDeque::new(),
            baseline_rtt_micros: seed,
            rtt_min_observed: seed,
            rtt_max_observed: seed,
            max_level,
            budget_bad: false,
            last_verdict: None,
        }
    }

    /// Current level.
    #[must_use]
    pub fn level(&self) -> QualityLevel {
        self.level
    }

    /// Tuning in use (needed to resolve [`targets`] on level changes).
    #[must_use]
    pub fn policy(&self) -> &QualityPolicy {
        &self.policy
    }

    /// Effective RTT baseline: minimum RTT of the sliding window.
    #[must_use]
    pub fn baseline_rtt_micros(&self) -> u64 {
        self.baseline_rtt_micros
    }

    /// Lowest and highest RTT ever reported (diagnostics).
    #[must_use]
    pub fn rtt_observed(&self) -> (u64, u64) {
        (self.rtt_min_observed, self.rtt_max_observed)
    }

    /// The verdict of the last fed sample, after the budget hysteresis was
    /// applied (§18.71 decision traces).
    #[must_use]
    pub fn last_verdict(&self) -> Option<SampleVerdict> {
        self.last_verdict
    }

    /// Feeds the sample's RTT into the sliding baseline window.
    fn track_rtt(&mut self, now: Instant, rtt_micros: u64) {
        self.rtt_min_observed = self.rtt_min_observed.min(rtt_micros);
        self.rtt_max_observed = self.rtt_max_observed.max(rtt_micros);
        self.rtt_window.push_back((now, rtt_micros));
        let cutoff = now.checked_sub(self.policy.baseline_window).unwrap_or(now);
        while self
            .rtt_window
            .front()
            .is_some_and(|(t, _)| *t < cutoff)
        {
            self.rtt_window.pop_front();
        }
        let window_min = self
            .rtt_window
            .iter()
            .map(|(_, r)| *r)
            .min()
            .unwrap_or(rtt_micros);
        self.baseline_rtt_micros = window_min.max(self.policy.baseline_floor_micros).max(1);
    }

    /// Per-signal breakdown of a sample (§18.71): which signals read as bad
    /// and the RTT threshold in use, so the caller can log *why* a sample
    /// counted against the level instead of guessing after the fact.
    #[must_use]
    pub fn verdict(&self, s: &Sample) -> SampleVerdict {
        // Integer-domain compare: bad RTT = baseline * factor + slack (the
        // factor is scaled x1024 to avoid float precision lints in the hot
        // path). The slack keeps sub-ms baselines from flagging normal noise.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let factor_x1024 = (self.policy.rtt_bad_factor * 1024.0) as u128;
        let threshold = u128::from(self.baseline_rtt_micros) * factor_x1024
            + u128::from(self.policy.rtt_bad_slack_micros) * 1024;
        let rtt_bad = u128::from(s.rtt_micros) * 1024 > threshold;
        SampleVerdict {
            rtt_bad,
            jitter_bad: s.jitter_micros > self.policy.jitter_bad_micros,
            loss_bad: s.loss > self.policy.loss_bad,
            budget_bad: self.budget_bad,
            #[allow(clippy::cast_possible_truncation)] // micros fit in u64
            rtt_threshold_micros: (threshold / 1024) as u64,
        }
    }

    /// Feeds one sample; returns an event when the level changes.
    pub fn update(&mut self, now: Instant, sample: Sample) -> Option<QualityEvent> {
        self.track_rtt(now, sample.rtt_micros);
        // Economy mode (§18.70): hysteresis on the budget utilization.
        if self.policy.budget_kbps.is_some()
            && let Some(util) = sample.budget_util
        {
            if util > BUDGET_HI_UTIL {
                self.budget_bad = true;
            } else if util < BUDGET_LO_UTIL {
                self.budget_bad = false;
            }
        }
        let verdict = self.verdict(&sample);
        self.last_verdict = Some(verdict);
        if verdict.any_bad() {
            self.bad_streak += 1;
            self.good_streak = 0;
            if self.bad_streak >= self.policy.bad_to_down && self.level < self.max_level {
                self.bad_streak = 0;
                self.level += 1;
                return Some(self.event(true));
            }
            return None;
        }

        self.good_streak += 1;
        self.bad_streak = 0;
        if self.good_streak >= self.policy.good_to_up && self.level > LEVEL_FULL {
            let cooled = self
                .last_raise
                .is_none_or(|t| now.duration_since(t) >= self.policy.raise_cooldown);
            if cooled {
                self.good_streak = 0;
                self.last_raise = Some(now);
                self.level -= 1;
                return Some(self.event(false));
            }
        }
        None
    }

    fn event(&self, degraded: bool) -> QualityEvent {
        QualityEvent {
            level: self.level,
            degraded,
            force_video_codec: self.level >= self.policy.force_video_codec_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> QualityPolicy {
        QualityPolicy {
            baseline_rtt_micros: 10_000,
            baseline_window: Duration::from_secs(60),
            baseline_floor_micros: 3_000,
            rtt_bad_factor: 1.5,
            rtt_bad_slack_micros: 2_000,
            jitter_bad_micros: 15_000,
            loss_bad: 0.01,
            bad_to_down: 2,
            good_to_up: 3,
            raise_cooldown: Duration::from_secs(10),
            force_video_codec_at: 2,
            video_floor_kbps: 256,
            audio_floor_kbps: 16,
            levels: default_levels(),
            min_fps_guaranteed: 1,
            budget_kbps: None,
        }
    }

    fn good() -> Sample {
        Sample { rtt_micros: 10_000, jitter_micros: 1_000, loss: 0.0, budget_util: None }
    }

    fn bad_rtt() -> Sample {
        Sample { rtt_micros: 20_000, jitter_micros: 1_000, loss: 0.0, budget_util: None }
    }

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn good_link_stays_full() {
        let mut c = QualityController::new(policy());
        for i in 1..100 {
            assert_eq!(c.update(t0() + Duration::from_secs(i), good()), None);
            assert_eq!(c.level(), LEVEL_FULL);
        }
    }

    #[test]
    fn two_bad_samples_step_down() {
        let mut c = QualityController::new(policy());
        c.update(t0(), good()); // seed the baseline window with a healthy RTT
        assert_eq!(c.update(t0() + Duration::from_secs(1), bad_rtt()), None); // first bad: no reaction
        let ev = c.update(t0() + Duration::from_secs(2), bad_rtt());
        assert_eq!(
            ev,
            Some(QualityEvent { level: 1, degraded: true, force_video_codec: false })
        );
    }

    #[test]
    fn single_bad_sample_recovers_without_level_change() {
        let mut c = QualityController::new(policy());
        assert_eq!(c.update(t0(), bad_rtt()), None);
        assert_eq!(c.update(t0() + Duration::from_secs(1), good()), None);
        assert_eq!(c.update(t0() + Duration::from_secs(2), good()), None);
        assert_eq!(c.level(), LEVEL_FULL, "streak reset by the good sample");
    }

    #[test]
    fn loss_alone_is_a_bad_signal() {
        let mut c = QualityController::new(policy());
        let lossy = Sample { loss: 0.05, ..good() };
        c.update(t0(), lossy);
        let ev = c.update(t0() + Duration::from_secs(1), lossy);
        assert!(ev.is_some_and(|e| e.degraded));
    }

    #[test]
    fn jitter_alone_is_a_bad_signal() {
        let mut c = QualityController::new(policy());
        let jumpy = Sample { jitter_micros: 40_000, ..good() };
        c.update(t0(), jumpy);
        assert!(c.update(t0() + Duration::from_secs(1), jumpy).is_some());
    }

    #[test]
    fn recovery_needs_sustained_good_and_cooldown() {
        let mut c = QualityController::new(policy());
        c.update(t0(), good()); // seed the baseline window
        // Down to level 2.
        for i in 1..5 {
            c.update(t0() + Duration::from_secs(i), bad_rtt());
        }
        assert_eq!(c.level(), 2);

        // 3 good samples raise one level.
        let mut now = t0() + Duration::from_secs(10);
        let mut raised = None;
        for _ in 0..3 {
            raised = c.update(now, good());
            now += Duration::from_secs(1);
        }
        assert_eq!(raised, Some(QualityEvent { level: 1, degraded: false, force_video_codec: false }));

        // Cooldown: 3 more good samples right away must NOT raise again.
        for _ in 0..3 {
            assert_eq!(c.update(now, good()), None, "cooldown blocks the raise");
            now += Duration::from_secs(1);
        }
        // After the cooldown, sustained good raises again (the event fires
        // on the first sample past the cooldown; keep the first Some).
        now += Duration::from_secs(10);
        let mut raised = None;
        for _ in 0..3 {
            if raised.is_none() {
                raised = c.update(now, good());
            } else {
                assert_eq!(c.update(now, good()), None);
            }
            now += Duration::from_secs(1);
        }
        assert_eq!(raised.map(|e| e.level), Some(0));
    }

    #[test]
    fn level_never_passes_degraded() {
        let mut c = QualityController::new(policy());
        c.update(t0(), good()); // seed the baseline window
        let mut now = t0() + Duration::from_secs(1);
        for _ in 0..30 {
            c.update(now, bad_rtt());
            now += Duration::from_secs(1);
        }
        assert_eq!(c.level(), LEVEL_DEGRADED);
    }

    #[test]
    fn force_video_codec_flag_below_threshold() {
        let mut c = QualityController::new(policy());
        c.update(t0(), good()); // seed the baseline window
        // Level 1: no force.
        c.update(t0() + Duration::from_secs(1), bad_rtt());
        let ev = c.update(t0() + Duration::from_secs(2), bad_rtt()).unwrap();
        assert!(!ev.force_video_codec);
        // Level 2: force.
        c.update(t0() + Duration::from_secs(3), bad_rtt());
        let ev = c.update(t0() + Duration::from_secs(4), bad_rtt()).unwrap();
        assert_eq!(ev.level, 2);
        assert!(ev.force_video_codec);
        // Recovery to level 1 clears the flag.
        let mut now = t0() + Duration::from_secs(20);
        let mut ev = None;
        for _ in 0..3 {
            ev = c.update(now, good());
            now += Duration::from_secs(1);
        }
        assert_eq!(ev.map(|e| e.force_video_codec), Some(false));
    }

    #[test]
    fn baseline_is_window_min_and_relaxes_when_the_link_breathes() {
        let mut c = QualityController::new(policy());
        // Session starts on an already congested link: RTT 100 ms.
        let congested = Sample { rtt_micros: 100_000, jitter_micros: 1_000, loss: 0.0, budget_util: None };
        for i in 0..10 {
            c.update(t0() + Duration::from_secs(i), congested);
        }
        assert_eq!(c.baseline_rtt_micros(), 100_000, "baseline learned congested");
        // The link breathes once (one probe slips through at 20 ms): the
        // window min becomes the real distance and later samples at 100 ms
        // correctly read as congestion again.
        c.update(t0() + Duration::from_secs(10), Sample { rtt_micros: 20_000, ..congested });
        assert_eq!(c.baseline_rtt_micros(), 20_000);
        let ev = c.update(t0() + Duration::from_secs(11), congested);
        let ev = ev.or_else(|| c.update(t0() + Duration::from_secs(12), congested));
        assert!(ev.is_some_and(|e| e.degraded), "100 ms vs 20 ms baseline is bad");
    }

    #[test]
    fn baseline_expires_a_permanently_worse_link() {
        let mut c = QualityController::new(policy());
        // Good link first.
        for i in 0..10 {
            c.update(t0() + Duration::from_secs(i), good());
        }
        assert_eq!(c.baseline_rtt_micros(), 10_000);
        // The link permanently worsens to 100 ms (new route): while the good
        // samples stay in the window it counts as congestion...
        let worse = Sample { rtt_micros: 100_000, jitter_micros: 1_000, loss: 0.0, budget_util: None };
        c.update(t0() + Duration::from_secs(10), worse);
        assert_eq!(c.baseline_rtt_micros(), 10_000);
        // ...but once the window slides past them, the baseline re-learns.
        c.update(t0() + Duration::from_secs(71), worse);
        assert_eq!(c.baseline_rtt_micros(), 100_000);
    }

    #[test]
    fn baseline_never_collapses_below_the_floor() {
        // §18.71 BUG-C2: a LAN session measured a 400 µs baseline and the
        // 2,6 ms threshold flagged decode noise as congestion forever.
        let mut c = QualityController::new(policy());
        let lan = Sample { rtt_micros: 400, jitter_micros: 100, loss: 0.0, budget_util: None };
        for i in 0..10 {
            c.update(t0() + Duration::from_secs(i), lan);
        }
        assert_eq!(c.baseline_rtt_micros(), 3_000, "floor clamps the collapse");
        // Threshold = 3000*1.5 + 2000 = 6500 µs: a 3 ms scheduling spike on
        // the same LAN must NOT count as congestion anymore.
        let spike = Sample { rtt_micros: 3_000, ..lan };
        assert!(!c.verdict(&spike).any_bad(), "spike inside the floored threshold is good");
        let v = c.verdict(&spike);
        assert_eq!(v.rtt_threshold_micros, 6_500);
        assert!(!v.any_bad());
        assert_eq!(v.bad_signals(), "-");
        // But a real queueing delay still reads as bad, named in the verdict.
        let queued = Sample { rtt_micros: 20_000, ..lan };
        let v = c.verdict(&queued);
        assert!(v.rtt_bad && v.any_bad());
        assert_eq!(v.bad_signals(), "rtt");
    }

    #[test]
    fn verdict_names_every_bad_signal() {
        let mut c = QualityController::new(QualityPolicy { budget_kbps: Some(800), ..policy() });
        c.update(t0(), Sample { budget_util: Some(1.5), ..good() });
        let v = c.verdict(&Sample {
            rtt_micros: 50_000,
            jitter_micros: 50_000,
            loss: 0.5,
            budget_util: Some(1.5),
        });
        assert_eq!(v.bad_signals(), "rtt,jitter,loss,budget");
    }

    #[test]
    fn observed_min_max_are_tracked() {
        let mut c = QualityController::new(policy());
        c.update(t0(), Sample { rtt_micros: 40_000, ..good() });
        c.update(t0() + Duration::from_secs(1), Sample { rtt_micros: 5_000, ..good() });
        c.update(t0() + Duration::from_secs(2), Sample { rtt_micros: 90_000, ..good() });
        assert_eq!(c.rtt_observed(), (5_000, 90_000));
    }

    #[test]
    fn targets_respect_floors_and_ceiling() {
        let p = policy();
        let full = targets(LEVEL_FULL, 4_000, 64, &p);
        assert_eq!((full.video_kbps, full.fps, full.audio_kbps), (4_000, 30, 64));
        assert!(!full.degraded_resolution);

        let n5 = targets(5, 4_000, 64, &p);
        assert_eq!(n5.video_kbps, 400); // 10 % of ceiling, above the floor
        assert_eq!(n5.audio_kbps, 16); // 25 % of 64

        // A tiny ceiling clamps to the floors instead of going under.
        let floored = targets(5, 1_000, 24, &p);
        assert_eq!(floored.video_kbps, 256);
        assert_eq!(floored.audio_kbps, 16);

        let n6 = targets(LEVEL_DEGRADED, 4_000, 64, &p);
        assert!(n6.degraded_resolution);
        assert_eq!(n6.fps, 10);
    }

    #[test]
    fn audio_ladder_steps_down_with_the_level() {
        // §12.5: opus-music (64k stereo ceiling) steps 64→48→40→32→24→16→12,
        // opus-voice (32k ceiling) 32→24→20→16→12→8→6 — one factor ladder
        // serves both presets; the floor (6 kbps here) is the last clamp.
        let p = QualityPolicy { audio_floor_kbps: 6, ..policy() };
        let music: Vec<u32> = (0..=6).map(|l| targets(l, 4_000, 64, &p).audio_kbps).collect();
        assert_eq!(music, [64, 48, 40, 32, 24, 16, 12]);
        let voice: Vec<u32> = (0..=6).map(|l| targets(l, 4_000, 32, &p).audio_kbps).collect();
        assert_eq!(voice, [32, 24, 20, 16, 12, 8, 6]);
        // Never above the ceiling, never below the floor.
        let tiny: Vec<u32> = (0..=6).map(|l| targets(l, 4_000, 7, &p).audio_kbps).collect();
        assert!(tiny.iter().all(|&k| (6..=7).contains(&k)), "{tiny:?}");
    }

    #[test]
    fn targets_come_from_the_configured_table() {
        // A custom 2-level table ("cheap jpegrects" policy, §18.69).
        let p = QualityPolicy {
            levels: vec![
                LevelSpec { video_factor: 1.0, fps: 30, audio_factor: 1.0, jpeg_quality: 70, vp9_speed: 8 },
                LevelSpec { video_factor: 0.2, fps: 12, audio_factor: 0.5, jpeg_quality: 55, vp9_speed: 8 },
            ],
            ..policy()
        };
        let n0 = targets(0, 4_000, 64, &p);
        assert_eq!((n0.video_kbps, n0.fps, n0.jpeg_quality, n0.vp9_speed), (4_000, 30, 70, 8));
        assert!(!n0.degraded_resolution);
        let n1 = targets(1, 4_000, 64, &p);
        assert_eq!((n1.video_kbps, n1.fps, n1.audio_kbps), (800, 12, 32));
        assert_eq!((n1.jpeg_quality, n1.vp9_speed), (55, 8));
        assert!(n1.degraded_resolution, "last row of the table is the degraded one");
        // The network controller never steps past the last configured row.
        let mut c = QualityController::new(p);
        c.update(t0(), good());
        let mut now = t0() + Duration::from_secs(1);
        for _ in 0..30 {
            c.update(now, bad_rtt());
            now += Duration::from_secs(1);
        }
        assert_eq!(c.level(), 1);
    }

    #[test]
    fn min_fps_guaranteed_floors_every_row() {
        let p = QualityPolicy {
            min_fps_guaranteed: 5,
            ..policy()
        };
        let n6 = targets(LEVEL_DEGRADED, 4_000, 64, &p);
        assert_eq!(n6.fps, 10, "table row above the floor passes through");
        let p = QualityPolicy {
            min_fps_guaranteed: 12,
            ..policy()
        };
        let n6 = targets(LEVEL_DEGRADED, 4_000, 64, &p);
        assert_eq!(n6.fps, 12, "the guaranteed floor clamps the table row");
    }

    #[test]
    fn pressure_weights_normalize_and_gate_utilization() {
        let pol = PressurePolicy::default();
        let s = PressureSample { change: 1.0, cpu: 1.0, util: 1.0, congested: true };
        assert!((pol.pressure(&s) - 1.0).abs() < 1e-9, "all max is pressure 1");
        // Utilization is gated by measured congestion: a fat pipe on a
        // healthy network does not press.
        let not_congested = PressureSample { congested: false, ..s };
        let p = pol.pressure(&not_congested);
        assert!(
            (p - 1.0).abs() < 1e-9,
            "util term gated out AND out of the normalizer: {p}"
        );
        // Zero-sum weights mean "no pressure signal at all": never bad.
        let zero = PressurePolicy { weight_change: 0.0, weight_cpu: 0.0, weight_bandwidth: 0.0, ..pol };
        assert_eq!(zero.pressure(&s), 0.0);
    }

    #[test]
    fn pressure_controller_steps_down_fast_and_up_slow() {
        let pol = PressurePolicy { bad_to_down: 2, good_to_up: 3, raise_cooldown: Duration::from_secs(10), ..Default::default() };
        let mut c = PressureController::new(pol, 3);
        let hot = PressureSample { change: 1.0, cpu: 1.0, util: 1.0, congested: true };
        let cold = PressureSample { change: 0.0, cpu: 0.0, util: 0.0, congested: false };
        assert_eq!(c.update(t0(), hot), None, "first bad sample only arms");
        assert_eq!(c.update(t0() + Duration::from_secs(1), hot), Some(1));
        // Neutral band: no streak movement.
        let mid = PressureSample { change: 0.7, cpu: 0.7, util: 0.0, congested: false };
        assert_eq!(c.update(t0() + Duration::from_secs(2), mid), None);
        assert_eq!(c.level(), 1);
        // Sustained cold raises one level (good_to_up=3).
        let mut now = t0() + Duration::from_secs(3);
        let mut raised = None;
        for _ in 0..3 {
            raised = c.update(now, cold);
            now += Duration::from_secs(1);
        }
        assert_eq!(raised, Some(0));
        // Never past max_level.
        for _ in 0..20 {
            c.update(now, hot);
            now += Duration::from_secs(1);
        }
        assert_eq!(c.level(), 3);
    }

    #[test]
    fn resolve_targets_splits_the_axes() {
        let p = policy();
        // Network at N2 (500 kbps factors), content at N3 (20 fps, jpeg 65).
        let t = resolve_targets(2, 3, 4_000, 64, &p);
        assert_eq!(t.video_kbps, 1_000, "bitrate is the network axis' (25 %)");
        assert_eq!(t.audio_kbps, 40);
        assert_eq!(t.fps, 20, "fps is the minimum of both rows");
        assert_eq!(t.jpeg_quality, 65, "jpeg quality is the content axis'");
        assert_eq!(t.vp9_speed, 8);
        // Content axis more degraded than network: fps follows the worst.
        let t = resolve_targets(0, 4, 4_000, 64, &p);
        assert_eq!((t.video_kbps, t.fps, t.jpeg_quality), (4_000, 15, 60));
    }

    #[test]
    fn budget_overrun_degrades_on_a_perfect_network() {
        // §18.70: with a budget set, over-utilization alone (RTT/jitter/loss
        // all healthy) steps the ladder down with hysteresis.
        let p = QualityPolicy { budget_kbps: Some(800), ..policy() };
        let mut c = QualityController::new(p);
        c.update(t0(), good());
        let over = Sample { budget_util: Some(1.2), ..good() };
        assert_eq!(c.update(t0() + Duration::from_secs(1), over), None, "first bad arms");
        let ev = c.update(t0() + Duration::from_secs(2), over);
        assert!(ev.is_some_and(|e| e.degraded && e.level == 1), "over budget steps down");
        // Inside the hysteresis band the state sticks (still bad), but the
        // streak restarted at the step: two more bad samples are needed.
        let mid = Sample { budget_util: Some(0.8), ..good() };
        assert!(c.verdict(&mid).any_bad(), "0.8 is inside the sticky band while bad");
        assert_eq!(c.update(t0() + Duration::from_secs(3), mid), None);
        let ev = c.update(t0() + Duration::from_secs(4), mid);
        assert!(ev.is_some_and(|e| e.level == 2), "still bad: keeps stepping");
        // Below the low mark the budget signal clears and good streaks start.
        let under = Sample { budget_util: Some(0.5), ..good() };
        assert!({
            c.update(t0() + Duration::from_secs(5), under);
            !c.verdict(&under).any_bad()
        }, "under budget clears the bad state");
    }

    #[test]
    fn no_budget_ignores_the_utilization_field() {
        let mut c = QualityController::new(policy()); // budget_kbps: None
        c.update(t0(), good());
        let crazy = Sample { budget_util: Some(99.0), ..good() };
        assert_eq!(c.update(t0() + Duration::from_secs(1), crazy), None);
        assert_eq!(c.level(), LEVEL_FULL);
    }

    #[test]
    fn fec_hysteresis_never_flaps() {
        // Off below the band, on above it, sticky inside it (§12.5).
        assert!(!fec_update(false, 0.0));
        assert!(!fec_update(false, 0.01), "1 % loss is below the on threshold");
        assert!(!fec_update(false, FEC_ON_LOSS), "exactly at the threshold stays off");
        assert!(fec_update(false, 0.03));
        assert!(fec_update(true, 0.01), "inside the band FEC stays on");
        assert!(!fec_update(false, 0.01), "inside the band FEC stays off");
        assert!(!fec_update(true, 0.004));
        assert!(!fec_update(true, 0.0));
    }
}
