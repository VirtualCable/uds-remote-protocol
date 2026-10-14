//! Global driver state: one adapter and its monitors.
//!
//! WDF object contexts would be the idiomatic alternative; a global map keeps
//! the spike simple (a single device and adapter exist per driver instance).
//! Monitors are keyed by their IddCx handle so more can be added later without
//! restructuring (only one is created by default — see the spike plan §9).

use std::sync::{Mutex, MutexGuard, PoisonError};

use super::swapchain;

/// Per-monitor runtime state.
pub struct MonitorEntry {
    /// IddCx monitor object (`IDDCX_MONITOR`), stored as `usize` because raw
    /// handles are `!Send`.
    pub handle: usize,
    /// Connector index this monitor was created with.
    pub connector_index: u32,
    /// Active swap-chain processing task, if the OS has assigned one.
    pub swapchain_task: Option<swapchain::Task>,
}

/// Whole-driver state.
pub struct DriverState {
    /// The IddCx adapter object (`IDDCX_ADAPTER` as `usize`), set on D0 entry.
    pub adapter: Option<usize>,
    /// Live monitors.
    pub monitors: Vec<MonitorEntry>,
}

static STATE: Mutex<DriverState> = Mutex::new(DriverState {
    adapter: None,
    monitors: Vec::new(),
});

/// Locks the global state, recovering from poisoning: a poisoned driver state
/// is still better than panicking inside a WDF/IddCx callback (which would
/// unwind into the UMDF host).
pub fn lock() -> MutexGuard<'static, DriverState> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}
