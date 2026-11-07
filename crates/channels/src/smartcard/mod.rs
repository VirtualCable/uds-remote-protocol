// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Smartcard backends (§17): implementations of the
//! [`SmartcardIntegration`] contract (which lives in
//! `client::integrations::smartcard`, alongside the `SmartcardService`
//! dispatcher). Provided here: the scripted [`DummyBackend`] (tests/E2E)
//! and the real [`PcscBackend`] (feature `pcsc-backend`).

pub mod dummy;
#[cfg(feature = "pcsc-backend")]
pub mod pcsc_backend;

pub use client::integrations::smartcard::*;
pub use dummy::DummyBackend;
#[cfg(feature = "pcsc-backend")]
pub use pcsc_backend::PcscBackend;

use std::sync::Arc;

#[cfg(feature = "pcsc-backend")]
use tracing::warn;

/// Builds the platform backend for a session: the real PC/SC stack with
/// the `pcsc-backend` feature, `None` (→ `SCARD_E_NO_SERVICE` replies)
/// otherwise or when the subsystem is unavailable.
#[cfg(feature = "pcsc-backend")]
#[must_use]
pub fn default_backend() -> Option<Arc<dyn SmartcardIntegration>> {
    let backend = PcscBackend::new();
    if backend.is_available() {
        Some(Arc::new(backend))
    } else {
        warn!("PC/SC subsystem unavailable; smartcard requests will fail with SCARD_E_NO_SERVICE");
        None
    }
}

/// Without the `pcsc-backend` feature there is no backend: requests are
/// answered with `SCARD_E_NO_SERVICE`.
#[cfg(not(feature = "pcsc-backend"))]
#[must_use]
pub fn default_backend() -> Option<Arc<dyn SmartcardIntegration>> {
    None
}
