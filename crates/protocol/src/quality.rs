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
//! | N1    | 50 %          | 30  | 100 %         |
//! | N2    | 25 %          | 30  | 100 %         |
//! | N3    | 25 %          | 20  | 100 %         |
//! | N4    | 25 %          | 15  | 100 %         |
//! | N5    | floor         | 15  | floor         |
//! | N6    | floor         | 10  | floor + reduced resolution |
//!
//! The bitrates are *factors* of the negotiated ceiling, clamped to the
//! configured floors — absolute numbers belong to session policy, not to
//! this controller.
//!
//! Congestion vs content regime: when the level drops at or below
//! [`QualityPolicy::force_video_codec_at`], the event carries
//! `force_video_codec: true` — the server should force the codec ladder to
//! its VP9 level (real rate control) instead of squeezing JPEG quality to
//! illegibility (§8.5).

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
    /// Baseline RTT in microseconds, learned during the connect burst.
    pub baseline_rtt_micros: u64,
    /// RTT / baseline above which a sample counts as bad (queue filling).
    pub rtt_bad_factor: f64,
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
    /// Lowest audio bitrate we ever apply (kbps, §12.4 floor).
    pub audio_floor_kbps: u32,
}

impl QualityPolicy {
    /// Defaults from §12: 1.5x baseline RTT is queue buildup, 1 % loss is
    /// real loss (not Wi-Fi noise), two bad samples to react, ten seconds of
    /// stability (at ~1 probe/s) before recovering one level.
    #[must_use]
    pub fn standard(baseline_rtt_micros: u64) -> Self {
        Self {
            baseline_rtt_micros: baseline_rtt_micros.max(1),
            rtt_bad_factor: 1.5,
            jitter_bad_micros: 15_000,
            loss_bad: 0.01,
            bad_to_down: 2,
            good_to_up: 10,
            raise_cooldown: Duration::from_secs(10),
            force_video_codec_at: 2,
            video_floor_kbps: 256,
            audio_floor_kbps: 16,
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
    (1.00, 30, 1.0), // N0 full
    (0.50, 30, 1.0), // N1 video 50 %
    (0.25, 30, 1.0), // N2 video 25 % (+ force VP9)
    (0.25, 20, 1.0), // N3 20 fps
    (0.25, 15, 1.0), // N4 15 fps
    (0.10, 15, 0.5), // N5 near the floors
    (0.10, 10, 0.5), // N6 degraded (reduced resolution, §12.3 last resort)
];

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
}

impl QualityController {
    /// Creates a controller at [`LEVEL_FULL`].
    #[must_use]
    pub fn new(policy: QualityPolicy) -> Self {
        Self {
            policy,
            level: LEVEL_FULL,
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

    /// Tuning in use (needed to resolve [`targets`] on level changes).
    #[must_use]
    pub fn policy(&self) -> &QualityPolicy {
        &self.policy
    }

    /// True when the sample is bad by any signal (§12.2).
    fn is_bad(&self, s: &Sample) -> bool {
        // Integer-domain compare: bad RTT = baseline * factor (factor scaled
        // x1024 to avoid float precision lints in the hot path).
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let factor_x1024 = (self.policy.rtt_bad_factor * 1024.0) as u128;
        let rtt_bad =
            u128::from(s.rtt_micros) * 1024 > u128::from(self.policy.baseline_rtt_micros) * factor_x1024;
        rtt_bad || s.jitter_micros > self.policy.jitter_bad_micros || s.loss > self.policy.loss_bad
    }

    /// Feeds one sample; returns an event when the level changes.
    pub fn update(&mut self, now: Instant, sample: Sample) -> Option<QualityEvent> {
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
            rtt_bad_factor: 1.5,
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
        assert_eq!(c.update(t0(), bad_rtt()), None); // first bad: no reaction
        let ev = c.update(t0() + Duration::from_secs(1), bad_rtt());
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
        // Down to level 2.
        for i in 0..4 {
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
        let mut now = t0();
        for _ in 0..30 {
            c.update(now, bad_rtt());
            now += Duration::from_secs(1);
        }
        assert_eq!(c.level(), LEVEL_DEGRADED);
    }

    #[test]
    fn force_video_codec_flag_below_threshold() {
        let mut c = QualityController::new(policy());
        // Level 1: no force.
        c.update(t0(), bad_rtt());
        let ev = c.update(t0() + Duration::from_secs(1), bad_rtt()).unwrap();
        assert!(!ev.force_video_codec);
        // Level 2: force.
        c.update(t0() + Duration::from_secs(2), bad_rtt());
        let ev = c.update(t0() + Duration::from_secs(3), bad_rtt()).unwrap();
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
    fn targets_respect_floors_and_ceiling() {
        let p = policy();
        let full = targets(LEVEL_FULL, 4_000, 64, &p);
        assert_eq!((full.video_kbps, full.fps, full.audio_kbps), (4_000, 30, 64));
        assert!(!full.degraded_resolution);

        let n5 = targets(5, 4_000, 64, &p);
        assert_eq!(n5.video_kbps, 400); // 10 % of ceiling, above the floor
        assert_eq!(n5.audio_kbps, 32);

        // A tiny ceiling clamps to the floors instead of going under.
        let floored = targets(5, 1_000, 24, &p);
        assert_eq!(floored.video_kbps, 256);
        assert_eq!(floored.audio_kbps, 16);

        let n6 = targets(LEVEL_DEGRADED, 4_000, 64, &p);
        assert!(n6.degraded_resolution);
        assert_eq!(n6.fps, 10);
    }
}
