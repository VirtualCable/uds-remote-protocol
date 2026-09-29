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

pub use error::{CodecError, Result};
pub use raw::{RawDecoder, RawEncoder};
pub use video::{
    DecodedFrame, EncodedPacket, VideoCodecType, VideoDecoder, VideoEncoder, VideoFrame,
};
