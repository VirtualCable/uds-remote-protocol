// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

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
#[cfg(all(windows, feature = "windows-capture"))]
pub mod audio_endpoint;
#[cfg(all(windows, feature = "windows-capture"))]
pub mod audio_uds;
#[cfg(all(windows, feature = "windows-capture"))]
pub mod audio_wasapi;
pub mod capture;
pub mod clipboard;
pub mod display_mode;
pub use server_config as config;
#[cfg(all(windows, feature = "windows-capture"))]
pub mod handoff;
#[cfg(all(windows, feature = "windows-capture"))]
pub mod dxgi;
pub mod fps;
pub mod input;
pub mod mic;
pub mod pacer;
#[cfg(all(windows, feature = "windows-capture"))]
pub mod session_mutex;
pub mod smartcard;
#[cfg(all(windows, feature = "windows-capture"))]
pub mod smartcard_pipe;
pub mod storage;
pub mod synthetic;
