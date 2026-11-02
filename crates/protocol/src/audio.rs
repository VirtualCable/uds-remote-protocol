// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Audio datagram payload format (design doc §9).
//!
//! One self-contained codec frame per datagram, never fragmented:
//!
//! ```text
//! | seq (4B BE) | capture_ts_ms (4B BE) | codec frame bytes... |
//! ```
//!
//! - `seq`: per-channel sender counter for the receiver's jitter buffer
//!   (reorder and loss detection, §9.1).
//! - `capture_ts_ms`: server capture timestamp (ms) for A/V lip-sync
//!   (§9.1, riesgos.md §4.2).
//!
//! The datagram plaintext budget (1180 bytes, §6.2) always fits one Opus
//! frame (≤ 1275 bytes worst case, far less at 20 ms / ≤ 96 kbps), so there
//! is no fragmentation layer here — unlike video.

use crate::error::ProtocolError;

/// Audio datagram header size in bytes (`seq` + `capture_ts_ms`).
pub const HEADER_SIZE: usize = 8;

/// Header of one audio datagram payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    /// Per-channel sender counter, monotonically increasing by 1 per frame.
    pub seq: u32,
    /// Server capture timestamp in milliseconds (A/V lip-sync, §9.1).
    pub capture_ts_ms: u32,
}

/// Encodes one audio datagram payload, appending to `out` (allows the
/// caller to reuse a scratch buffer in the hot path).
pub fn encode_into(header: AudioHeader, frame: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&header.seq.to_be_bytes());
    out.extend_from_slice(&header.capture_ts_ms.to_be_bytes());
    out.extend_from_slice(frame);
}

/// Encodes one audio datagram payload into a fresh buffer.
#[must_use]
pub fn encode(header: AudioHeader, frame: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_SIZE + frame.len());
    encode_into(header, frame, &mut out);
    out
}

/// Decodes one audio datagram payload, returning the header and a view of
/// the codec frame bytes (borrowed from `buf`).
///
/// # Errors
/// Returns [`ProtocolError::Truncated`] if the buffer cannot hold the
/// header, and [`ProtocolError::Malformed`] if the frame is empty (a
/// datagram always carries exactly one codec frame).
pub fn decode(buf: &[u8]) -> Result<(AudioHeader, &[u8]), ProtocolError> {
    if buf.len() < HEADER_SIZE {
        return Err(ProtocolError::Truncated);
    }
    let header = AudioHeader {
        seq: u32::from_be_bytes(buf[0..4].try_into().expect("4-byte slice")),
        capture_ts_ms: u32::from_be_bytes(buf[4..8].try_into().expect("4-byte slice")),
    };
    let frame = &buf[HEADER_SIZE..];
    if frame.is_empty() {
        return Err(ProtocolError::Malformed("empty audio frame"));
    }
    Ok((header, frame))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let header = AudioHeader {
            seq: 0x0102_0304,
            capture_ts_ms: 0xA0B0_C0D0,
        };
        let frame = [1, 2, 3, 250];
        let buf = encode(header, &frame);
        assert_eq!(buf.len(), HEADER_SIZE + frame.len());
        let (decoded, decoded_frame) = decode(&buf).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(decoded_frame, frame);
    }

    #[test]
    fn wire_layout_is_big_endian() {
        let buf = encode(
            AudioHeader {
                seq: 1,
                capture_ts_ms: 1000,
            },
            &[0xFF],
        );
        assert_eq!(
            buf,
            [0, 0, 0, 1, 0, 0, 0x03, 0xE8, 0xFF],
            "seq and capture_ts_ms are 4-byte big-endian"
        );
    }

    #[test]
    fn encode_into_appends() {
        let mut out = vec![0xAA];
        encode_into(
            AudioHeader {
                seq: 7,
                capture_ts_ms: 42,
            },
            &[1, 2],
            &mut out,
        );
        assert_eq!(out.len(), 1 + HEADER_SIZE + 2);
        assert_eq!(out[0], 0xAA);
    }

    #[test]
    fn truncated_header_is_rejected() {
        for len in 0..HEADER_SIZE {
            assert_eq!(decode(&vec![0u8; len]), Err(ProtocolError::Truncated));
        }
    }

    #[test]
    fn empty_frame_is_rejected() {
        let buf = encode(
            AudioHeader {
                seq: 0,
                capture_ts_ms: 0,
            },
            &[],
        );
        assert_eq!(
            decode(&buf),
            Err(ProtocolError::Malformed("empty audio frame"))
        );
    }
}
