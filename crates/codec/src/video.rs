// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Video codec traits and common types (design doc §8).
//!
//! The protocol negotiates a codec via capabilities; the core only sees
//! these traits. The trait is codec-agnostic (VP9 today; VP8, H.264 or AV1
//! tomorrow) and backend-agnostic (software or hardware): the negotiated
//! capability announces `codec + backend`.

/// Video codec identifier, with stable wire IDs for capability negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodecType {
    /// Uncompressed BGRA (reference implementation for tests; never
    /// negotiated in production).
    Raw,
    /// VP8 (libvpx; low CPU cost, worse ratio).
    Vp8,
    /// VP9 (libvpx; better ratio, more CPU).
    Vp9,
    /// H.264 (e.g. Cisco OpenH264, loaded dynamically — never redistributed).
    H264,
    /// AV1 (future).
    Av1,
    /// JPEG over dirty rects (libjpeg-turbo; base regime of the codec
    /// ladder, §8.5).
    JpegRects,
}

impl VideoCodecType {
    /// Stable numeric ID for the wire (capability negotiation).
    #[must_use]
    pub fn wire_id(self) -> u16 {
        match self {
            Self::Raw => 0,
            Self::Vp8 => 1,
            Self::Vp9 => 2,
            Self::H264 => 3,
            Self::Av1 => 4,
            Self::JpegRects => 5,
        }
    }

    /// Inverse of [`VideoCodecType::wire_id`]; `None` for unknown IDs.
    #[must_use]
    pub fn from_wire_id(id: u16) -> Option<Self> {
        match id {
            0 => Some(Self::Raw),
            1 => Some(Self::Vp8),
            2 => Some(Self::Vp9),
            3 => Some(Self::H264),
            4 => Some(Self::Av1),
            5 => Some(Self::JpegRects),
            _ => None,
        }
    }
}

/// Cheap keyframe sniffing of an encoded packet, for broken-chain drop
/// decisions (client): self-contained codecs always count as keyframes.
#[must_use]
pub fn packet_is_keyframe(codec: VideoCodecType, data: &[u8]) -> bool {
    match codec {
        VideoCodecType::Vp9 => {
            // VP9 uncompressed header, MSB-first in the first byte:
            // frame_marker(2, must be 0b10), profile(2), reserved(1),
            // frame_type(1, 0 = keyframe), show_frame(1),
            // error_resilient(1) — 8 bits, so the keyframe sync code
            // (49 83 42) lands byte-aligned at offset 1. Verified against
            // libvpx output: key = 0x83, delta = 0x87 — they differ only
            // in bit 2. (The old LSB-first reading matched neither, so no
            // VP9 packet was ever seen as a keyframe and stream recovery
            // could never converge.)
            data.first()
                .is_some_and(|b0| (b0 >> 6) == 0b10 && b0 & 0b100 == 0)
        }
        VideoCodecType::Vp8 => {
            // VP8 frame tag, little-endian: bit 0 of the first byte is the
            // frame type — 0 means keyframe.
            data.first().is_some_and(|b0| b0 & 1 == 0)
        }
        VideoCodecType::H264 => {
            // Annex B stream: keyframe iff any NAL has nal_unit_type 5 (IDR).
            let mut i = 0;
            while i + 4 < data.len() {
                let sc = if data[i..].starts_with(&[0, 0, 0, 1]) {
                    4
                } else if data[i..].starts_with(&[0, 0, 1]) {
                    3
                } else {
                    0
                };
                if sc == 0 {
                    i += 1;
                } else {
                    if data[i + sc] & 0x1F == 5 {
                        return true;
                    }
                    i += sc + 1;
                }
            }
            false
        }
        VideoCodecType::Raw | VideoCodecType::Av1 | VideoCodecType::JpegRects => true,
    }
}

/// Input frame for the encoder: BGRA pixels with explicit row stride.
///
/// The stride comes from `DXGI_MAPPED_RECT`/`D3D11_MAPPED_SUBRESOURCE`
/// `RowPitch` — GPU rows are aligned (16/64/128B) and `width*4` is almost
/// never right (riesgos r2 §3.1).
#[derive(Debug, Clone, Copy)]
pub struct VideoFrame<'a> {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row stride in bytes (≥ width*4).
    pub stride: usize,
    /// BGRA pixel data, `stride * height` bytes.
    pub data: &'a [u8],
}

/// An encoded video packet (one frame, possibly fragmented later for UDP).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedPacket {
    /// Codec bitstream for one frame.
    pub data: Vec<u8>,
    /// True if the frame is independently decodable (keyframe).
    pub keyframe: bool,
}

/// A decoded frame, tight BGRA (stride = width*4), ready for a `VideoSink`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row stride in bytes (tight: width*4).
    pub stride: usize,
    /// BGRA pixel data.
    pub data: Vec<u8>,
}

/// Video encoder: BGRA frame in → bitstream packet out.
///
/// `Send` (like the audio traits): the server runs the encoder on its
/// capture thread and the client session may live on any task/thread
/// (§15.3). Each encoder is still used from a single thread at a time.
pub trait VideoEncoder: Send {
    /// Encodes one frame.
    fn encode(&mut self, frame: &VideoFrame<'_>) -> crate::Result<EncodedPacket>;

    /// Forces the next frame to be a keyframe (rate-limited by the channel
    /// layer, max 1 per 500 ms — riesgos §2.1).
    fn request_keyframe(&mut self);

    /// Changes the target bitrate (hot, per-frame granularity where the
    /// backend allows it) for the adaptive bitrate controller (§12).
    fn set_bitrate(&mut self, kbps: u32) -> crate::Result<()>;

    /// Codec implemented by this encoder.
    fn codec_type(&self) -> VideoCodecType;

    /// Takes a pending codec change, if any. Composite encoders (the codec
    /// ladder, §8.5) switch their inner codec at runtime; the server polls
    /// this after each `encode` and, when `Some`, re-announces `DisplayInfo`
    /// with the new codec id before sending the frame's fragments. Simple
    /// encoders never change and use the default.
    fn take_codec_change(&mut self) -> Option<VideoCodecType> {
        None
    }
}

/// Video decoder: bitstream packet in → BGRA frame out.
///
/// `Send` (like the audio traits): the client session owns its decoders and
/// may run on any task/thread (§15.3). Each decoder is still used from a
/// single thread at a time.
pub trait VideoDecoder: Send {
    /// Decodes one packet. `Ok(None)` means the decoder buffered the packet
    /// but produced no frame yet (hardware/OS decoders have inherent
    /// latency — e.g. the MS H264 MFT holds several frames before the first
    /// output); that is normal flow, not a broken stream. Errors are
    /// reserved for real decode failures.
    fn decode(&mut self, packet: &EncodedPacket) -> crate::Result<Option<DecodedFrame>>;

    /// Codec implemented by this decoder.
    fn codec_type(&self) -> VideoCodecType;
}
