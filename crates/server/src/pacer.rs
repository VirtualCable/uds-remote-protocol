//! Token-bucket pacer for the UDP display leg (§12.6).
//!
//! Without pacing, one encoded frame is blasted at CPU speed: hundreds of
//! datagrams leave back-to-back, overflow every buffer on the path (socket
//! buffers, switch queues, the peer's NIC ring) and mostly die. A burst at
//! 10 GbE is not kinder than at 1 GbE — it just overflows faster. The pacer
//! spreads the burst at a configurable rate; the quality controller (§12.5)
//! retunes that rate live from the peer's loss reports.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Bits per second applied before the first quality sample arrives.
/// Conservative: a LAN usually absorbs 100 Mbps; the controller raises it.
pub const DEFAULT_PACER_BPS: u64 = 100_000_000;
/// Never pace below this, even under heavy congestion (§12.4 floor spirit:
/// below this the display leg degrades into the TCP fallback's territory).
pub const MIN_PACER_BPS: u64 = 25_000_000;
/// Never pace above this: beyond it, intermediate buffers melt again.
pub const MAX_PACER_BPS: u64 = 2_000_000_000;
/// Rate multiplier over the video target bitrate. Keyframes are ~10-20x the
/// average frame and must not stall for a whole frame interval, so the pacer
/// runs well above the encoder's average.
pub const RATE_HEADROOM: u64 = 8;
/// Burst allowance: how many bytes may leave instantly when the bucket is
/// full. ~64 KiB absorbs small delta frames without any added latency.
const BUCKET_BYTES: f64 = 65_536.0;

/// Shared rate knob (bits per second) written by the quality controller.
#[derive(Clone)]
pub struct PacerRate(Arc<AtomicU64>);

impl PacerRate {
    /// Creates the knob at the default rate; the returned `Arc` is the raw
    /// handle for custom writers (the quality controller uses the typed API).
    #[must_use]
    pub fn new() -> (Self, Arc<AtomicU64>) {
        let shared = Arc::new(AtomicU64::new(DEFAULT_PACER_BPS));
        (Self(shared.clone()), shared)
    }

    /// Retunes from the video target bitrate (kbps), with headroom for
    /// keyframe bursts and clamped to the sane band.
    pub fn set_from_video_kbps(&self, video_kbps: u32) {
        let bps = u64::from(video_kbps) * 1000 * RATE_HEADROOM;
        let bps = bps.clamp(MIN_PACER_BPS, MAX_PACER_BPS);
        self.0.store(bps, Ordering::Relaxed);
    }

    fn bps(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Token bucket: refills at the shared rate, drains one datagram at a time.
pub struct Pacer {
    rate: PacerRate,
    tokens: f64,
    last: Instant,
}

impl Pacer {
    /// Starts with a full bucket: the first burst (up to the bucket size)
    /// leaves immediately.
    #[must_use]
    pub fn new(rate: PacerRate) -> Self {
        Self { rate, tokens: BUCKET_BYTES, last: Instant::now() }
    }

    /// Waits until `bytes` may leave under the current rate.
    pub async fn pace(&mut self, bytes: usize) {
        let wait = self.consume(bytes, Instant::now());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
            // Debt accounting: we spent tokens we did not have yet.
            self.last = Instant::now();
        }
    }

    /// Book-keeps one datagram of `bytes` at time `now`; returns how long the
    /// caller must wait before sending it (zero when the bucket covers it).
    fn consume(&mut self, bytes: usize, now: Instant) -> std::time::Duration {
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        // Refill from the *current* rate (the knob may have moved).
        #[allow(clippy::cast_precision_loss)] // rates up to 2 Gbps fit f64 exactly
        let bytes_per_sec = self.rate.bps() as f64 / 8.0;
        self.tokens = (self.tokens + bytes_per_sec * elapsed).min(BUCKET_BYTES);
        #[allow(clippy::cast_precision_loss)] // datagram sizes fit f64 exactly
        let need = bytes as f64;
        if self.tokens >= need {
            self.tokens -= need;
            return std::time::Duration::ZERO;
        }
        let deficit = need - self.tokens;
        self.tokens = 0.0;
        std::time::Duration::from_secs_f64(deficit / bytes_per_sec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_bucket_allows_small_burst_immediately() {
        let (rate, _) = PacerRate::new();
        let mut p = Pacer::new(rate);
        assert!(p.consume(1024, Instant::now()).is_zero());
    }

    #[test]
    fn deficit_reports_refill_wait() {
        let (rate, shared) = PacerRate::new();
        shared.store(80_000_000, Ordering::Relaxed); // 10 MB/s
        let mut p = Pacer::new(rate);
        let t0 = Instant::now();
        assert!(p.consume(BUCKET_BYTES as usize, t0).is_zero()); // drain bucket
        // 10 KB at 10 MB/s = exactly 1 ms of wait.
        assert_eq!(p.consume(10_000, t0), std::time::Duration::from_millis(1));
    }

    #[test]
    fn bucket_refills_over_time() {
        let (rate, shared) = PacerRate::new();
        shared.store(80_000_000, Ordering::Relaxed); // 10 MB/s
        let mut p = Pacer::new(rate);
        let t0 = Instant::now();
        assert!(p.consume(BUCKET_BYTES as usize, t0).is_zero());
        // 10 ms later 100 KB refilled (capped at the bucket size).
        let t1 = t0 + std::time::Duration::from_millis(10);
        assert!(p.consume(10_000, t1).is_zero());
    }

    #[test]
    fn rate_knob_clamps_and_scales() {
        let (rate, _) = PacerRate::new();
        rate.set_from_video_kbps(4_000); // 4 Mbps video → 32 Mbps pacing
        assert_eq!(rate.bps(), 32_000_000);
        rate.set_from_video_kbps(1); // floor
        assert_eq!(rate.bps(), MIN_PACER_BPS);
        rate.set_from_video_kbps(10_000_000); // ceiling
        assert_eq!(rate.bps(), MAX_PACER_BPS);
    }
}
