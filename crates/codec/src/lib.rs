// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

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

mod audio;
mod error;
#[cfg(feature = "codec-jpeg")]
mod jpeg_rects;
#[cfg(feature = "codec-ladder")]
mod ladder_encoder;
#[cfg(feature = "codec-opus")]
mod opus;
mod preset;
mod raw;
mod video;
#[cfg(any(feature = "codec-vp9", feature = "codec-vp8"))]
mod vpx;

/// Audio format adaptation between device shapes and the wire format:
/// f32→i16 conversion, channel remix, linear resampling, capture ring (§9).
pub mod audio_dsp;
/// BGRA → I420 color conversion (scalar Rust; libyuv SIMD later, behind a
/// feature, with the same API).
pub mod color;
/// Codec ladder state machine: decides when to switch codec regime (§8.5).
pub mod ladder;
/// Dirty-region detection and rect merging for rect-oriented codecs (§8.5).
pub mod rects;

pub use audio::{
    AUDIO_PRESETS, AudioCodecType, AudioDecoder, AudioEncoder, AudioPreset, build_audio_decoder,
    build_audio_encoder,
};
pub use error::{CodecError, Result};
pub use ladder::{Ladder, LadderPolicy, Level, Switch};
#[cfg(feature = "codec-ladder")]
pub use ladder_encoder::LadderEncoder;
#[cfg(feature = "codec-jpeg")]
pub use jpeg_rects::{JpegRectsDecoder, JpegRectsEncoder};
pub use preset::{CodecParams, CodecPreset, DEFAULT_BITRATE_KBPS, PRESETS, build_decoder};
#[cfg(feature = "codec-opus")]
pub use opus::{OpusDecoder, OpusEncoder};
pub use raw::{RawDecoder, RawEncoder};
pub use video::{
    DecodedFrame, EncodedPacket, VideoCodecType, VideoDecoder, VideoEncoder, VideoFrame,
};
#[cfg(feature = "codec-vp9")]
pub use vpx::{Vp9Decoder, Vp9Encoder};
#[cfg(feature = "codec-vp8")]
pub use vpx::{Vp8Decoder, Vp8Encoder};
