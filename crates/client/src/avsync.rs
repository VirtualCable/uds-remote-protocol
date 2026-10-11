//! A/V presentation sync, audio-master (design doc §9.1).
//!
//! Video fragments and audio datagrams share the server's session clock via
//! `capture_ts_ms`. The client maps the audio playback cursor onto that
//! clock (`audio_now`) and presents each decoded video frame when the audio
//! clock reaches the frame's timestamp:
//!
//! - no audio (or no output device): present immediately — there is nothing
//!   to sync to;
//! - frame due now or up to [`PRESENT_AHEAD_MS`] early: present immediately;
//! - frame up to [`MAX_HOLD_MS`] in the future: hold it until the audio
//!   clock catches up;
//! - further out: clamp to `audio_now + MAX_HOLD_MS` (clock-mismatch guard);
//! - frame more than [`STALE_DROP_MS`] behind: drop it — stale video is
//!   worse than a skipped frame.
//!
//! This module is the pure decision core: no clocks, no I/O, fully unit
//! tested. The plumbing (audio cursor mapping, presentation queue) lives in
//! the client binary.

/// A frame at most this far ahead of the audio clock is presented at once
/// (below this threshold the skew is imperceptible, §9.1 PoC).
pub const PRESENT_AHEAD_MS: i64 = 30;

/// Furthest into the future a frame is held waiting for the audio clock.
/// Frames beyond this are clamped to `audio_now + MAX_HOLD_MS`.
pub const MAX_HOLD_MS: i64 = 200;

/// A frame further than this behind the audio clock is stale and dropped.
pub const STALE_DROP_MS: i64 = 500;

/// What to do with a decoded video frame (§9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentDecision {
    /// Present the frame immediately.
    PresentNow,
    /// Hold the frame until the audio clock reaches the given timestamp
    /// (ms on the shared session clock), then present it.
    HoldUntil(u32),
    /// Drop the frame (too stale to be worth presenting).
    Drop,
}

/// Decides what to do with a decoded video frame stamped `video_ts_ms`,
/// given the audio playback cursor `audio_now_ms` (both ms on the shared
/// session clock, §9.1). With `has_audio` false (no audio announced or no
/// output device) every frame is presented immediately.
#[must_use]
pub fn decide(video_ts_ms: u32, audio_now_ms: u32, has_audio: bool) -> PresentDecision {
    if !has_audio {
        return PresentDecision::PresentNow;
    }
    // i64 math: the skew is signed and either side can lead.
    let diff = i64::from(video_ts_ms) - i64::from(audio_now_ms);
    if diff < -STALE_DROP_MS {
        return PresentDecision::Drop;
    }
    if diff <= PRESENT_AHEAD_MS {
        return PresentDecision::PresentNow;
    }
    // MAX_HOLD_MS is a small positive constant; the cast cannot truncate.
    #[allow(clippy::cast_possible_truncation)]
    let target = if diff <= MAX_HOLD_MS {
        video_ts_ms
    } else {
        audio_now_ms.saturating_add(MAX_HOLD_MS as u32)
    };
    PresentDecision::HoldUntil(target)
}

/// Running A/V sync statistics, logged periodically by the client (§9.1):
/// skew of presented frames (`video_ts - audio_now` at present time,
/// negative = video late) plus drop/hold counters.
#[derive(Debug, Default)]
pub struct SyncStats {
    /// Frames presented since the last reset.
    pub presented: u64,
    /// Frames that spent time in the presentation queue.
    pub held: u64,
    /// Frames dropped for being stale (> [`STALE_DROP_MS`] behind).
    pub dropped_stale: u64,
    /// Held frames dropped because the presentation queue overflowed.
    pub dropped_overflow: u64,
    skew_sum_ms: i64,
    skew_max_abs_ms: i64,
}

impl SyncStats {
    /// Records one presented frame and its skew (`video_ts - audio_now`).
    pub fn record_present(&mut self, skew_ms: i64) {
        self.presented += 1;
        self.skew_sum_ms += skew_ms;
        self.skew_max_abs_ms = self.skew_max_abs_ms.max(skew_ms.abs());
    }

    /// Records one frame entering the presentation queue.
    pub fn record_hold(&mut self) {
        self.held += 1;
    }

    /// Records one stale drop.
    pub fn record_stale_drop(&mut self) {
        self.dropped_stale += 1;
    }

    /// Records one presentation-queue overflow drop.
    pub fn record_overflow_drop(&mut self) {
        self.dropped_overflow += 1;
    }

    /// Mean skew of presented frames since the last reset (0 if none).
    #[must_use]
    pub fn skew_avg_ms(&self) -> i64 {
        if self.presented == 0 {
            return 0;
        }
        // i64 division of a sum by its own count cannot overflow.
        #[allow(clippy::cast_possible_wrap)]
        let n = self.presented as i64;
        self.skew_sum_ms / n
    }

    /// Largest absolute skew of a presented frame since the last reset.
    #[must_use]
    pub fn skew_max_abs_ms(&self) -> i64 {
        self.skew_max_abs_ms
    }

    /// Clears all counters (called after each periodic log line).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_audio_presents_immediately() {
        // Any timestamp relation: without an audio clock there is nothing
        // to sync to (§9.1).
        for (v, a) in [(0, 0), (1_000, 0), (0, 10_000), (u32::MAX, 0)] {
            assert_eq!(decide(v, a, false), PresentDecision::PresentNow);
        }
    }

    #[test]
    fn due_or_slightly_early_presents_now() {
        // Due exactly, behind within the window, or ≤30 ms ahead.
        assert_eq!(decide(1_000, 1_000, true), PresentDecision::PresentNow);
        assert_eq!(decide(980, 1_000, true), PresentDecision::PresentNow);
        assert_eq!(decide(1_030, 1_000, true), PresentDecision::PresentNow);
    }

    #[test]
    fn near_future_is_held_until_its_ts() {
        assert_eq!(
            decide(1_100, 1_000, true),
            PresentDecision::HoldUntil(1_100)
        );
        assert_eq!(
            decide(1_200, 1_000, true),
            PresentDecision::HoldUntil(1_200)
        );
    }

    #[test]
    fn far_future_is_clamped_to_the_hold_cap() {
        // Clock-mismatch guard: never wait longer than MAX_HOLD_MS.
        assert_eq!(
            decide(5_000, 1_000, true),
            PresentDecision::HoldUntil(1_200)
        );
    }

    #[test]
    fn stale_frames_are_dropped() {
        assert_eq!(decide(499, 1_000, true), PresentDecision::Drop);
        assert_eq!(decide(0, 10_000, true), PresentDecision::Drop);
        // Boundary: exactly STALE_DROP_MS behind is still presentable.
        assert_eq!(decide(500, 1_000, true), PresentDecision::PresentNow);
    }

    #[test]
    fn stats_track_and_reset() {
        let mut st = SyncStats::default();
        st.record_present(-10);
        st.record_present(30);
        st.record_hold();
        st.record_stale_drop();
        st.record_overflow_drop();
        assert_eq!(st.presented, 1 + 1);
        assert_eq!(st.skew_avg_ms(), 10);
        assert_eq!(st.skew_max_abs_ms(), 30);
        assert_eq!(st.held, 1);
        assert_eq!(st.dropped_stale, 1);
        assert_eq!(st.dropped_overflow, 1);
        st.reset();
        assert_eq!(st.presented, 0);
        assert_eq!(st.skew_avg_ms(), 0);
        assert_eq!(st.skew_max_abs_ms(), 0);
    }
}
