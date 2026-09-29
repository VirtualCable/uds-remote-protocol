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
