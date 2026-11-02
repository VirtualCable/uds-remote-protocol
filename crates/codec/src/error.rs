// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Errors of the `codec` crate.

/// Codec errors (video and audio).
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// Frame dimensions/stride/data size are inconsistent.
    #[error("bad frame dimensions: {width}x{height}, stride {stride}, data {data_len} bytes")]
    BadDimensions {
        /// Frame width in pixels.
        width: u32,
        /// Frame height in pixels.
        height: u32,
        /// Row stride in bytes.
        stride: usize,
        /// Actual data length in bytes.
        data_len: usize,
    },
    /// The encoder/decoder backend failed.
    #[error("codec backend error: {0}")]
    Backend(String),
    /// Malformed bitstream/packet.
    #[error("malformed packet")]
    Malformed,
    /// PCM input length does not match the configured frame size.
    #[error("bad PCM length: got {got} samples, expected exactly {expected}")]
    BadPcmLength {
        /// Samples actually provided.
        got: usize,
        /// Samples required (`frame_samples * channels`).
        expected: usize,
    },
    /// PCM output buffer cannot hold one full decoded frame.
    #[error("output buffer too small: got {got} samples, need at least {needed}")]
    OutputTooSmall {
        /// Samples the caller's buffer holds.
        got: usize,
        /// Samples required (`frame_samples * channels`).
        needed: usize,
    },
    /// Invalid codec configuration (sample rate, channels, frame duration...).
    #[error("invalid configuration: {0}")]
    InvalidConfig(&'static str),
    /// Operation not supported by this codec/backend.
    #[error("unsupported operation: {0}")]
    Unsupported(&'static str),
}

/// Standard result of the crate.
pub type Result<T> = std::result::Result<T, CodecError>;
