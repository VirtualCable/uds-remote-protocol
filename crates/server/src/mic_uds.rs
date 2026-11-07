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
///
/// The sink paces its writes instead of forwarding every decoded frame as it
/// arrives: the driver pops the ring on its own exact byte clock, and the mic
/// thread's 20 ms ticks jitter by up to a Windows timer quantum (~16 ms).
/// Writing frame-by-frame let a late tick empty the ring mid-period, which
/// the consumer heard as periodic silence ("morse"). The sink therefore
/// queues frames and pushes them in bursts sized to keep the ring at or
/// above [`TARGET_LEVEL_MS`] of audio; the ring-reported `accepted` byte
/// count closes the loop, so the level estimate self-corrects when nobody is
/// capturing (the ring just fills and stops accepting).
pub struct UdsMicSink {
    device: Option<DeviceGuard>,
    /// Decoded frames received so far.
    frames: u64,
    /// Frames dropped after a push failure (driver gone).
    dropped: u64,
    /// Queued samples not yet pushed to the driver.
    queue: Vec<i16>,
    /// Estimated bytes currently sitting in the driver ring.
    ring_level: f64,
    /// When the ring-level estimate was last aged.
    last_account: std::time::Instant,
}

/// Driver ring capacity (mirrors `logic::MIC_RING_CAPACITY`).
const RING_CAPACITY_BYTES: f64 = 64.0 * 1024.0;
/// Driver consumption rate while a capture stream runs: 48 kHz mono s16.
const RING_BYTE_RATE_PER_MS: f64 = 96.0;
/// Target fill: ~6 mic-thread ticks of jitter headroom (~100 ms of audio).
const TARGET_LEVEL_BYTES: f64 = 120.0 * RING_BYTE_RATE_PER_MS;
/// Maximum queued audio per push burst.
const MAX_QUEUE_MS: u64 = 100;

impl UdsMicSink {
    /// Creates the sink; the device opens on the first frame.
    #[must_use]
    pub fn new() -> Self {
        Self {
            device: None,
            frames: 0,
            dropped: 0,
            queue: Vec::with_capacity(960 * (MAX_QUEUE_MS as usize) / 20),
            ring_level: 0.0,
            last_account: std::time::Instant::now(),
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
                    self.ring_level = 0.0;
                    self.last_account = std::time::Instant::now();
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
        self.frames += 1;
        self.queue.extend_from_slice(pcm);

        // Age the ring-level estimate by the elapsed consumption.
        let now = std::time::Instant::now();
        let elapsed_ms = now.duration_since(self.last_account).as_secs_f64() * 1000.0;
        self.ring_level = (self.ring_level - elapsed_ms * RING_BYTE_RATE_PER_MS).max(0.0);
        self.last_account = now;

        // Push when the ring is running below target or the burst is full.
        let queued_ms = u64::try_from(self.queue.len() / 960).unwrap_or(u64::MAX) * 20;
        if self.ring_level >= TARGET_LEVEL_BYTES && queued_ms < MAX_QUEUE_MS {
            return;
        }
        let bytes: &[u8] =
            // SAFETY: i16 has no padding; the slice is valid for 2*len bytes.
            unsafe {
                std::slice::from_raw_parts(
                    self.queue.as_ptr().cast(),
                    std::mem::size_of_val(self.queue.as_slice()),
                )
            };
        let mut accepted = 0u32;
        // SAFETY: the input buffer is valid for `bytes.len()`; the output is
        // the returned byte count; `accepted` is a valid out pointer.
        let ok = unsafe {
            DeviceIoControl(
                self.device.as_ref().expect("checked").0,
                IOCTL_PUSH_MIC,
                Some(bytes.as_ptr().cast()),
                u32::try_from(bytes.len()).unwrap_or(u32::MAX),
                None,
                0,
                Some(&raw mut accepted),
                None,
            )
        };
        if ok.is_ok() {
            self.ring_level =
                (self.ring_level + f64::from(accepted)).min(RING_CAPACITY_BYTES);
            if (accepted as usize) < bytes.len() {
                // A short write means the ring filled up (nobody draining):
                // snap the estimate to full so we stop pushing for a while.
                self.ring_level = RING_CAPACITY_BYTES;
            }
            // The ring may take only part of the burst (full, or nobody
            // draining): keep the untaken tail for the next push.
            let kept_samples = usize::try_from(accepted).unwrap_or(0) / 2;
            if kept_samples >= self.queue.len() {
                self.queue.clear();
            } else {
                self.queue.drain(..kept_samples);
                // The ring rejected the rest because it is full: the tail
                // would go stale waiting for the next burst, so drop it and
                // count the loss (in frames, like the other counters).
                let dropped_frames = (self.queue.len() / 960) as u64;
                self.queue.clear();
                self.dropped += dropped_frames;
                if dropped_frames > 0 && self.dropped % LOG_EVERY_FRAMES < dropped_frames {
                    warn!(
                        dropped = self.dropped,
                        "mic feed: ring full, dropping stale tail (nobody capturing?)"
                    );
                }
            }
            if self.frames % LOG_EVERY_FRAMES == 1 {
                info!(
                    frames = self.frames,
                    ring_ms = (self.ring_level / RING_BYTE_RATE_PER_MS) as u32,
                    "mic feed to the UDS driver alive"
                );
            }
        } else {
            // Driver vanished mid-session: drop the handle, count and keep
            // decoding (the sink must never kill the mic thread).
            self.device = None;
            self.queue.clear();
            self.dropped += 1;
            if self.dropped % LOG_EVERY_FRAMES == 1 {
                warn!(dropped = self.dropped, "mic feed push failed; driver gone?");
            }
        }
    }
}
