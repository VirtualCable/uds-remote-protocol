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
    /// Operation not supported by this codec/backend.
    #[error("unsupported operation: {0}")]
    Unsupported(&'static str),
}

/// Standard result of the crate.
pub type Result<T> = std::result::Result<T, CodecError>;
