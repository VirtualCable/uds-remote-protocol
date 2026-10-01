//! # codec
//!
//! Codec abstraction: the protocol negotiates codecs via capabilities and
//! the core only sees the [`VideoEncoder`]/[`VideoDecoder`] and
//! [`AudioEncoder`]/[`AudioDecoder`] traits.
//!
//! Implementations behind features (none by default):
//! - `codec-vp9`: VP9 video via libvpx (royalty-free).
//! - `codec-vp8`: VP8 video via libvpx (compat only, not recommended — §8.4).
//! - `codec-jpeg`: JPEG-over-dirty-rects video via libjpeg-turbo (§8.5).
//! - `codec-opus`: Opus audio (royalty-free), with per-direction limits
//!   (bitrate, sample rate) negotiated during the handshake.
//! - Always available: [`RawEncoder`]/[`RawDecoder`] (uncompressed reference
//!   implementation for tests and channel-level E2E).

mod error;
#[cfg(feature = "codec-jpeg")]
mod jpeg_rects;
mod preset;
mod raw;
mod video;
#[cfg(any(feature = "codec-vp9", feature = "codec-vp8"))]
mod vpx;

/// BGRA → I420 color conversion (scalar Rust; libyuv SIMD later, behind a
/// feature, with the same API).
pub mod color;
/// Dirty-region detection and rect merging for rect-oriented codecs (§8.5).
pub mod rects;

pub use error::{CodecError, Result};
#[cfg(feature = "codec-jpeg")]
pub use jpeg_rects::{JpegRectsDecoder, JpegRectsEncoder};
pub use preset::{CodecParams, CodecPreset, PRESETS, build_decoder};
pub use raw::{RawDecoder, RawEncoder};
pub use video::{
    DecodedFrame, EncodedPacket, VideoCodecType, VideoDecoder, VideoEncoder, VideoFrame,
};
#[cfg(feature = "codec-vp9")]
pub use vpx::{Vp9Decoder, Vp9Encoder};
#[cfg(feature = "codec-vp8")]
pub use vpx::{Vp8Decoder, Vp8Encoder};
