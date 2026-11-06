// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! UDS driver microphone sink (§9.2): pushes the decoded client microphone
//! (48 kHz mono s16, already the wire shape) into the udsaudio driver's mic
//! ring via `IOCTL_PUSH_MIC`, so the remote session sees a real capture
//! endpoint ("UDS Virtual Audio Device").
//!
//! The control device is opened lazily on the first frame: the mic thread
//! must not die just because the driver was still starting when the session
//! began. A push failure disables the sink (the driver is gone — further
//! frames are dropped, counted and logged at a throttled cadence) instead
//! of killing audio-in for the session.

use anyhow::Context as _;
use tracing::{info, warn};
use windows::Win32::Foundation::{CloseHandle, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::PCWSTR;
use windows::core::w;

use super::mic::MicSink;

/// `CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
/// Must match `logic::IOCTL_PUSH_MIC` in the driver.
const IOCTL_PUSH_MIC: u32 = 0x0022_2000;

/// Control device symlink created by the driver.
const CONTROL_PATH: PCWSTR = w!("\\\\.\\UDSAudioCtl");

/// Failure/progress log cadence: one line per ~5 s of audio at 20 ms frames.
const LOG_EVERY_FRAMES: u64 = 250;

/// RAII guard for the control-device handle.
struct DeviceGuard(HANDLE);

// SAFETY: the handle is only used from the thread that owns the sink (the
// mic thread); `DeviceIoControl` and `CloseHandle` are thread-safe Win32
// calls. `HANDLE` is a raw pointer, so the auto impl is (correctly) absent.
unsafe impl Send for DeviceGuard {}

impl Drop for DeviceGuard {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and valid.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Mic sink feeding the udsaudio driver's virtual microphone.
pub struct UdsMicSink {
    device: Option<DeviceGuard>,
    /// Frames delivered to the driver.
    frames: u64,
    /// Frames dropped after a push failure (driver gone).
    dropped: u64,
}

impl UdsMicSink {
    /// Creates the sink; the device opens on the first frame.
    #[must_use]
    pub fn new() -> Self {
        Self {
            device: None,
            frames: 0,
            dropped: 0,
        }
    }

    fn open() -> anyhow::Result<DeviceGuard> {
        // SAFETY: static null-terminated path; no template, no security
        // attributes.
        let handle = unsafe {
            CreateFileW(
                CONTROL_PATH,
                GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .context("cannot open \\\\.\\UDSAudioCtl (is the udsaudio driver installed?)")?;
        Ok(DeviceGuard(handle))
    }
}

impl Default for UdsMicSink {
    fn default() -> Self {
        Self::new()
    }
}

impl MicSink for UdsMicSink {
    fn write_pcm(&mut self, pcm: &[i16]) {
        if self.device.is_none() {
            match Self::open() {
                Ok(dev) => {
                    info!("UDS driver mic feed started (48 kHz mono)");
                    self.device = Some(dev);
                }
                Err(e) => {
                    self.dropped += 1;
                    if self.dropped % LOG_EVERY_FRAMES == 1 {
                        warn!("mic feed cannot open the driver: {e:#}");
                    }
                    return;
                }
            }
        }
        let device = self.device.as_ref().expect("checked");
        let bytes: &[u8] =
            // SAFETY: i16 has no padding; the slice is valid for 2*len bytes.
            unsafe { std::slice::from_raw_parts(pcm.as_ptr().cast(), std::mem::size_of_val(pcm)) };
        let mut returned = 0u32;
        // SAFETY: the input buffer is valid for `bytes.len()`; no output
        // buffer is used; `returned` is a valid out pointer.
        let ok = unsafe {
            DeviceIoControl(
                device.0,
                IOCTL_PUSH_MIC,
                Some(bytes.as_ptr().cast()),
                u32::try_from(bytes.len()).unwrap_or(u32::MAX),
                None,
                0,
                Some(&raw mut returned),
                None,
            )
        };
        if ok.is_ok() {
            self.frames += 1;
            if self.frames % LOG_EVERY_FRAMES == 1 {
                info!(frames = self.frames, "mic feed to the UDS driver alive");
            }
        } else {
            // Driver vanished mid-session: drop the handle, count and keep
            // decoding (the sink must never kill the mic thread).
            self.device = None;
            self.dropped += 1;
            if self.dropped % LOG_EVERY_FRAMES == 1 {
                warn!(dropped = self.dropped, "mic feed push failed; driver gone?");
            }
        }
    }
}
