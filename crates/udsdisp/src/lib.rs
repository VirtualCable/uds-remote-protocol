//! UDS Virtual Display — IddCx (Indirect Display Driver Class eXtension)
//! driver, UMDF user mode.
//!
//! Windows-only crate: on other hosts it compiles to an empty library so the
//! workspace stays buildable and testable on Linux. The driver creates
//! virtual monitors whose resolution is dictated by the remote client; see
//! `doc/plan/iddcx-pantalla-virtual.md` for the spike plan.

pub mod logic;

#[cfg(windows)]
mod imp;
