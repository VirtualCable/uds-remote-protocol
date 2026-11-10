// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Clipboard messages, bidirectional (M8). Lazy pull model, a simplified
//! cliprdr: the side whose clipboard changed announces the available
//! formats with [`ClipboardMessage::Offer`]; the peer pulls the one it wants
//! with `Request` and gets `Data` (or `DataFailed`). Text travels as UTF-8;
//! the format bitmask leaves room for richer formats (PNG, files) without
//! wire changes. The wire format is tag byte + big-endian fields, same
//! convention as [`crate::input`] and [`crate::cursor`].

/// Wire tag for [`ClipboardMessage::Offer`].
const TAG_OFFER: u8 = 1;
/// Wire tag for [`ClipboardMessage::Request`].
const TAG_REQUEST: u8 = 2;
/// Wire tag for [`ClipboardMessage::Data`].
const TAG_DATA: u8 = 3;
/// Wire tag for [`ClipboardMessage::DataFailed`].
const TAG_DATA_FAILED: u8 = 4;

/// Format bit for plain text (UTF-8 on the wire).
pub const FORMAT_TEXT: u32 = 1;

/// Maximum payload accepted on the wire (both directions). Generous for
/// text; richer formats will get their own per-format caps when added.
pub const MAX_CLIPBOARD_PAYLOAD: u32 = 8 * 1024 * 1024;

/// Errors decoding clipboard messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ClipboardError {
    /// Buffer shorter than the message needs.
    #[error("truncated clipboard message")]
    Truncated,
    /// Unknown message tag byte.
    #[error("unknown clipboard tag: {0}")]
    UnknownTag(u8),
    /// Payload exceeds [`MAX_CLIPBOARD_PAYLOAD`].
    #[error("clipboard payload too large: {0}")]
    PayloadTooLarge(u32),
    /// Format bitmask is empty or holds no known format.
    #[error("clipboard offer without known formats: 0x{0:x}")]
    NoKnownFormat(u32),
}

/// One clipboard channel message (either direction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardMessage {
    /// The sender's clipboard changed; `formats` is a bitmask with at least
    /// one known bit set. `seq` identifies this clipboard generation:
    /// requests and data carry it back so stale generations can be dropped.
    Offer {
        /// Clipboard generation identifier.
        seq: u32,
        /// Bitmask of available formats (`FORMAT_TEXT`, ...).
        formats: u32,
    },
    /// Pull one format of a previously announced generation.
    Request {
        /// Generation the request refers to.
        seq: u32,
        /// Requested format bit.
        format: u32,
    },
    /// Format data for a request.
    Data {
        /// Generation the data belongs to.
        seq: u32,
        /// Format bit of the payload.
        format: u32,
        /// Raw format bytes (UTF-8 text for `FORMAT_TEXT`).
        payload: Vec<u8>,
    },
    /// The requested data could not be produced (clipboard changed under
    /// us, format gone, access denied...).
    DataFailed {
        /// Generation of the failed request.
        seq: u32,
    },
}

impl ClipboardMessage {
    /// Encodes the message, appending to `out` (allows batching).
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::Offer { seq, formats } => {
                out.push(TAG_OFFER);
                out.extend_from_slice(&seq.to_be_bytes());
                out.extend_from_slice(&formats.to_be_bytes());
            }
            Self::Request { seq, format } => {
                out.push(TAG_REQUEST);
                out.extend_from_slice(&seq.to_be_bytes());
                out.extend_from_slice(&format.to_be_bytes());
            }
            Self::Data {
                seq,
                format,
                payload,
            } => {
                out.push(TAG_DATA);
                out.extend_from_slice(&seq.to_be_bytes());
                out.extend_from_slice(&format.to_be_bytes());
                out.extend_from_slice(
                    &u32::try_from(payload.len())
                        .unwrap_or(u32::MAX)
                        .to_be_bytes(),
                );
                out.extend_from_slice(payload);
            }
            Self::DataFailed { seq } => {
                out.push(TAG_DATA_FAILED);
                out.extend_from_slice(&seq.to_be_bytes());
            }
        }
    }

    /// Encodes the message into a fresh buffer.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// Decodes one message from the front of `buf`.
    ///
    /// # Errors
    /// Returns [`ClipboardError`] on truncation, unknown tags, oversized
    /// payloads or offers without any known format.
    pub fn decode(buf: &[u8]) -> Result<Self, ClipboardError> {
        let (&tag, rest) = buf.split_first().ok_or(ClipboardError::Truncated)?;
        match tag {
            TAG_OFFER => {
                if rest.len() < 8 {
                    return Err(ClipboardError::Truncated);
                }
                let seq = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
                let formats = u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]);
                if formats & FORMAT_TEXT == 0 {
                    return Err(ClipboardError::NoKnownFormat(formats));
                }
                Ok(Self::Offer { seq, formats })
            }
            TAG_REQUEST => {
                if rest.len() < 8 {
                    return Err(ClipboardError::Truncated);
                }
                let seq = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
                let format = u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]);
                Ok(Self::Request { seq, format })
            }
            TAG_DATA => {
                if rest.len() < 12 {
                    return Err(ClipboardError::Truncated);
                }
                let seq = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
                let format = u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]);
                let len = u32::from_be_bytes([rest[8], rest[9], rest[10], rest[11]]);
                if len > MAX_CLIPBOARD_PAYLOAD {
                    return Err(ClipboardError::PayloadTooLarge(len));
                }
                let payload = rest
                    .get(12..12 + len as usize)
                    .ok_or(ClipboardError::Truncated)?
                    .to_vec();
                Ok(Self::Data {
                    seq,
                    format,
                    payload,
                })
            }
            TAG_DATA_FAILED => {
                if rest.len() < 4 {
                    return Err(ClipboardError::Truncated);
                }
                let seq = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
                Ok(Self::DataFailed { seq })
            }
            other => Err(ClipboardError::UnknownTag(other)),
        }
    }
}

/// Symmetric clipboard session state (both peers run the same logic):
/// tracks our clipboard generation, the peer's last announced generation
/// and a cache of our current text so `Request`s are answered without a
/// second OS read. Pure and synchronous; the platform backend only feeds
/// local changes and applies remote sets.
#[derive(Debug, Default)]
pub struct ClipboardSession {
    /// Our clipboard generation counter (bumped on every local change).
    local_seq: u32,
    /// Peer's last generation we have seen (offers older than this are stale).
    peer_seq: Option<u32>,
    /// Peer generation whose data we are expecting (one pull in flight).
    pending_pull: Option<u32>,
    /// Cache of our current clipboard text (for answering `Request`).
    local_text: Option<Vec<u8>>,
}

impl ClipboardSession {
    /// Fresh session with empty clipboard state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Our clipboard changed: cache the text and produce the `Offer` to send.
    pub fn local_text_changed(&mut self, text: &[u8]) -> ClipboardMessage {
        self.local_seq = self.local_seq.wrapping_add(1);
        self.local_text = Some(text.to_vec());
        ClipboardMessage::Offer {
            seq: self.local_seq,
            formats: FORMAT_TEXT,
        }
    }

    /// Peer's clipboard changed: pull the text format, unless the offer is
    /// stale (a newer generation already arrived) or already pulled.
    pub fn peer_offer(&mut self, seq: u32, formats: u32) -> Option<ClipboardMessage> {
        if formats & FORMAT_TEXT == 0 {
            return None;
        }
        if self
            .peer_seq
            .is_some_and(|seen| seq.wrapping_sub(seen).cast_signed() <= 0)
        {
            return None; // stale or already seen generation
        }
        self.peer_seq = Some(seq);
        self.pending_pull = Some(seq);
        Some(ClipboardMessage::Request {
            seq,
            format: FORMAT_TEXT,
        })
    }

    /// Peer asks for our data: answer from the cache when the generation
    /// still matches, otherwise fail (clipboard moved on).
    #[must_use]
    pub fn peer_request(&self, seq: u32, format: u32) -> ClipboardMessage {
        match (&self.local_text, format) {
            (Some(text), f) if seq == self.local_seq && f == FORMAT_TEXT => {
                ClipboardMessage::Data {
                    seq,
                    format,
                    payload: text.clone(),
                }
            }
            _ => ClipboardMessage::DataFailed { seq },
        }
    }

    /// Peer sent data: accept it only if it answers our pending pull.
    /// Returns the payload to apply to the local clipboard.
    pub fn peer_data(&mut self, seq: u32, format: u32, payload: &[u8]) -> Option<Vec<u8>> {
        if self.pending_pull == Some(seq) && format == FORMAT_TEXT {
            self.pending_pull = None;
            Some(payload.to_vec())
        } else {
            None
        }
    }

    /// Peer's pull failed: clear the pending state.
    pub fn peer_data_failed(&mut self, seq: u32) {
        if self.pending_pull == Some(seq) {
            self.pending_pull = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_roundtrip() {
        let msg = ClipboardMessage::Offer {
            seq: 7,
            formats: FORMAT_TEXT,
        };
        assert_eq!(ClipboardMessage::decode(&msg.encode()), Ok(msg));
    }

    #[test]
    fn request_roundtrip() {
        let msg = ClipboardMessage::Request {
            seq: 42,
            format: FORMAT_TEXT,
        };
        assert_eq!(ClipboardMessage::decode(&msg.encode()), Ok(msg));
    }

    #[test]
    fn data_roundtrip() {
        let msg = ClipboardMessage::Data {
            seq: 3,
            format: FORMAT_TEXT,
            payload: "hola, portapapeles".as_bytes().to_vec(),
        };
        assert_eq!(ClipboardMessage::decode(&msg.encode()), Ok(msg));
    }

    #[test]
    fn data_failed_roundtrip() {
        let msg = ClipboardMessage::DataFailed { seq: 9 };
        assert_eq!(ClipboardMessage::decode(&msg.encode()), Ok(msg));
    }

    #[test]
    fn empty_buffer_is_truncated() {
        assert_eq!(
            ClipboardMessage::decode(&[]),
            Err(ClipboardError::Truncated)
        );
    }

    #[test]
    fn unknown_tag_is_rejected() {
        assert_eq!(
            ClipboardMessage::decode(&[99]),
            Err(ClipboardError::UnknownTag(99))
        );
    }

    #[test]
    fn offer_without_known_format_is_rejected() {
        let mut buf = vec![TAG_OFFER];
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(&0x8000u32.to_be_bytes());
        assert_eq!(
            ClipboardMessage::decode(&buf),
            Err(ClipboardError::NoKnownFormat(0x8000))
        );
    }

    #[test]
    fn oversized_payload_is_rejected() {
        let mut buf = vec![TAG_DATA];
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(&FORMAT_TEXT.to_be_bytes());
        buf.extend_from_slice(&(MAX_CLIPBOARD_PAYLOAD + 1).to_be_bytes());
        assert_eq!(
            ClipboardMessage::decode(&buf),
            Err(ClipboardError::PayloadTooLarge(MAX_CLIPBOARD_PAYLOAD + 1))
        );
    }

    #[test]
    fn truncated_data_payload_is_rejected() {
        let msg = ClipboardMessage::Data {
            seq: 1,
            format: FORMAT_TEXT,
            payload: vec![0; 64],
        };
        let buf = msg.encode();
        assert_eq!(
            ClipboardMessage::decode(&buf[..buf.len() - 1]),
            Err(ClipboardError::Truncated)
        );
    }

    #[test]
    fn session_full_pull_cycle() {
        let mut server = ClipboardSession::new();
        let mut client = ClipboardSession::new();

        // Server side copies text: offer flows to the client.
        let offer = server.local_text_changed(b"hola");
        let ClipboardMessage::Offer { seq, formats } = offer else {
            panic!("expected offer");
        };
        // Client pulls.
        let request = client.peer_offer(seq, formats).expect("should pull");
        let ClipboardMessage::Request { seq: rseq, format } = request else {
            panic!("expected request");
        };
        // Server answers from cache, client accepts the data.
        let data = server.peer_request(rseq, format);
        let ClipboardMessage::Data {
            seq: dseq,
            format: dformat,
            payload,
        } = data
        else {
            panic!("expected data");
        };
        assert_eq!(
            client.peer_data(dseq, dformat, &payload),
            Some(b"hola".to_vec())
        );
    }

    #[test]
    fn session_stale_offer_is_ignored() {
        let mut s = ClipboardSession::new();
        assert!(s.peer_offer(5, FORMAT_TEXT).is_some());
        assert!(s.peer_offer(5, FORMAT_TEXT).is_none()); // duplicate
        assert!(s.peer_offer(4, FORMAT_TEXT).is_none()); // older
        assert!(s.peer_offer(6, FORMAT_TEXT).is_some()); // newer
    }

    #[test]
    fn session_request_mismatch_fails() {
        let mut s = ClipboardSession::new();
        let _ = s.local_text_changed(b"x");
        // Wrong generation: clipboard moved on.
        assert_eq!(
            s.peer_request(99, FORMAT_TEXT),
            ClipboardMessage::DataFailed { seq: 99 }
        );
        // Unknown format: not servable.
        let ClipboardMessage::Offer { seq, .. } = s.local_text_changed(b"y") else {
            panic!()
        };
        assert_eq!(
            s.peer_request(seq, 0x8000),
            ClipboardMessage::DataFailed { seq }
        );
    }

    #[test]
    fn session_data_without_pull_is_dropped() {
        let mut s = ClipboardSession::new();
        assert_eq!(s.peer_data(1, FORMAT_TEXT, b"unsolicited"), None);
        // And a failed pull clears the pending state.
        let _ = s.peer_offer(3, FORMAT_TEXT);
        s.peer_data_failed(3);
        assert_eq!(s.peer_data(3, FORMAT_TEXT, b"late"), None);
    }
}
