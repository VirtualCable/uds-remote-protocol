// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Frame fragmentation and reassembly (design doc §6.2).
//!
//! A video frame may exceed the MTU, so it is split into self-contained
//! datagrams, each carrying:
//!
//! ```text
//! | display_id (1B) | frame_id (3B BE) | frag_idx (2B BE) | frag_count (2B BE) | capture_ts_ms (4B BE) | chunk ... |
//! ```
//!
//! - `display_id` is reserved for future multi-monitor (0 in phase 1).
//! - `frame_id` is a 24-bit counter (wraps at 2^24).
//! - `capture_ts_ms` is the frame's capture timestamp on the server's shared
//!   session clock (ms), carried by every fragment for A/V lip-sync (§9.1).
//! - If any fragment is missing, the whole frame is dropped: a half frame
//!   never reaches the decoder.
//! - Payload per fragment is capped so the full datagram stays within the
//!   IPv6-safe MTU budget (see [`fragment_payload_max`]).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::error::ProtocolError;

/// Fragment header size in bytes.
pub const HEADER_SIZE: usize = 12;

/// Hard cap on datagram plaintext (must match `crypt::MAX_DATAGRAM_PAYLOAD`
/// — kept as a literal so `protocol` does not depend on `crypt`).
const MAX_DATAGRAM_PLAINTEXT: usize = 1180;

/// Maximum fragments accepted per frame (~9.5 MiB of payload): a frame
/// bigger than that is a mis-parse or an attack, not a real video frame.
const MAX_FRAME_FRAGMENTS: u16 = 8192;

/// Maximum payload (chunk) bytes per fragment.
#[must_use]
pub const fn fragment_payload_max() -> usize {
    MAX_DATAGRAM_PLAINTEXT - HEADER_SIZE
}

/// Stateless element splitter.
pub struct Fragmenter;

impl Fragmenter {
    /// Splits `element` (one encoded frame) into wire fragments for
    /// `display_id`/`frame_id`, all stamped with `capture_ts_ms` (ms on the
    /// server's session clock, §9.1). `frame_id` is truncated to 24 bits.
    #[must_use]
    pub fn split(display_id: u8, frame_id: u32, capture_ts_ms: u32, element: &[u8]) -> Vec<Vec<u8>> {
        let chunk = fragment_payload_max();
        let count = element.len().div_ceil(chunk).max(1);
        let frame_id = frame_id & 0x00FF_FFFF;
        let count16 = u16::try_from(count).expect("fragment count fits u16");

        (0..count)
            .map(|idx| {
                let start = idx * chunk;
                let end = (start + chunk).min(element.len());
                let mut frag = Vec::with_capacity(HEADER_SIZE + (end - start));
                frag.push(display_id);
                frag.extend_from_slice(&frame_id.to_be_bytes()[1..]);
                frag.extend_from_slice(&u16::try_from(idx).expect("idx < count ≤ u16").to_be_bytes());
                frag.extend_from_slice(&count16.to_be_bytes());
                frag.extend_from_slice(&capture_ts_ms.to_be_bytes());
                frag.extend_from_slice(&element[start..end]);
                frag
            })
            .collect()
    }
}

/// A fully reassembled frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReassembledFrame {
    /// Display the frame belongs to (0 in phase 1).
    pub display_id: u8,
    /// 24-bit frame counter.
    pub frame_id: u32,
    /// Capture timestamp (ms on the server's session clock) for A/V
    /// lip-sync (§9.1).
    pub capture_ts_ms: u32,
    /// The complete element (encoded frame).
    pub data: Vec<u8>,
}

struct Partial {
    count: u16,
    received: u16,
    total_len: usize,
    capture_ts_ms: u32,
    buf: Vec<u8>,
    present: Vec<bool>,
    /// Last fragment arrival; drives the clock-based eviction
    /// ([`Reassembler::evict_stale`]) that complements the window one.
    touched: Instant,
}

/// Reassembles fragments into complete frames, dropping incomplete ones.
///
/// Tracks a sliding window of recent frame ids; stale partial frames are
/// evicted when newer frames arrive (§6.2) or when they go untouched for too
/// long ([`Reassembler::evict_stale`], for losses right before a
/// static-screen period where no newer frame ever arrives). Evicted
/// incomplete frames are counted ([`Reassembler::dropped_frames`]) so the
/// receiver can ask for a keyframe when loss makes the stream
/// undecodable (§12.2).
pub struct Reassembler {
    partials: HashMap<(u8, u32), Partial>,
    /// Highest frame id seen so far (per display); drives eviction.
    max_seen: HashMap<u8, u32>,
    /// Frames evicted incomplete (their remaining fragments never came).
    dropped: u64,
}

/// How far behind the newest frame a partial may lag before eviction.
const TRACK_WINDOW: u32 = 64;

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    /// Creates an empty reassembler.
    #[must_use]
    pub fn new() -> Self {
        Self {
            partials: HashMap::new(),
            max_seen: HashMap::new(),
            dropped: 0,
        }
    }

    /// Frames evicted incomplete so far (cumulative). A raise means the
    /// decoder may be missing a delta frame: predictive codecs need a
    /// keyframe to recover (§12.2).
    #[must_use]
    pub fn dropped_frames(&self) -> u64 {
        self.dropped
    }

    /// Evicts partials untouched for `max_age`, counting them as dropped.
    ///
    /// The window eviction only fires when a *newer* frame arrives, so a
    /// loss right before a static-screen period (rect codecs send nothing
    /// while nothing changes) would go unnoticed until the next change —
    /// leaving a stale region on screen meanwhile. The receiver calls this
    /// on a timer so loss detection never depends on future traffic.
    pub fn evict_stale(&mut self, max_age: Duration) -> u64 {
        let mut dropped_now = 0u64;
        self.partials.retain(|_, p| {
            let keep = p.touched.elapsed() < max_age;
            dropped_now += u64::from(!keep);
            keep
        });
        self.dropped += dropped_now;
        dropped_now
    }

    /// True if `frame_id` (display 0) has all its fragments.
    #[must_use]
    pub fn is_frame_complete(&self, frame_id: u32) -> bool {
        self.partials
            .get(&(0, frame_id))
            .is_some_and(|p| p.received == p.count)
    }

    /// True if `frame_id` (display 0) is still being tracked.
    #[must_use]
    pub fn tracks_frame(&self, frame_id: u32) -> bool {
        self.partials.contains_key(&(0, frame_id))
    }

    /// Feeds one wire fragment. Returns the complete frame when its last
    /// missing fragment arrives; `None` while incomplete (or for duplicates).
    pub fn push(&mut self, frag: &[u8]) -> Result<Option<ReassembledFrame>, ProtocolError> {
        if frag.len() < HEADER_SIZE {
            return Err(ProtocolError::Truncated);
        }
        let display_id = frag[0];
        let frame_id = u32::from_be_bytes([0, frag[1], frag[2], frag[3]]);
        let idx = u16::from_be_bytes(frag[4..6].try_into().expect("2-byte slice"));
        let count = u16::from_be_bytes(frag[6..8].try_into().expect("2-byte slice"));
        let capture_ts_ms =
            u32::from_be_bytes(frag[8..12].try_into().expect("4-byte slice"));
        let chunk = &frag[HEADER_SIZE..];
        if count == 0
            || count > MAX_FRAME_FRAGMENTS
            || idx >= count
            || chunk.len() > fragment_payload_max()
        {
            return Err(ProtocolError::Truncated);
        }

        // Evict partials left too far behind (their remaining fragments are
        // assumed lost; a half frame is never delivered).
        let newest = self.max_seen.entry(display_id).or_insert(frame_id);
        if frame_id >= *newest {
            *newest = frame_id;
        }
        let horizon = newest.saturating_sub(TRACK_WINDOW);
        let mut dropped_now = 0u64;
        self.partials.retain(|&(d, f), p| {
            let keep = d != display_id || f >= horizon;
            if !keep && p.received < p.count {
                dropped_now += 1;
            }
            keep
        });
        self.dropped += dropped_now;

        let key = (display_id, frame_id);
        let entry = self.partials.entry(key).or_insert_with(|| Partial {
            count,
            received: 0,
            total_len: 0,
            capture_ts_ms,
            buf: vec![0u8; fragment_payload_max() * count as usize],
            present: vec![false; count as usize],
            touched: Instant::now(),
        });
        if entry.count != count || entry.capture_ts_ms != capture_ts_ms {
            // Inconsistent restatement of the same frame: drop it.
            self.partials.remove(&key);
            return Err(ProtocolError::Truncated);
        }
        if entry.present[idx as usize] {
            return Ok(None); // duplicate fragment
        }
        entry.touched = Instant::now();

        let offset = idx as usize * fragment_payload_max();
        entry.buf[offset..offset + chunk.len()].copy_from_slice(chunk);
        entry.present[idx as usize] = true;
        entry.received += 1;
        entry.total_len += chunk.len();

        if entry.received == entry.count {
            let p = self.partials.remove(&key).expect("present above");
            let mut data = p.buf;
            data.truncate(p.total_len);
            return Ok(Some(ReassembledFrame {
                display_id,
                frame_id,
                capture_ts_ms,
                data,
            }));
        }
        Ok(None)
    }
}
