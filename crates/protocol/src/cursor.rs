// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Cursor messages, server → client (design doc §10): the client paints
//! the cursor locally (like RDP), so the server only sends the *shape* and
//! *visibility*, never the position — the position is fixed client-side and
//! injected into the remote session through the input channel.
//!
//! Shapes are sent already converted to RGBA (DXGI delivers monochrome /
//! color XOR+AND masks; converting on the server keeps every client
//! renderer trivial). The wire format is tag byte + big-endian fields,
//! same convention as [`crate::input`].

/// Wire tag for [`CursorMessage::Visibility`].
const TAG_VISIBILITY: u8 = 1;
/// Wire tag for [`CursorMessage::Shape`].
const TAG_SHAPE: u8 = 2;

/// Maximum cursor dimension accepted on the wire ( DXGI pointer shapes are
/// bounded by the driver; 256 covers every realistic hardware cursor).
pub const MAX_CURSOR_DIM: u16 = 256;

/// Errors decoding cursor messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CursorError {
    /// Buffer shorter than the message needs.
    #[error("truncated cursor message")]
    Truncated,
    /// Unknown message tag byte.
    #[error("unknown cursor tag: {0}")]
    UnknownTag(u8),
    /// Shape dimensions exceed [`MAX_CURSOR_DIM`].
    #[error("cursor shape too large: {0}x{1}")]
    TooLarge(u16, u16),
    /// Pixel payload length does not match `width * height * 4`.
    #[error("cursor pixel length mismatch")]
    BadPixelLength,
    /// XOR mask length is neither 0 nor `ceil(width * height / 8)`.
    #[error("cursor xor-mask length mismatch")]
    BadMaskLength,
}

/// One cursor state update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorMessage {
    /// Cursor became visible or hidden on the remote desktop.
    Visibility {
        /// True when the remote cursor is visible.
        visible: bool,
    },
    /// New cursor shape, as 32-bit RGBA (one byte per channel, row-major).
    Shape {
        /// Width in pixels.
        width: u16,
        /// Height in pixels.
        height: u16,
        /// Hotspot X offset inside the image.
        hot_x: u16,
        /// Hotspot Y offset inside the image.
        hot_y: u16,
        /// RGBA pixels, `width * height * 4` bytes. XOR pixels (see
        /// `xor_mask`) carry their source RGB with alpha 255 so consumers
        /// without XOR support still draw something sensible.
        pixels: Vec<u8>,
        /// 1bpp mask over the pixels (MSB first, row-major,
        /// `ceil(width*height/8)` bytes): a set bit marks the pixel as
        /// XOR-with-background (DXGI masked-color semantics, §10). Empty
        /// when no pixel XORs.
        xor_mask: Vec<u8>,
    },
}

impl CursorMessage {
    /// Encodes the message, appending to `out` (allows batching).
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::Visibility { visible } => {
                out.push(TAG_VISIBILITY);
                out.push(u8::from(*visible));
            }
            Self::Shape {
                width,
                height,
                hot_x,
                hot_y,
                pixels,
                xor_mask,
            } => {
                out.push(TAG_SHAPE);
                out.extend_from_slice(&width.to_be_bytes());
                out.extend_from_slice(&height.to_be_bytes());
                out.extend_from_slice(&hot_x.to_be_bytes());
                out.extend_from_slice(&hot_y.to_be_bytes());
                // Bounded in practice: decode rejects dims over
                // MAX_CURSOR_DIM, so a well-formed shape is ≤ 256 KiB.
                out.extend_from_slice(
                    &u32::try_from(pixels.len()).unwrap_or(u32::MAX).to_be_bytes(),
                );
                out.extend_from_slice(pixels);
                // Trailing XOR mask (absent in pre-XOR encoders = no XOR
                // pixels); one message per channel packet, so the tail is
                // unambiguous.
                out.extend_from_slice(
                    &u32::try_from(xor_mask.len()).unwrap_or(u32::MAX).to_be_bytes(),
                );
                out.extend_from_slice(xor_mask);
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
    /// Returns [`CursorError`] on truncation, unknown tags or inconsistent
    /// shape payloads.
    pub fn decode(buf: &[u8]) -> Result<Self, CursorError> {
        let (&tag, rest) = buf.split_first().ok_or(CursorError::Truncated)?;
        match tag {
            TAG_VISIBILITY => {
                let (&v, _) = rest.split_first().ok_or(CursorError::Truncated)?;
                Ok(Self::Visibility { visible: v != 0 })
            }
            TAG_SHAPE => {
                if rest.len() < 12 {
                    return Err(CursorError::Truncated);
                }
                let width = u16::from_be_bytes([rest[0], rest[1]]);
                let height = u16::from_be_bytes([rest[2], rest[3]]);
                let hot_x = u16::from_be_bytes([rest[4], rest[5]]);
                let hot_y = u16::from_be_bytes([rest[6], rest[7]]);
                let len = u32::from_be_bytes([rest[8], rest[9], rest[10], rest[11]]) as usize;
                if width > MAX_CURSOR_DIM || height > MAX_CURSOR_DIM {
                    return Err(CursorError::TooLarge(width, height));
                }
                if len != usize::from(width) * usize::from(height) * 4 {
                    return Err(CursorError::BadPixelLength);
                }
                let pixels = rest.get(12..12 + len).ok_or(CursorError::Truncated)?.to_vec();
                // Optional trailing XOR mask: absent means "no XOR pixels"
                // (older encoders); present, it must cover the pixels 1bpp.
                let tail = rest.get(12 + len..).unwrap_or(&[]);
                let xor_mask = if tail.len() >= 4 {
                    let mlen = u32::from_be_bytes([tail[0], tail[1], tail[2], tail[3]]) as usize;
                    let expected = (usize::from(width) * usize::from(height)).div_ceil(8);
                    if mlen != 0 && mlen != expected {
                        return Err(CursorError::BadMaskLength);
                    }
                    tail.get(4..4 + mlen).ok_or(CursorError::Truncated)?.to_vec()
                } else {
                    Vec::new()
                };
                Ok(Self::Shape {
                    width,
                    height,
                    hot_x,
                    hot_y,
                    pixels,
                    xor_mask,
                })
            }
            other => Err(CursorError::UnknownTag(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_roundtrip() {
        for visible in [true, false] {
            let msg = CursorMessage::Visibility { visible };
            assert_eq!(CursorMessage::decode(&msg.encode()), Ok(msg));
        }
    }

    #[test]
    fn shape_roundtrip() {
        let msg = CursorMessage::Shape {
            width: 2,
            height: 3,
            hot_x: 1,
            hot_y: 0,
            pixels: (0..24u8).collect(),
            xor_mask: Vec::new(),
        };
        assert_eq!(CursorMessage::decode(&msg.encode()), Ok(msg));
    }

    #[test]
    fn shape_with_xor_mask_roundtrip() {
        // 2x3 = 6 pixels -> 1 mask byte.
        let msg = CursorMessage::Shape {
            width: 2,
            height: 3,
            hot_x: 1,
            hot_y: 0,
            pixels: (0..24u8).collect(),
            xor_mask: vec![0b1010_0000],
        };
        assert_eq!(CursorMessage::decode(&msg.encode()), Ok(msg));
    }

    #[test]
    fn shape_without_mask_tail_decodes_with_empty_mask() {
        // Pre-XOR encoders stop after the pixels.
        let msg = CursorMessage::Shape {
            width: 2,
            height: 3,
            hot_x: 1,
            hot_y: 0,
            pixels: (0..24u8).collect(),
            xor_mask: Vec::new(),
        };
        let mut buf = msg.encode();
        buf.truncate(buf.len() - 4); // drop the u32 mask length
        assert_eq!(CursorMessage::decode(&buf), Ok(msg));
    }

    #[test]
    fn bad_xor_mask_length_is_rejected() {
        let msg = CursorMessage::Shape {
            width: 2,
            height: 3,
            hot_x: 1,
            hot_y: 0,
            pixels: (0..24u8).collect(),
            xor_mask: vec![0; 2], // 6 pixels need exactly 1 byte
        };
        assert_eq!(
            CursorMessage::decode(&msg.encode()),
            Err(CursorError::BadMaskLength)
        );
    }

    #[test]
    fn empty_buffer_is_truncated() {
        assert_eq!(CursorMessage::decode(&[]), Err(CursorError::Truncated));
    }

    #[test]
    fn unknown_tag_is_rejected() {
        assert_eq!(CursorMessage::decode(&[99]), Err(CursorError::UnknownTag(99)));
    }

    #[test]
    fn oversized_shape_is_rejected() {
        let mut buf = vec![TAG_SHAPE];
        buf.extend_from_slice(&300u16.to_be_bytes());
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes());
        buf.extend_from_slice(&1200u32.to_be_bytes());
        buf.extend_from_slice(&[0u8; 1200]);
        assert_eq!(CursorMessage::decode(&buf), Err(CursorError::TooLarge(300, 1)));
    }

    #[test]
    fn mismatched_pixel_length_is_rejected() {
        let mut buf = vec![TAG_SHAPE];
        buf.extend_from_slice(&2u16.to_be_bytes());
        buf.extend_from_slice(&2u16.to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes());
        buf.extend_from_slice(&15u32.to_be_bytes()); // should be 16
        buf.extend_from_slice(&[0u8; 15]);
        assert_eq!(CursorMessage::decode(&buf), Err(CursorError::BadPixelLength));
    }

    #[test]
    fn truncated_shape_payload_is_rejected() {
        let msg = CursorMessage::Shape {
            width: 4,
            height: 4,
            hot_x: 0,
            hot_y: 0,
            pixels: vec![0; 64],
            xor_mask: Vec::new(),
        };
        let buf = msg.encode();
        // Cut inside the pixel payload (the 4-byte empty-mask tail comes last).
        assert_eq!(
            CursorMessage::decode(&buf[..buf.len() - 5]),
            Err(CursorError::Truncated)
        );
        // A cut mask tail is rejected when the length word survives.
        let msg = CursorMessage::Shape {
            width: 4,
            height: 4,
            hot_x: 0,
            hot_y: 0,
            pixels: vec![0; 64],
            xor_mask: vec![0xAB, 0xCD],
        };
        let buf = msg.encode();
        assert_eq!(CursorMessage::decode(&buf[..buf.len() - 1]), Err(CursorError::Truncated));
    }
}
