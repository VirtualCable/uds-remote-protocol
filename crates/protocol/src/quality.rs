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
}

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
        }
    }
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
}

/// Resolves a level into absolute targets given the negotiated ceilings.
#[must_use]
pub fn targets(
    level: QualityLevel,
    video_ceiling_kbps: u32,
    audio_ceiling_kbps: u32,
    policy: &QualityPolicy,
) -> QualityTargets {
    let level = level.min(LEVEL_DEGRADED);
    let (vfactor, fps, afactor) = LADDER[level as usize];
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // factors are 0..1
    QualityTargets {
        video_kbps: ((f64::from(video_ceiling_kbps) * vfactor) as u32)
            .max(policy.video_floor_kbps),
        fps,
        audio_kbps: ((f64::from(audio_ceiling_kbps) * afactor) as u32)
            .max(policy.audio_floor_kbps),
        degraded_resolution: level == LEVEL_DEGRADED,
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
}

impl QualityController {
    /// Creates a controller at [`LEVEL_FULL`].
    #[must_use]
    pub fn new(policy: QualityPolicy) -> Self {
        let seed = policy.baseline_rtt_micros.max(1);
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
        self.baseline_rtt_micros = window_min.max(1);
    }

    /// True when the sample is bad by any signal (§12.2).
    fn is_bad(&self, s: &Sample) -> bool {
        // Integer-domain compare: bad RTT = baseline * factor + slack (the
        // factor is scaled x1024 to avoid float precision lints in the hot
        // path). The slack keeps sub-ms baselines from flagging normal noise.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let factor_x1024 = (self.policy.rtt_bad_factor * 1024.0) as u128;
        let threshold = u128::from(self.baseline_rtt_micros) * factor_x1024
            + u128::from(self.policy.rtt_bad_slack_micros) * 1024;
        let rtt_bad = u128::from(s.rtt_micros) * 1024 > threshold;
        rtt_bad || s.jitter_micros > self.policy.jitter_bad_micros || s.loss > self.policy.loss_bad
    }

    /// Feeds one sample; returns an event when the level changes.
    pub fn update(&mut self, now: Instant, sample: Sample) -> Option<QualityEvent> {
        self.track_rtt(now, sample.rtt_micros);
        if self.is_bad(&sample) {
            self.bad_streak += 1;
            self.good_streak = 0;
            if self.bad_streak >= self.policy.bad_to_down && self.level < LEVEL_DEGRADED {
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
        }
    }

    fn good() -> Sample {
        Sample { rtt_micros: 10_000, jitter_micros: 1_000, loss: 0.0 }
    }

    fn bad_rtt() -> Sample {
        Sample { rtt_micros: 20_000, jitter_micros: 1_000, loss: 0.0 }
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
        let congested = Sample { rtt_micros: 100_000, jitter_micros: 1_000, loss: 0.0 };
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
        let worse = Sample { rtt_micros: 100_000, jitter_micros: 1_000, loss: 0.0 };
        c.update(t0() + Duration::from_secs(10), worse);
        assert_eq!(c.baseline_rtt_micros(), 10_000);
        // ...but once the window slides past them, the baseline re-learns.
        c.update(t0() + Duration::from_secs(71), worse);
        assert_eq!(c.baseline_rtt_micros(), 100_000);
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
