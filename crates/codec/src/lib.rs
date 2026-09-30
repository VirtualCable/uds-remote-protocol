//! # codec
//!
//! Codec abstraction: the protocol negotiates codecs via capabilities and
//! the core only sees the [`VideoEncoder`]/[`VideoDecoder`] and
//! [`AudioEncoder`]/[`AudioDecoder`] traits.
//!
//! Implementations behind features (none by default):
//! - `codec-vp9`: VP9 video via libvpx (royalty-free).
//! - `codec-opus`: Opus audio (royalty-free), with per-direction limits
//!   (bitrate, sample rate) negotiated during the handshake.
//! - Always available: [`RawEncoder`]/[`RawDecoder`] (uncompressed reference
//!   implementation for tests and channel-level E2E).

mod error;
mod raw;
mod video;
#[cfg(any(feature = "codec-vp9", feature = "codec-vp8"))]
mod vpx;

/// BGRA → I420 color conversion (scalar Rust; libyuv SIMD later, behind a
/// feature, with the same API).
pub mod color;

pub use error::{CodecError, Result};
pub use raw::{RawDecoder, RawEncoder};
pub use video::{
    DecodedFrame, EncodedPacket, VideoCodecType, VideoDecoder, VideoEncoder, VideoFrame,
};
#[cfg(feature = "codec-vp9")]
pub use vpx::{Vp9Decoder, Vp9Encoder};
#[cfg(feature = "codec-vp8")]
pub use vpx::{Vp8Decoder, Vp8Encoder};
