//! `udsdisp-ctl` library: exposes the driver control client so the
//! orchestrator service can reuse it. On non-Windows hosts this crate is an
//! empty shell so the workspace stays buildable.

#[cfg(windows)]
pub mod client;
