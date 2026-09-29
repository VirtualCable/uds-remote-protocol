//! `RawCodec`: uncompressed BGRA passthrough. Reference implementation of
//! the video traits, used by channel-level E2E tests (it needs no external
//! libraries and every frame is independent).
//!
//! Wire format of the packet: `|width 4B LE|height 4B LE|tight BGRA data|`
//! (stride padding is stripped on encode — it is wire overhead).

use crate::error::{CodecError, Result};
use crate::video::{
    DecodedFrame, EncodedPacket, VideoCodecType, VideoDecoder, VideoEncoder, VideoFrame,
};

/// Uncompressed video encoder (test/reference implementation).
#[derive(Debug, Default)]
pub struct RawEncoder;

impl RawEncoder {
    /// Creates the encoder.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Validates a BGRA frame's geometry. Shared with the decoder.
    fn check_dims(width: u32, height: u32, stride: usize, data_len: usize) -> Result<()> {
        let row = width as usize * 4;
        if stride < row || data_len < stride * (height as usize - 1) + row {
            return Err(CodecError::BadDimensions {
                width,
                height,
                stride,
                data_len,
            });
        }
        Ok(())
    }
}

impl VideoEncoder for RawEncoder {
    fn encode(&mut self, frame: &VideoFrame<'_>) -> Result<EncodedPacket> {
        Self::check_dims(frame.width, frame.height, frame.stride, frame.data.len())?;

        let row = frame.width as usize * 4;
        let mut data = Vec::with_capacity(8 + row * frame.height as usize);
        data.extend_from_slice(&frame.width.to_le_bytes());
        data.extend_from_slice(&frame.height.to_le_bytes());
        for y in 0..frame.height as usize {
            data.extend_from_slice(&frame.data[y * frame.stride..y * frame.stride + row]);
        }
        Ok(EncodedPacket {
            data,
            keyframe: true,
        })
    }

    fn request_keyframe(&mut self) {
        // Every raw frame is independent: nothing to do.
    }

    fn set_bitrate(&mut self, _kbps: u32) -> Result<()> {
        // No rate control without compression: accepted as a no-op.
        Ok(())
    }

    fn codec_type(&self) -> VideoCodecType {
        VideoCodecType::Raw
    }
}

/// Uncompressed video decoder (test/reference implementation).
#[derive(Debug, Default)]
pub struct RawDecoder;

impl RawDecoder {
    /// Creates the decoder.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl VideoDecoder for RawDecoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<DecodedFrame> {
        if packet.data.len() < 8 {
            return Err(CodecError::Malformed);
        }
        let width = u32::from_le_bytes(packet.data[..4].try_into().expect("4-byte slice"));
        let height = u32::from_le_bytes(packet.data[4..8].try_into().expect("4-byte slice"));
        let stride = width as usize * 4;
        let expected = stride
            .checked_mul(height as usize)
            .ok_or(CodecError::Malformed)?;
        let data = &packet.data[8..];
        if width == 0 || height == 0 || data.len() != expected {
            return Err(CodecError::Malformed);
        }
        Ok(DecodedFrame {
            width,
            height,
            stride,
            data: data.to_vec(),
        })
    }

    fn codec_type(&self) -> VideoCodecType {
        VideoCodecType::Raw
    }
}
