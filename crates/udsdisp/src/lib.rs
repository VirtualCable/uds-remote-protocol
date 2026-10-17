//! UDS Virtual Display — IddCx (Indirect Display Driver Class eXtension)
//! driver, UMDF user mode.
//!
//! Windows-only crate: on other hosts it compiles to an empty library so the
//! workspace stays buildable and testable on Linux. The driver creates
//! virtual monitors whose resolution is dictated by the remote client; see
//! `doc/plan/iddcx-pantalla-virtual.md` for the spike plan.

// The host-independent logic (modes, IOCTL wire protocol) lives in the pure
// `udsdisp-proto` crate; re-exported so the driver's `crate::logic` paths
// and external callers keep working unchanged.
pub use udsdisp_proto::logic;

#[cfg(windows)]
mod imp;
