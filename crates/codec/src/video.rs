//! Video codec traits and common types (design doc §8).
//!
//! The protocol negotiates a codec via capabilities; the core only sees
//! these traits. The trait is codec-agnostic (VP9 today; VP8, H.264 or AV1
//! tomorrow) and backend-agnostic (software or hardware): the negotiated
//! capability announces `codec + backend`.

/// Video codec identifier, with stable wire IDs for capability negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
            _ => None,
        }
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
pub trait VideoEncoder {
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
}

/// Video decoder: bitstream packet in → BGRA frame out.
pub trait VideoDecoder {
    /// Decodes one packet.
    fn decode(&mut self, packet: &EncodedPacket) -> crate::Result<DecodedFrame>;

    /// Codec implemented by this decoder.
    fn codec_type(&self) -> VideoCodecType;
}
