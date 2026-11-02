// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Audio jitter buffer (design doc §9.1).
//!
//! UDP datagrams arrive out of order and with losses; this buffer reorders
//! them by their `seq` counter (see [`protocol::audio`]) within a small
//! target depth (default 3 frames = 60 ms at 20 ms/frame) and feeds the
//! decoder exactly one frame per pop, in sequence:
//!
//! - Late packets (a `seq` already played) are dropped.
//! - Missing frames produce [`JitterOutput::Plc`] requests — the decoder's
//!   packet loss concealment synthesizes a plausible frame instead of
//!   silence (§9). Consecutive PLC is capped ([`MAX_CONSECUTIVE_PLC`]);
//!   beyond that the buffer resyncs to the newest available packet (or
//!   re-primes if none), so a long outage never turns into an endless PLC
//!   stream.
//!
//! Pure and synchronous: no tokio, no sockets (§15 wasm rule). The caller
//! (audio output loop) drives the clock: one [`JitterBuffer::pop`] per frame
//! duration.
//!
//! `seq` is a plain `u32` compared without wrap handling: at 50 packets/s
//! the counter wraps after ~2.7 years of continuous audio, far beyond any
//! session.

use std::collections::BTreeMap;

/// Default target depth in frames (3 × 20 ms = 60 ms, §9.1).
pub const DEFAULT_TARGET_DEPTH: usize = 3;

/// Minimum configurable target depth (frames).
pub const MIN_TARGET_DEPTH: usize = 2;

/// Maximum configurable target depth (frames).
pub const MAX_TARGET_DEPTH: usize = 4;

/// Cap on consecutive PLC frames before the buffer resyncs (§9.1).
pub const MAX_CONSECUTIVE_PLC: usize = 5;

/// One frame popped from the jitter buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JitterOutput {
    /// A received packet (the codec frame bytes, header already stripped).
    Packet(Vec<u8>),
    /// The frame for this slot was lost: ask the decoder for PLC
    /// (`decode(None, ...)`).
    Plc,
}

/// Cumulative counters, for the loss metrics reported on the control
/// channel (§12.2).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JitterStats {
    /// Packets accepted into the buffer.
    pub received: u64,
    /// Packets dropped because their `seq` was already played.
    pub late_dropped: u64,
    /// PLC frames emitted (one per missing frame).
    pub plc_frames: u64,
    /// Resync events (PLC cap hit, or a large gap skipped).
    pub resyncs: u64,
}

/// Reordering jitter buffer for one audio stream. See the module docs.
#[derive(Debug)]
pub struct JitterBuffer {
    target_depth: usize,
    pending: BTreeMap<u32, Vec<u8>>,
    /// Next `seq` to deliver; meaningful only while `started`.
    next_play: u32,
    /// False until the target depth is first reached (priming).
    started: bool,
    consecutive_plc: usize,
    stats: JitterStats,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl JitterBuffer {
    /// Creates a buffer with the default target depth (3 frames).
    #[must_use]
    pub fn new() -> Self {
        Self::with_depth(DEFAULT_TARGET_DEPTH)
    }

    /// Creates a buffer with a custom target depth, clamped to
    /// [`MIN_TARGET_DEPTH`]..=[`MAX_TARGET_DEPTH`] (§9.1: 2-4 frames).
    #[must_use]
    pub fn with_depth(target_depth: usize) -> Self {
        Self {
            target_depth: target_depth.clamp(MIN_TARGET_DEPTH, MAX_TARGET_DEPTH),
            pending: BTreeMap::new(),
            next_play: 0,
            started: false,
            consecutive_plc: 0,
            stats: JitterStats::default(),
        }
    }

    /// Cumulative counters (loss metrics, §12.2).
    #[must_use]
    pub fn stats(&self) -> JitterStats {
        self.stats
    }

    /// The configured target depth in frames.
    #[must_use]
    pub fn target_depth(&self) -> usize {
        self.target_depth
    }

    /// Max packets held at once; on overflow the oldest is evicted (it will
    /// surface as a gap and be concealed).
    fn capacity(&self) -> usize {
        self.target_depth * 3
    }

    /// Feeds one received packet (its `seq` from the audio header and the
    /// codec frame bytes). Late and duplicate packets are dropped.
    pub fn push(&mut self, seq: u32, frame: Vec<u8>) {
        // Late: already played (or already skipped as lost).
        if self.started && seq < self.next_play {
            self.stats.late_dropped += 1;
            return;
        }
        // Duplicate.
        if self.pending.contains_key(&seq) {
            return;
        }
        if self.pending.len() >= self.capacity() {
            let newest = self.pending.keys().next_back().copied();
            if newest.is_some_and(|newest| seq < newest) {
                // Buffer full and this packet is not the newest: drop the
                // incoming one to bound latency.
                return;
            }
            self.pending.pop_first();
        }
        self.pending.insert(seq, frame);
        self.stats.received += 1;
    }

    /// Pops the next frame to decode, in `seq` order. Returns `None` until
    /// the target depth is reached (priming) and after a resync with an
    /// empty buffer (re-priming). Once started, every call yields exactly
    /// one frame — [`JitterOutput::Plc`] for missing ones.
    pub fn pop(&mut self) -> Option<JitterOutput> {
        if !self.started {
            if self.pending.len() < self.target_depth {
                return None;
            }
            self.started = true;
            self.consecutive_plc = 0;
            self.next_play = self.pending.keys().next().copied().expect("non-empty pending");
        }

        if let Some(frame) = self.pending.remove(&self.next_play) {
            self.consecutive_plc = 0;
            self.next_play = self.next_play.wrapping_add(1);
            return Some(JitterOutput::Packet(frame));
        }

        // The expected packet is missing.
        if self.consecutive_plc >= MAX_CONSECUTIVE_PLC {
            // Too much loss in a row: resync. If newer packets are buffered,
            // jump to the oldest of them (the skipped frames are lost for
            // good); otherwise re-prime from silence.
            self.stats.resyncs += 1;
            self.consecutive_plc = 0;
            if let Some((&seq, _)) = self.pending.first_key_value() {
                self.next_play = seq.wrapping_add(1);
                let frame = self.pending.remove(&seq).expect("key from first_key_value");
                return Some(JitterOutput::Packet(frame));
            }
            self.started = false;
            return None;
        }

        self.consecutive_plc += 1;
        self.stats.plc_frames += 1;
        self.next_play = self.next_play.wrapping_add(1);
        Some(JitterOutput::Plc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u32) -> Vec<u8> {
        seq.to_be_bytes().to_vec()
    }

    fn packet_seq(out: &JitterOutput) -> u32 {
        match out {
            JitterOutput::Packet(bytes) => {
                u32::from_be_bytes(bytes[..4].try_into().expect("test frame"))
            }
            JitterOutput::Plc => panic!("expected packet, got Plc"),
        }
    }

    #[test]
    fn not_primed_until_target_depth() {
        let mut jb = JitterBuffer::new();
        jb.push(0, frame(0));
        jb.push(1, frame(1));
        assert_eq!(jb.pop(), None, "2 < target depth 3");
        jb.push(2, frame(2));
        assert_eq!(packet_seq(&jb.pop().unwrap()), 0);
    }

    #[test]
    fn in_order_stream() {
        let mut jb = JitterBuffer::new();
        for seq in 0..3 {
            jb.push(seq, frame(seq));
        }
        for expected in 0..10u32 {
            if expected >= 3 {
                jb.push(expected, frame(expected));
            }
            assert_eq!(packet_seq(&jb.pop().unwrap()), expected);
        }
        assert_eq!(jb.stats().plc_frames, 0);
    }

    #[test]
    fn reordering_within_window_is_absorbed() {
        let mut jb = JitterBuffer::new();
        jb.push(0, frame(0));
        jb.push(1, frame(1));
        jb.push(2, frame(2));
        assert_eq!(packet_seq(&jb.pop().unwrap()), 0);
        // 4 arrives before 3: the depth cushion absorbs the swap.
        jb.push(4, frame(4));
        assert_eq!(packet_seq(&jb.pop().unwrap()), 1);
        jb.push(3, frame(3));
        assert_eq!(packet_seq(&jb.pop().unwrap()), 2);
        assert_eq!(packet_seq(&jb.pop().unwrap()), 3);
        assert_eq!(packet_seq(&jb.pop().unwrap()), 4);
        assert_eq!(jb.stats().plc_frames, 0);
    }

    #[test]
    fn one_lost_frame_produces_exactly_one_plc() {
        let mut jb = JitterBuffer::new();
        for seq in 0..3 {
            jb.push(seq, frame(seq));
        }
        for expected in 0..3 {
            assert_eq!(packet_seq(&jb.pop().unwrap()), expected);
        }
        // 3 is lost; 4, 5, 6 arrive.
        for seq in 4..7 {
            jb.push(seq, frame(seq));
        }
        assert_eq!(jb.pop(), Some(JitterOutput::Plc), "seq 3 is missing");
        for expected in 4..7 {
            assert_eq!(packet_seq(&jb.pop().unwrap()), expected);
        }
        assert_eq!(jb.stats().plc_frames, 1);
    }

    #[test]
    fn late_packet_is_dropped() {
        let mut jb = JitterBuffer::new();
        for seq in 0..3 {
            jb.push(seq, frame(seq));
        }
        for _ in 0..3 {
            jb.pop();
        }
        jb.push(1, frame(1)); // already played
        assert_eq!(jb.stats().late_dropped, 1);
        assert_eq!(jb.stats().received, 3, "late packet not counted as received");
    }

    #[test]
    fn duplicate_packet_is_ignored() {
        let mut jb = JitterBuffer::new();
        for seq in 0..3 {
            jb.push(seq, frame(seq));
        }
        jb.push(1, frame(1));
        assert_eq!(jb.stats().received, 3);
        assert_eq!(jb.stats().late_dropped, 0);
    }

    #[test]
    fn large_gap_resyncs_after_the_plc_cap() {
        let mut jb = JitterBuffer::new();
        for seq in 0..3 {
            jb.push(seq, frame(seq));
        }
        for expected in 0..3 {
            assert_eq!(packet_seq(&jb.pop().unwrap()), expected);
        }
        // Huge jump (e.g. the sender restarted its counter chain).
        for seq in 20..23 {
            jb.push(seq, frame(seq));
        }
        for _ in 0..MAX_CONSECUTIVE_PLC {
            assert_eq!(jb.pop(), Some(JitterOutput::Plc));
        }
        assert_eq!(
            packet_seq(&jb.pop().unwrap()),
            20,
            "resync jumps to the oldest buffered packet"
        );
        assert_eq!(jb.pop().map(|o| packet_seq(&o)), Some(21));
        assert_eq!(jb.stats().plc_frames, MAX_CONSECUTIVE_PLC as u64);
        assert_eq!(jb.stats().resyncs, 1);
    }

    #[test]
    fn total_outage_reprimes() {
        let mut jb = JitterBuffer::new();
        for seq in 0..3 {
            jb.push(seq, frame(seq));
        }
        for _ in 0..3 {
            jb.pop();
        }
        // Nothing more arrives: PLC up to the cap, then the buffer re-primes.
        for _ in 0..MAX_CONSECUTIVE_PLC {
            assert_eq!(jb.pop(), Some(JitterOutput::Plc));
        }
        assert_eq!(jb.pop(), None, "re-priming after the PLC cap");
        // Two packets are not enough to re-prime.
        jb.push(10, frame(10));
        jb.push(11, frame(11));
        assert_eq!(jb.pop(), None);
        jb.push(12, frame(12));
        assert_eq!(packet_seq(&jb.pop().unwrap()), 10);
        assert_eq!(jb.stats().resyncs, 1);
    }

    #[test]
    fn target_depth_is_clamped_to_the_design_range() {
        assert_eq!(JitterBuffer::with_depth(0).target_depth(), MIN_TARGET_DEPTH);
        assert_eq!(JitterBuffer::with_depth(1).target_depth(), MIN_TARGET_DEPTH);
        assert_eq!(JitterBuffer::with_depth(4).target_depth(), MAX_TARGET_DEPTH);
        assert_eq!(JitterBuffer::with_depth(9).target_depth(), MAX_TARGET_DEPTH);
    }

    #[test]
    fn custom_depth_primes_at_its_own_target() {
        let mut jb = JitterBuffer::with_depth(2);
        jb.push(0, frame(0));
        assert_eq!(jb.pop(), None);
        jb.push(1, frame(1));
        assert_eq!(packet_seq(&jb.pop().unwrap()), 0);
    }

    #[test]
    fn overflow_evicts_the_oldest_packet() {
        let mut jb = JitterBuffer::new(); // depth 3, capacity 9
        for seq in 0..9 {
            jb.push(seq, frame(seq));
        }
        jb.push(9, frame(9)); // evicts 0
        assert_eq!(jb.stats().received, 10);
        // Priming starts at the oldest available packet, so the evicted 0
        // never surfaces as a gap.
        assert_eq!(packet_seq(&jb.pop().unwrap()), 1);
        assert_eq!(packet_seq(&jb.pop().unwrap()), 2);
    }
}
