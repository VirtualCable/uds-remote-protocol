// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Client-side webcam capture for uds-rp (design doc §18.57), living in
//! the channels crate: the camera is a device, the VP9 encoding is
//! protocol and stays in `client`.
//!
//! The [`WebcamSource`] contract lives in `client::integrations::webcam`
//! (same *shape* as uds-client's `WebcamIntegration`); this module provides
//! the device implementations and the default probe.
//!
//! Two implementations:
//! - [`NokhwaSource`]: the real camera via nokhwa (multiplatform).
//! - [`MockSource`]: deterministic synthetic frames, selected with the
//!   `UDSRP_CAM_MOCK=1` environment variable or automatically when no
//!   camera is present, so E2E tests never need hardware.

mod mock;
mod nokhwa_impl;

pub use client::integrations::webcam::{RawFrame, WebcamSource};
pub use mock::MockSource;
pub use nokhwa_impl::NokhwaSource;

/// Environment variable forcing the synthetic source (like uds-client's
/// `UDSLAUNCHER_CAM_MOCK`).
pub const MOCK_ENV: &str = "UDSRP_CAM_MOCK";

/// Environment variable selecting the camera device (index or name
/// substring), same convention as uds-client's `UDSLAUNCHER_CAM_DEVICE`.
pub const DEVICE_ENV: &str = "UDSRP_CAM_DEVICE";

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
