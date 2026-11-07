// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Webcam capture contract (design doc §18.57): the camera is a device,
//! the VP9 encoding is protocol and stays in `client`.
//!
//! The [`WebcamSource`] trait has the same *shape* as the
//! `rdp::integrations::webcam::WebcamIntegration` trait used by uds-client
//! (minus the RDP-channel-specific `channel_ptr`/`request_sample`/
//! `push_frame` methods), so a future uds-client integration is a thin
//! wrapper in whichever direction fits.
//!
//! Implementations (real camera via nokhwa, deterministic mock) live in
//! `channels::webcam`.

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
