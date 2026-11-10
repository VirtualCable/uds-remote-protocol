// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Codec ladder state machine (design doc §8.5).
//!
//! Decides when to switch between the base regime (rects-JPEG, low CPU,
//! bandwidth linear with changed area) and the video regime (VP9, flat
//! ~2 Mbps, high CPU) from the sustained changed-area percentage.
//!
//! Damping layers, each targeting a distinct oscillation frequency:
//!
//! - **EMA** (`ema_alpha`) filters per-frame noise.
//! - **Hysteresis** (`up_threshold` / `down_threshold`) filters threshold
//!   noise: the band between the two is "stay where you are".
//! - **Hold times** (`hold_up` / `hold_down`) filter short bursts: the EMA
//!   must stay past the threshold continuously for the hold time. Going up
//!   is urgent (video fluidity), going down is an optimization, so
//!   `hold_down` is normally much longer than `hold_up`.
//! - **Dynamic dwell** (`min_dwell`, `backoff`, `dwell_max`) filters slow
//!   oscillation: after each switch the encoder must stay in the new level
//!   for at least the current dwell. If levels keep ending quickly
//!   (oscillation), the dwell grows by `backoff` per switch up to
//!   `dwell_max`; a level that survives `STABLE_FACTOR` times its dwell
//!   decays the dwell back towards `min_dwell`.
//!
//! Forced switches (network congestion driving the level, keyframe
//! requests) go through [`Ladder::force`]: they apply immediately and do
//! **not** feed the backoff — they are orders, not oscillation.
//!
//! The machine is pure logic: time is injected, no codecs are involved.
//! [`crate::ladder_encoder`] (feature `codec-ladder`) wires it to real
//! encoders.

use std::time::{Duration, Instant};

/// Ladder level. `Low` is the cheap base regime (rects-JPEG), `High` the
/// video regime (VP9). Named generically so the same machine can later
/// drive the bitrate ladder (§12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Base regime: minimal CPU, bandwidth proportional to changed area.
    Low,
    /// Video regime: temporal prediction, flat-ish bandwidth, more CPU.
    High,
}

/// A decided level switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switch {
    /// Level to switch to.
    pub to: Level,
    /// True when the switch came from [`Ladder::force`] (congestion-driven);
    /// false for metric-driven switches.
    pub forced: bool,
}

/// Ladder tuning. All durations are wall-clock; percentages are the EMA of
/// the changed-area fraction of the screen (0.0-100.0).
#[derive(Debug, Clone)]
pub struct LadderPolicy {
    /// EMA percentage above which the High level is considered.
    pub up_threshold_pct: f64,
    /// EMA percentage below which the Low level is considered. Must be
    /// strictly below `up_threshold_pct` (hysteresis band).
    pub down_threshold_pct: f64,
    /// How long the EMA must stay above `up_threshold_pct` to go up.
    pub hold_up: Duration,
    /// How long the EMA must stay below `down_threshold_pct` to go down.
    pub hold_down: Duration,
    /// Base minimum time in a level before any metric-driven switch.
    pub min_dwell: Duration,
    /// Dwell multiplier applied when oscillation is detected (> 1.0).
    pub backoff: f64,
    /// Cap for the dynamic dwell.
    pub dwell_max: Duration,
    /// EMA smoothing factor (0, 1]: weight of the newest sample.
    pub ema_alpha: f64,
}

impl LadderPolicy {
    /// "balanced" defaults from the §8.4 measurements, hardened after the
    /// first interactive runs: a minimize/restore burst (~0.3-0.5 s of hot
    /// EMA) must never switch the regime. The holds double, the dwell
    /// doubles, and the slower EMA (`ema_alpha` 0.05) absorbs single-frame
    /// spikes completely — bursts are filtered by time, not by the
    /// threshold. The up threshold sits low (13 %): windowed video covers
    /// ~15-25 % of the screen and must reach the video regime; the second
    /// up-trigger (JPEG bandwidth pressure, see `LadderEncoder`) covers the
    /// cases the area metric misses. All of it stays configurable per
    /// session (`[standalone.ladder]`, `--ladder`).
    #[must_use]
    pub fn balanced() -> Self {
        Self {
            up_threshold_pct: 13.0,
            down_threshold_pct: 9.0,
            hold_up: Duration::from_secs(2),
            hold_down: Duration::from_secs(8),
            min_dwell: Duration::from_secs(10),
            backoff: 1.25,
            dwell_max: Duration::from_secs(120),
            ema_alpha: 0.05,
        }
    }
}

/// A level that lasted at least `dwell * STABLE_FACTOR` counts as stable and
/// decays the dynamic dwell back down instead of growing it.
const STABLE_FACTOR: u32 = 3;

/// Cap for the dwell gate on DOWN switches, as a multiple of
/// `policy.min_dwell`: the backoff-grown dwell protects against expensive
/// up-flapping, but the way down must stay timely (§18.75.27).
const DOWN_DWELL_CAP: u32 = 2;

/// Codec ladder state machine. See module docs for the damping layers.
#[derive(Debug)]
pub struct Ladder {
    policy: LadderPolicy,
    level: Level,
    /// EMA of the changed-area percentage; `None` until the first sample.
    ema: Option<f64>,
    /// When the current level was entered.
    entered: Instant,
    /// When the EMA first crossed the threshold toward the other level
    /// without leaving it since.
    crossing_since: Option<Instant>,
    /// Current dynamic dwell (starts at `policy.min_dwell`).
    dwell: Duration,
}

impl Ladder {
    /// Creates a ladder in `Level::Low`, entered at `now`.
    #[must_use]
    pub fn new(policy: LadderPolicy, now: Instant) -> Self {
        let dwell = policy.min_dwell;
        Self {
            policy,
            level: Level::Low,
            ema: None,
            entered: now,
            crossing_since: None,
            dwell,
        }
    }

    /// Current level.
    #[must_use]
    pub fn level(&self) -> Level {
        self.level
    }

    /// Policy in use (live reference for derived metrics).
    #[must_use]
    pub fn policy(&self) -> &LadderPolicy {
        &self.policy
    }

    /// Current dynamic dwell (grows under oscillation, decays when stable).
    #[must_use]
    pub fn dwell(&self) -> Duration {
        self.dwell
    }

    /// Current EMA of the changed-area percentage, if any sample was fed.
    #[must_use]
    pub fn ema(&self) -> Option<f64> {
        self.ema
    }

    /// Feeds one frame's changed-area percentage (0.0-100.0). Returns a
    /// switch when the machine decides to change level.
    pub fn update(&mut self, now: Instant, changed_pct: f64) -> Option<Switch> {
        let sample = changed_pct.clamp(0.0, 100.0);
        self.ema = Some(match self.ema {
            None => sample,
            Some(ema) => ema + self.policy.ema_alpha * (sample - ema),
        });
        let ema = self.ema.expect("just set");

        // Dwell gate: no metric-driven switch is allowed yet. The crossing
        // tracker is reset so the hold time only starts counting once the
        // dwell has expired. The gate applies the full (possibly
        // backoff-grown) dwell to UP switches — switching up is the
        // expensive direction (keyframe + decoder rebuild at the peer) —
        // but caps it at 2x the minimum for DOWN switches: staying in the
        // video regime wastes real bandwidth, so a past oscillation must
        // never delay the way down beyond ~2x min_dwell + hold_down
        // (§18.75.27).
        let gate = match self.level {
            Level::Low => self.dwell,
            Level::High => self.dwell.min(self.policy.min_dwell * DOWN_DWELL_CAP),
        };
        if now.duration_since(self.entered) < gate {
            self.crossing_since = None;
            return None;
        }

        let (threshold, hold, target) = match self.level {
            Level::Low => (self.policy.up_threshold_pct, self.policy.hold_up, Level::High),
            Level::High => (self.policy.down_threshold_pct, self.policy.hold_down, Level::Low),
        };
        let crossed = match self.level {
            Level::Low => ema > threshold,
            Level::High => ema < threshold,
        };
        if !crossed {
            self.crossing_since = None;
            return None;
        }
        let since = *self.crossing_since.get_or_insert(now);
        if now.duration_since(since) < hold {
            return None;
        }
        Some(self.switch_to(now, target, false))
    }

    /// Forces a level (congestion control, external policy). Applies
    /// immediately, bypassing dwell and holds, and does not feed the
    /// oscillation backoff. Returns `None` when already there.
    pub fn force(&mut self, now: Instant, level: Level) -> Option<Switch> {
        if level == self.level {
            return None;
        }
        Some(self.switch_to(now, level, true))
    }

    fn switch_to(&mut self, now: Instant, to: Level, forced: bool) -> Switch {
        if !forced {
            let lasted = now.duration_since(self.entered);
            let stable = self.dwell * STABLE_FACTOR;
            if lasted < stable {
                let grown = self.dwell.mul_f64(self.policy.backoff);
                self.dwell = grown.min(self.policy.dwell_max);
            } else {
                let shrunk = self.dwell.div_f64(self.policy.backoff);
                self.dwell = shrunk.max(self.policy.min_dwell);
            }
        }
        self.level = to;
        self.entered = now;
        self.crossing_since = None;
        Switch { to, forced }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    use super::*;

    /// Fast policy for tests: tiny holds/dwells, strong EMA response.
    fn test_policy() -> LadderPolicy {
        LadderPolicy {
            up_threshold_pct: 12.0,
            down_threshold_pct: 7.0,
            hold_up: Duration::from_millis(100),
            hold_down: Duration::from_millis(400),
            min_dwell: Duration::from_millis(500),
            backoff: 1.5,
            dwell_max: Duration::from_millis(5_000),
            ema_alpha: 1.0, // no smoothing: EMA = last sample
        }
    }

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn quiet_desktop_stays_low() {
        let mut l = Ladder::new(test_policy(), t0());
        for i in 1..100 {
            let sw = l.update(t0() + Duration::from_millis(i * 100), 1.0);
            assert_eq!(sw, None, "tick {i}");
            assert_eq!(l.level(), Level::Low);
        }
    }

    #[test]
    fn sustained_video_goes_up_after_hold() {
        let mut l = Ladder::new(test_policy(), t0());
        // Dwell (500 ms) first: no switch even with a hot EMA. Note the tick
        // at exactly 500 ms already expires the dwell (gate is `< dwell`),
        // so only ticks 1..=4 are asserted blocked here.
        for i in 1..=4 {
            assert_eq!(l.update(t0() + Duration::from_millis(i * 100), 50.0), None);
        }
        // Tick 500 ms expires the dwell and starts the hold_up window;
        // tick 550 ms is still inside hold_up (100 ms), 600 ms completes it.
        assert_eq!(l.update(t0() + Duration::from_millis(500), 50.0), None);
        assert_eq!(l.update(t0() + Duration::from_millis(550), 50.0), None);
        let sw = l.update(t0() + Duration::from_millis(650), 50.0);
        assert_eq!(sw, Some(Switch { to: Level::High, forced: false }));
        assert_eq!(l.level(), Level::High);
    }

    #[test]
    fn burst_shorter_than_hold_does_not_switch() {
        let mut l = Ladder::new(test_policy(), t0());
        // Past dwell, one hot frame, then quiet again before hold_up ends.
        assert_eq!(l.update(t0() + Duration::from_millis(600), 50.0), None);
        assert_eq!(l.update(t0() + Duration::from_millis(650), 1.0), None);
        // Even a long quiet run afterwards must not switch (crossing reset).
        assert_eq!(l.update(t0() + Duration::from_millis(900), 50.0), None);
        assert_eq!(l.level(), Level::Low);
    }

    #[test]
    fn hysteresis_band_holds_level() {
        let mut l = Ladder::new(test_policy(), t0());
        // Go up first.
        for i in 1..=8 {
            l.update(t0() + Duration::from_millis(i * 100), 50.0);
        }
        assert_eq!(l.level(), Level::High);
        // Inside the band (7-12 %): never crosses down_threshold.
        let base = t0() + Duration::from_millis(800);
        for i in 1..100 {
            let sw = l.update(base + Duration::from_millis(i * 100), 9.0);
            assert_eq!(sw, None, "tick {i}");
            assert_eq!(l.level(), Level::High);
        }
    }

    #[test]
    fn coming_down_needs_longer_hold() {
        let mut l = Ladder::new(test_policy(), t0());
        for i in 1..=8 {
            l.update(t0() + Duration::from_millis(i * 100), 50.0);
        }
        assert_eq!(l.level(), Level::High);
        let base = t0() + Duration::from_millis(800);
        // Dwell (500 ms) blocks the first 5 quiet ticks.
        for i in 1..=5 {
            assert_eq!(l.update(base + Duration::from_millis(i * 100), 1.0), None);
        }
        // hold_down (400 ms) from the first post-dwell crossing.
        assert_eq!(l.update(base + Duration::from_millis(600), 1.0), None);
        assert_eq!(l.update(base + Duration::from_millis(900), 1.0), None);
        let sw = l.update(base + Duration::from_millis(1100), 1.0);
        assert_eq!(sw, Some(Switch { to: Level::Low, forced: false }));
    }

    #[test]
    fn oscillation_grows_dwell_with_backoff() {
        let mut l = Ladder::new(test_policy(), t0());
        assert_eq!(l.dwell(), Duration::from_millis(500));
        // Each level ends quickly (lasted < dwell*3) -> dwell *= 1.5 per
        // switch, and each round has two switches (up + down).
        let mut now = t0();
        for round in 0..3 {
            // Drive up.
            loop {
                now += Duration::from_millis(100);
                if l.update(now, 50.0).is_some() {
                    break;
                }
            }
            // Drive down.
            loop {
                now += Duration::from_millis(100);
                if l.update(now, 1.0).is_some() {
                    break;
                }
            }
            let expected_ms = (500.0 * 1.5f64.powi(2 * (round + 1))) as u128;
            assert_eq!(l.dwell().as_millis(), expected_ms.min(5_000), "round {round}");
        }
    }

    #[test]
    fn dwell_is_capped_at_max() {
        let mut l = Ladder::new(test_policy(), t0());
        let mut now = t0();
        for _ in 0..20 {
            loop {
                now += Duration::from_millis(100);
                if l.update(now, 50.0).is_some() {
                    break;
                }
            }
            loop {
                now += Duration::from_millis(100);
                if l.update(now, 1.0).is_some() {
                    break;
                }
            }
        }
        assert_eq!(l.dwell(), Duration::from_millis(5_000));
    }

    #[test]
    fn stable_level_decays_dwell_back() {
        let mut l = Ladder::new(test_policy(), t0());
        let mut now = t0();
        // One quick oscillation (up + down) grows the dwell twice.
        for target in [50.0, 1.0] {
            loop {
                now += Duration::from_millis(100);
                if l.update(now, target).is_some() {
                    break;
                }
            }
        }
        assert_eq!(l.dwell(), Duration::from_millis(1125));
        // Stay Low and stable for 3x the dwell, then a slow, legitimate
        // climb: the switch back up decays the dwell one step.
        now += Duration::from_millis(1125 * 3 + 100);
        loop {
            now += Duration::from_millis(100);
            if l.update(now, 50.0).is_some() {
                break;
            }
        }
        assert_eq!(l.dwell(), Duration::from_millis(750));
    }

    #[test]
    fn down_switch_dwell_is_capped_despite_backoff() {
        let mut l = Ladder::new(test_policy(), t0());
        // Inflate the dwell with quick oscillation: 500 -> 750 -> 1125.
        let mut now = t0();
        for target in [50.0, 1.0] {
            loop {
                now += Duration::from_millis(100);
                if l.update(now, target).is_some() {
                    break;
                }
            }
        }
        assert_eq!(l.dwell(), Duration::from_millis(1125));
        // Back up to High with a forced switch (keeps the dwell).
        l.force(now, Level::High);
        // In High, the gate is capped at 2x min_dwell = 1000 ms: quiet
        // samples right after that point must switch down even though the
        // grown dwell (1125 ms) has not expired yet.
        now += Duration::from_millis(1010);
        assert!(l.update(now, 1.0).is_none(), "hold_down not complete yet");
        now += Duration::from_millis(400);
        let sw = l.update(now, 1.0);
        assert_eq!(sw, Some(Switch { to: Level::Low, forced: false }));
    }

    #[test]
    fn force_bypasses_dwell_and_skips_backoff() {
        let mut l = Ladder::new(test_policy(), t0());
        // Dwell is 500 ms; force applies immediately.
        let sw = l.force(t0() + Duration::from_millis(50), Level::High);
        assert_eq!(sw, Some(Switch { to: Level::High, forced: true }));
        assert_eq!(l.level(), Level::High);
        // Dwell untouched by the forced switch.
        assert_eq!(l.dwell(), Duration::from_millis(500));
        // Forcing the current level is a no-op.
        assert_eq!(l.force(t0() + Duration::from_millis(60), Level::High), None);
    }

    #[test]
    fn ema_smooths_single_spike() {
        let mut policy = test_policy();
        policy.ema_alpha = 0.1;
        let mut l = Ladder::new(policy, t0());
        // One huge spike after dwell: EMA only reaches ~4 % of the jump.
        assert_eq!(l.update(t0() + Duration::from_millis(600), 1.0), None);
        assert_eq!(l.update(t0() + Duration::from_millis(700), 100.0), None);
        let ema = l.ema().expect("samples fed");
        assert!(ema < 12.0, "ema {ema} must stay below the up threshold");
        assert_eq!(l.level(), Level::Low);
    }
}
