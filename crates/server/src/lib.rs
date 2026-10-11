//! # server
//!
//! Server side of the remote desktop protocol.
//!
//! Screen capture and input injection live behind a platform trait
//! (`FrameSource` in `capture`), so the session core stays portable. The
//! reference implementation is Windows (Desktop Duplication API + `SendInput`),
//! behind the `windows-capture` feature; Linux/macOS may be added in the
//! future without touching the core.

pub mod audio;
pub mod capture;
#[cfg(all(windows, feature = "windows-capture"))]
pub mod dxgi;
pub mod input;
pub mod synthetic;
