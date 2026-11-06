// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Client-side webcam capture for uds-rp (design doc §18.57), living in
//! the channels crate: the camera is a device, the VP9 encoding is
//! protocol and stays in `client`.
//!
//! The [`WebcamSource`] trait has the same *shape* as the
//! `rdp::integrations::webcam::WebcamIntegration` trait used by uds-client
//! (minus the RDP-channel-specific `channel_ptr`/`request_sample`/
//! `push_frame` methods), so a future uds-client integration is a thin
//! wrapper in whichever direction fits.
//!
//! Two implementations:
//! - [`NokhwaSource`]: the real camera via nokhwa (multiplatform).
//! - [`MockSource`]: deterministic synthetic frames, selected with the
//!   `UDSRP_CAM_MOCK=1` environment variable or automatically when no
//!   camera is present, so E2E tests never need hardware.

mod mock;
mod nokhwa_impl;

pub use mock::MockSource;
pub use nokhwa_impl::NokhwaSource;

/// Environment variable forcing the synthetic source (like uds-client's
/// `UDSLAUNCHER_CAM_MOCK`).
pub const MOCK_ENV: &str = "UDSRP_CAM_MOCK";

/// Environment variable selecting the camera device (index or name
/// substring), same convention as uds-client's `UDSLAUNCHER_CAM_DEVICE`.
pub const DEVICE_ENV: &str = "UDSRP_CAM_DEVICE";

/// One captured frame, RGB24 (`width*height*3` bytes), straight from the
/// camera backend. The VP9 encoder stage converts to its input format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Capture timestamp in milliseconds (client monotonic clock).
    pub timestamp_ms: u32,
    /// RGB24 pixel data, tight stride (`width*3`).
    pub rgb: Vec<u8>,
}

/// A capturable webcam. Implementations must be cheap to construct (the
/// camera is only opened on [`WebcamSource::start`]).
pub trait WebcamSource: Send + Sync + std::fmt::Debug {
    /// Human-readable device name (for logs and the client's UI).
    fn device_name(&self) -> String;
    /// Maximum capture resolution the device supports, `(0, 0)` when
    /// unknown (the server clamps its request to this when nonzero).
    fn max_dimensions(&self) -> (u32, u32);
    /// Maximum capture frame rate, 0 when unknown.
    fn max_fps(&self) -> u32;
    /// Opens the device and starts streaming frames at the negotiated
    /// geometry (the backend picks the closest supported format). The
    /// receiver ends when [`WebcamSource::stop`] is called or the source
    /// is dropped. Starting twice without an intermediate stop is an
    /// error.
    fn start(
        &self,
        width: u32,
        height: u32,
        fps: u32,
    ) -> anyhow::Result<flume::Receiver<RawFrame>>;
    /// Stops the stream started by [`WebcamSource::start`]. Idempotent.
    fn stop(&self);
}

/// True when the environment requests the synthetic source.
#[must_use]
pub fn mock_requested() -> bool {
    std::env::var(MOCK_ENV).is_ok_and(|val| val == "1")
}

/// Builds the default source: the real camera, or the mock when
/// `UDSRP_CAM_MOCK=1` is set. Returns `None` when no camera is available
/// (the client then simply does not offer `FLAG_WEBCAM`... unless the mock
/// was requested, which always succeeds).
#[must_use]
pub fn default_source() -> Option<Box<dyn WebcamSource>> {
    if mock_requested() {
        return Some(Box::new(MockSource::new()));
    }
    match NokhwaSource::probe() {
        Ok(source) => Some(Box::new(source)),
        Err(err) => {
            tracing::warn!("webcam: no camera available ({err}); webcam redirection disabled");
            None
        }
    }
}
