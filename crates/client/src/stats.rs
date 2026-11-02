// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Live session statistics (§15.3 debug aid).
//!
//! [`SessionStats`] is a lock-free shared accumulator: the session loops
//! and the audio thread update it as events happen, and once per second
//! the session emits a [`StatsSnapshot`] through
//! [`crate::ClientEvent::Stats`]. Consumers (the reference GUI overlay)
//! diff consecutive snapshots for per-second rates.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};

/// Point-in-time copy of every stat, sent to the consumer once per second.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StatsSnapshot {
    /// UDP leg active (false = reliable-stream fallback).
    pub udp: bool,
    /// Smoothed RTT (min-filtered per second, §12.2), milliseconds.
    pub rtt_ms: f32,
    /// RTT jitter EWMA, milliseconds.
    pub jitter_ms: f32,
    /// Datagram loss rate, per-mille (fraction × 1000).
    pub loss_permille: f32,
    /// Datagrams received since session start.
    pub dgrams_recv: u64,
    /// Datagrams estimated lost since session start.
    pub dgrams_lost: u64,
    /// Frames written to the framebuffer since session start.
    pub presented: u64,
    /// Frames that went through the hold queue since session start.
    pub held: u64,
    /// Frames dropped on hold-queue overflow since session start.
    pub dropped_overflow: u64,
    /// Frames dropped as stale since session start.
    pub dropped_stale: u64,
    /// Keyframe requests sent since session start.
    pub keyframe_requests: u64,
    /// Frames currently waiting in the hold queue.
    pub present_queue_len: u32,
    /// True when the A/V clock is audio-driven (false = present now).
    pub audio_clock_live: bool,
    /// Mean A/V skew of the current 5 s window, ms (video − audio).
    pub skew_avg_ms: i32,
    /// Max absolute A/V skew of the current 5 s window, ms.
    pub skew_max_abs_ms: i32,
    /// Audio frames decoded since session start (0 when audio is off).
    pub audio_decoded: u64,
    /// PLC-concealed audio frames since session start.
    pub audio_plc: u64,
    /// Jitter-buffer resyncs since session start.
    pub audio_resyncs: u64,
    /// Underrun (silence) samples played since session start.
    pub audio_underrun_samples: u64,
    /// Samples currently buffered for playback.
    pub audio_buffered_samples: u32,
}

/// Shared accumulator behind [`StatsSnapshot`]; one instance per session.
#[derive(Debug, Default)]
pub struct SessionStats {
    udp: AtomicBool,
    rtt_ms: AtomicU32,
    jitter_ms: AtomicU32,
    loss_permille: AtomicU32,
    dgrams_recv: AtomicU64,
    dgrams_lost: AtomicU64,
    presented: AtomicU64,
    held: AtomicU64,
    dropped_overflow: AtomicU64,
    dropped_stale: AtomicU64,
    keyframe_requests: AtomicU64,
    present_queue_len: AtomicU64,
    audio_clock_live: AtomicBool,
    skew_avg_ms: AtomicI32,
    skew_max_abs_ms: AtomicI32,
    audio_decoded: AtomicU64,
    audio_plc: AtomicU64,
    audio_resyncs: AtomicU64,
    audio_underrun_samples: AtomicU64,
    audio_buffered_samples: AtomicU64,
}

impl SessionStats {
    /// Transport leg in use.
    pub fn set_udp(&self, udp: bool) {
        self.udp.store(udp, Ordering::Relaxed);
    }

    /// Per-second network report (§12.2).
    pub fn set_net(&self, rtt_ms: f32, jitter_ms: f32, loss_permille: f32, recv: u64, lost: u64) {
        self.rtt_ms.store(rtt_ms.to_bits(), Ordering::Relaxed);
        self.jitter_ms.store(jitter_ms.to_bits(), Ordering::Relaxed);
        self.loss_permille.store(loss_permille.to_bits(), Ordering::Relaxed);
        self.dgrams_recv.store(recv, Ordering::Relaxed);
        self.dgrams_lost.store(lost, Ordering::Relaxed);
    }

    /// One frame written to the framebuffer.
    pub fn record_present(&self) {
        self.presented.fetch_add(1, Ordering::Relaxed);
    }

    /// One frame parked in the hold queue.
    pub fn record_hold(&self) {
        self.held.fetch_add(1, Ordering::Relaxed);
    }

    /// One frame dropped on hold-queue overflow.
    pub fn record_overflow_drop(&self) {
        self.dropped_overflow.fetch_add(1, Ordering::Relaxed);
    }

    /// One frame dropped as stale.
    pub fn record_stale_drop(&self) {
        self.dropped_stale.fetch_add(1, Ordering::Relaxed);
    }

    /// One keyframe request sent.
    pub fn record_keyframe_request(&self) {
        self.keyframe_requests.fetch_add(1, Ordering::Relaxed);
    }

    /// A/V presentation gauges, refreshed on every presentation tick.
    pub fn set_avsync(&self, queue_len: u32, clock_live: bool, skew_avg_ms: i32, skew_max_abs_ms: i32) {
        self.present_queue_len.store(u64::from(queue_len), Ordering::Relaxed);
        self.audio_clock_live.store(clock_live, Ordering::Relaxed);
        self.skew_avg_ms.store(skew_avg_ms, Ordering::Relaxed);
        self.skew_max_abs_ms.store(skew_max_abs_ms, Ordering::Relaxed);
    }

    /// Audio pipeline counters (called once per audio frame tick).
    pub fn set_audio(&self, decoded: u64, plc: u64, resyncs: u64, underrun_samples: u64, buffered: u32) {
        self.audio_decoded.store(decoded, Ordering::Relaxed);
        self.audio_plc.store(plc, Ordering::Relaxed);
        self.audio_resyncs.store(resyncs, Ordering::Relaxed);
        self.audio_underrun_samples.store(underrun_samples, Ordering::Relaxed);
        self.audio_buffered_samples.store(u64::from(buffered), Ordering::Relaxed);
    }

    /// Copies every stat into a plain snapshot.
    #[must_use]
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            udp: self.udp.load(Ordering::Relaxed),
            rtt_ms: f32::from_bits(self.rtt_ms.load(Ordering::Relaxed)),
            jitter_ms: f32::from_bits(self.jitter_ms.load(Ordering::Relaxed)),
            loss_permille: f32::from_bits(self.loss_permille.load(Ordering::Relaxed)),
            dgrams_recv: self.dgrams_recv.load(Ordering::Relaxed),
            dgrams_lost: self.dgrams_lost.load(Ordering::Relaxed),
            presented: self.presented.load(Ordering::Relaxed),
            held: self.held.load(Ordering::Relaxed),
            dropped_overflow: self.dropped_overflow.load(Ordering::Relaxed),
            dropped_stale: self.dropped_stale.load(Ordering::Relaxed),
            keyframe_requests: self.keyframe_requests.load(Ordering::Relaxed),
            present_queue_len: u32::try_from(self.present_queue_len.load(Ordering::Relaxed)).unwrap_or(u32::MAX),
            audio_clock_live: self.audio_clock_live.load(Ordering::Relaxed),
            skew_avg_ms: self.skew_avg_ms.load(Ordering::Relaxed),
            skew_max_abs_ms: self.skew_max_abs_ms.load(Ordering::Relaxed),
            audio_decoded: self.audio_decoded.load(Ordering::Relaxed),
            audio_plc: self.audio_plc.load(Ordering::Relaxed),
            audio_resyncs: self.audio_resyncs.load(Ordering::Relaxed),
            audio_underrun_samples: self.audio_underrun_samples.load(Ordering::Relaxed),
            audio_buffered_samples: u32::try_from(self.audio_buffered_samples.load(Ordering::Relaxed)).unwrap_or(u32::MAX),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reflects_every_update() {
        let stats = SessionStats::default();
        stats.set_udp(true);
        stats.set_net(0.5, 0.05, 1.5, 100, 2);
        stats.record_present();
        stats.record_present();
        stats.record_hold();
        stats.record_overflow_drop();
        stats.record_stale_drop();
        stats.record_keyframe_request();
        stats.set_avsync(2, true, -8, 27);
        stats.set_audio(300, 10, 2, 96_000, 1_920);
        let snap = stats.snapshot();
        assert!(snap.udp);
        assert!((snap.rtt_ms - 0.5).abs() < f32::EPSILON);
        assert!((snap.loss_permille - 1.5).abs() < f32::EPSILON);
        assert_eq!(snap.dgrams_recv, 100);
        assert_eq!(snap.dgrams_lost, 2);
        assert_eq!(snap.presented, 2);
        assert_eq!(snap.held, 1);
        assert_eq!(snap.dropped_overflow, 1);
        assert_eq!(snap.dropped_stale, 1);
        assert_eq!(snap.keyframe_requests, 1);
        assert_eq!(snap.present_queue_len, 2);
        assert!(snap.audio_clock_live);
        assert_eq!(snap.skew_avg_ms, -8);
        assert_eq!(snap.skew_max_abs_ms, 27);
        assert_eq!(snap.audio_decoded, 300);
        assert_eq!(snap.audio_plc, 10);
        assert_eq!(snap.audio_resyncs, 2);
        assert_eq!(snap.audio_underrun_samples, 96_000);
        assert_eq!(snap.audio_buffered_samples, 1_920);
    }
}
