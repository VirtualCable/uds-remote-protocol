//! Adaptive capture fps (§12.7): the encoder's own cost is the signal.
//!
//! The quality controller (§12) reacts to the *network*; it cannot see a
//! server that is simply too slow to encode the configured rate (a VDI VM
//! with 4 vCPUs and no GPU encoding VP9 at 1680x1050 tops out near 10 fps
//! whatever the link can carry). This adapter watches the per-frame encode
//! time (EMA) against the frame budget of the current rate:
//!
//! - load = EMA(encode) / (1000 ms / fps)
//! - load > `high_load` sustained for `hold_down` → step the rate down
//! - load < `low_load` sustained for `hold_up` → step the rate back up
//!
//! The configured/quality fps stays the ceiling: the adapter only ever
//! lowers the rate, and the effective fps written to the capture thread is
//! `min(quality_fps, adaptive_fps)` so the two controllers never fight
//! (the network one lowers the ceiling, this one lowers the actual rate
//! below it when the CPU is the bottleneck).
//!
//! Steps are multiplicative (x0.75 down, x1.25 up, min step 1) so the
//! adapter converges in a few moves without a fixed ladder table, and each
//! direction has its own hold so a single slow frame cannot yank the rate.

use std::time::{Duration, Instant};

/// Tuning knobs for [`FpsAdapter`]; see the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct FpsPolicy {
    /// Lowest rate the adapter may select.
    pub min_fps: u32,
    /// EMA encode load above this fraction of the frame budget steps down.
    pub high_load: f64,
    /// EMA encode load below this fraction of the frame budget steps up.
    pub low_load: f64,
    /// Sustained-over-`high_load` time before stepping down.
    pub hold_down: Duration,
    /// Sustained-under-`low_load` time before stepping up.
    pub hold_up: Duration,
    /// EMA smoothing factor (0, 1]: weight of the newest encode sample.
    pub ema_alpha: f64,
}

impl FpsPolicy {
    /// Balanced defaults: react to sustained overload in about a second,
    /// recover more cautiously (two seconds), never below 8 fps.
    #[must_use]
    pub fn balanced() -> Self {
        Self {
            min_fps: 8,
            high_load: 0.6,
            low_load: 0.3,
            hold_down: Duration::from_millis(1000),
            hold_up: Duration::from_millis(2000),
            ema_alpha: 0.3,
        }
    }

    /// Parses the compact wire form (`key=value,...`, same style as the
    /// codec-ladder overrides). An empty spec — or the literal `on` —
    /// yields the balanced defaults; unknown keys and bad values are
    /// rejected so a typo cannot silently tune nothing.
    pub fn from_wire(spec: &str) -> anyhow::Result<Self> {
        let mut policy = Self::balanced();
        for pair in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            if pair == "on" {
                continue;
            }
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("bad adaptive-fps override (expected key=value): {pair}"))?;
            match key.trim() {
                "min_fps" => policy.min_fps = value.parse().map_err(|_| anyhow::anyhow!("bad min_fps"))?,
                "high" => policy.high_load = value.parse().map_err(|_| anyhow::anyhow!("bad high_load"))?,
                "low" => policy.low_load = value.parse().map_err(|_| anyhow::anyhow!("bad low_load"))?,
                "hold_up_ms" => {
                    policy.hold_up =
                        Duration::from_millis(value.parse().map_err(|_| anyhow::anyhow!("bad hold_up_ms"))?);
                }
                "hold_down_ms" => {
                    policy.hold_down =
                        Duration::from_millis(value.parse().map_err(|_| anyhow::anyhow!("bad hold_down_ms"))?);
                }
                "ema_alpha" => {
                    policy.ema_alpha = value.parse().map_err(|_| anyhow::anyhow!("bad ema_alpha"))?;
                }
                other => anyhow::bail!("unknown adaptive-fps override: {other}"),
            }
        }
        policy.validate()?;
        Ok(policy)
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.min_fps >= 1, "adaptive-fps min_fps must be >= 1");
        anyhow::ensure!(
            0.0 < self.low_load && self.low_load < self.high_load,
            "adaptive-fps loads must satisfy 0 < low < high"
        );
        anyhow::ensure!(
            (0.0..=1.0).contains(&self.ema_alpha),
            "adaptive-fps ema_alpha must be in (0, 1]"
        );
        Ok(())
    }
}

/// Stateful adapter: feed it every encode duration and it returns the fps
/// the capture thread should run at (never above the ceiling).
#[derive(Debug)]
pub struct FpsAdapter {
    policy: FpsPolicy,
    /// Upper bound: the configured fps (the quality controller's target is
    /// applied by the caller as another bound on top of this).
    ceiling: u32,
    /// Currently selected adaptive rate (<= ceiling).
    current: u32,
    /// EMA of the encode time, in milliseconds.
    ema_ms: Option<f64>,
    /// Since when the load has stayed above `high_load` (None = not above).
    above_since: Option<Instant>,
    /// Since when the load has stayed below `low_load` (None = not below).
    below_since: Option<Instant>,
}

impl FpsAdapter {
    /// Starts at the ceiling (no adaptation until evidence accumulates).
    #[must_use]
    pub fn new(policy: FpsPolicy, ceiling: u32) -> Self {
        let ceiling = ceiling.max(policy.min_fps);
        Self {
            policy,
            ceiling,
            current: ceiling,
            ema_ms: None,
            above_since: None,
            below_since: None,
        }
    }

    /// The currently selected adaptive rate.
    #[must_use]
    pub fn fps(&self) -> u32 {
        self.current.min(self.ceiling)
    }

    /// Current EMA encode load against the active budget (for telemetry).
    #[must_use]
    pub fn load(&self) -> Option<f64> {
        self.ema_ms.map(|ema| {
            let budget_ms = 1000.0 / f64::from(self.fps());
            ema / budget_ms
        })
    }

    /// Feeds one frame's encode time; returns the new rate when it changed.
    pub fn observe(&mut self, encode: Duration, now: Instant) -> Option<u32> {
        let ms = encode.as_secs_f64() * 1000.0;
        let ema = match self.ema_ms {
            Some(prev) => prev + self.policy.ema_alpha * (ms - prev),
            None => ms,
        };
        self.ema_ms = Some(ema);

        let fps = self.fps();
        let load = ema / (1000.0 / f64::from(fps));
        if load > self.policy.high_load {
            self.below_since = None;
            let since = *self.above_since.get_or_insert(now);
            if now.duration_since(since) >= self.policy.hold_down && fps > self.policy.min_fps {
                self.current = (fps * 3 / 4).max(self.policy.min_fps).min(fps - 1);
                self.above_since = None;
                return Some(self.fps());
            }
        } else if load < self.policy.low_load {
            self.above_since = None;
            if fps < self.ceiling {
                let since = *self.below_since.get_or_insert(now);
                if now.duration_since(since) >= self.policy.hold_up {
                    self.current = (fps + (fps / 4).max(1)).min(self.ceiling);
                    self.below_since = None;
                    return Some(self.fps());
                }
            }
        } else {
            self.above_since = None;
            self.below_since = None;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(ceiling: u32) -> FpsAdapter {
        FpsAdapter::new(FpsPolicy::balanced(), ceiling)
    }

    #[test]
    fn starts_at_the_ceiling() {
        assert_eq!(adapter(30).fps(), 30);
    }

    #[test]
    fn sustained_overload_steps_down_and_floors_at_min() {
        let mut a = adapter(30);
        let mut t = Instant::now();
        // Encode takes 25 ms per frame: at 30 fps the budget is 33 ms →
        // load ~0.75 > 0.6, so the adapter must walk down.
        let mut last = a.fps();
        for _ in 0..200 {
            t += Duration::from_millis(100);
            if let Some(new) = a.observe(Duration::from_millis(25), t) {
                assert!(new < last, "steps only down under overload");
                last = new;
            }
        }
        assert!(a.fps() < 30, "converged below the ceiling: {}", a.fps());
        assert!(a.fps() >= a.policy.min_fps);
    }

    #[test]
    fn idle_encoder_steps_back_up_to_the_ceiling() {
        let mut a = adapter(30);
        let mut t = Instant::now();
        for _ in 0..50 {
            t += Duration::from_millis(100);
            a.observe(Duration::from_millis(40), t); // overload: force down
        }
        let low = a.fps();
        assert!(low < 30);
        for _ in 0..300 {
            t += Duration::from_millis(100);
            a.observe(Duration::from_millis(2), t); // cheap encode
        }
        assert_eq!(a.fps(), 30, "recovers to the ceiling");
    }

    #[test]
    fn holds_prevent_single_frame_yanks() {
        let mut a = adapter(30);
        let t = Instant::now();
        // One slow frame is not a sustained overload.
        assert_eq!(a.observe(Duration::from_millis(500), t), None);
        assert_eq!(a.fps(), 30);
    }

    #[test]
    fn wire_roundtrip_and_validation() {
        let p = FpsPolicy::from_wire("").expect("empty = balanced");
        assert_eq!(p, FpsPolicy::balanced());
        let p = FpsPolicy::from_wire("min_fps=5,high=0.7,low=0.2,hold_down_ms=500,ema_alpha=0.5")
            .expect("parse");
        assert_eq!(p.min_fps, 5);
        assert_eq!(p.high_load, 0.7);
        assert_eq!(p.low_load, 0.2);
        assert_eq!(p.hold_down, Duration::from_millis(500));
        assert_eq!(p.ema_alpha, 0.5);
        assert!(FpsPolicy::from_wire("bogus=1").is_err());
        assert!(FpsPolicy::from_wire("high=0.2,low=0.5").is_err()); // low < high
        assert!(FpsPolicy::from_wire("min_fps=0").is_err());
    }
}
