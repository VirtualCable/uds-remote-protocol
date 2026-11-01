//! UDS driver loopback source (Windows only): reads the session mix straight
//! from the udsaudio driver's render→loopback ring through the
//! `\\.\UDSAudioCtl` control device, bypassing the WASAPI loopback path.
//!
//! Why this exists: the audio engine's loopback tap on our WaveRT endpoint
//! delivered ~20 s of full-scale garbage every time a new render stream
//! started on the endpoint (session start, ad break in the browser), while
//! the driver's own harvest path (render sink → ring) is entirely under our
//! control and can only ever contain what the engine wrote to the render
//! buffer. Reading the ring via `IOCTL_READ_LOOPBACK` is both the fix and
//! the cleanest possible source: no engine involvement at all.
//!
//! The ring already carries the wire shape (48 kHz stereo s16), so there is
//! no conversion pipeline: a reader thread pops chunks into a ring buffer
//! and `fill_frame` drains it, synthesizing silence on underrun (no render
//! stream playing, or the harvest not running).

// The `windows` crate FFI methods take raw pointers; implicit borrows are
// the idiomatic call style for that API.
#![allow(clippy::borrow_as_ptr)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context as _, bail};
use tracing::{info, warn};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::PCWSTR;
use windows::core::w;

use super::audio::AudioSource;
use codec::audio_dsp::CaptureRing;

/// `CTL_CODE(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
/// Must match `logic::IOCTL_READ_LOOPBACK` in the driver.
const IOCTL_READ_LOOPBACK: u32 = 0x0022_2004;

/// Control device symlink created by the driver.
const CONTROL_PATH: PCWSTR = w!("\\\\.\\UDSAudioCtl");

/// Reader-thread poll interval. Each poll asks for up to `CHUNK_MS` of PCM;
/// the IOCTL returns less (possibly 0) when the ring has not accumulated
/// that much yet.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How much audio each IOCTL asks for at most.
const CHUNK_MS: usize = 40;

/// Ring capacity: 500 ms of wire-format audio (same policy as the WASAPI
/// source: overflow drops the oldest samples).
const RING_MS: usize = 500;

/// Underrun warning cadence (same policy as the WASAPI source).
const UNDERRUN_WARN_SAMPLES: u64 = 480_000;

/// Reads the driver's loopback ring into a `CaptureRing` until `quit` is
/// set. The device handle is opened and owned here, on the reader thread.
fn reader_loop(shared: &Shared, out_rate: u32, out_channels: u8) -> anyhow::Result<()> {
    // SAFETY: static null-terminated path; no template, no security
    // attributes.
    let device = unsafe {
        CreateFileW(
            CONTROL_PATH,
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .context("cannot open \\\\.\\UDSAudioCtl (is the udsaudio driver installed?)")?;
    let device = DeviceGuard(device);

    let frame_bytes = usize::from(out_channels) * 2; // s16
    let mut chunk = vec![0u8; out_rate as usize * frame_bytes * CHUNK_MS / 1000];
    while !shared.quit.load(Ordering::Relaxed) {
        let mut returned = 0u32;
        // SAFETY: `chunk` is a valid output buffer for its full length;
        // `returned` receives the popped byte count.
        unsafe {
            DeviceIoControl(
                device.0,
                IOCTL_READ_LOOPBACK,
                None,
                0,
                Some(chunk.as_mut_ptr().cast()),
                u32::try_from(chunk.len())?,
                Some(&raw mut returned),
                None,
            )
        }
        .context("DeviceIoControl(IOCTL_READ_LOOPBACK) failed")?;
        let bytes = &chunk[..returned as usize];
        if !bytes.is_empty() {
            // The ring carries s16 samples in the wire shape already.
            let samples: Vec<i16> = bytes
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            let mut ring = shared.ring.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            ring.push(&samples);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

/// RAII guard closing the control-device handle.
struct DeviceGuard(HANDLE);

impl Drop for DeviceGuard {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and open.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// State shared between the reader thread and the audio thread.
struct Shared {
    ring: std::sync::Mutex<CaptureRing>,
    quit: AtomicBool,
}

/// Driver loopback [`AudioSource`]: the session mix straight from the
/// udsaudio ring. See the module docs for the rationale.
pub struct UdsLoopbackSource {
    sample_rate: u32,
    channels: u8,
    frame_samples: usize,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    underrun_samples: u64,
    last_underrun_warn: u64,
}

impl UdsLoopbackSource {
    /// Starts the reader thread. The ring carries 48 kHz stereo s16, so the
    /// requested wire shape must match exactly (the Opus presets all do).
    pub fn new(sample_rate: u32, channels: u8, frame_samples: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(
            sample_rate == 48_000 && channels == 2,
            "uds-loopback only carries 48 kHz stereo s16 (requested {sample_rate} Hz, {channels} ch)"
        );
        anyhow::ensure!(frame_samples > 0, "empty frames");
        let ring_samples = sample_rate as usize * usize::from(channels) * RING_MS / 1000;
        let shared = Arc::new(Shared {
            ring: std::sync::Mutex::new(CaptureRing::new(ring_samples)),
            quit: AtomicBool::new(false),
        });
        // Open + all IOCTL traffic happens on the reader thread; the init
        // outcome comes back over this channel so a missing driver is
        // reported to the caller instead of producing a silently-dead source.
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("uds-loopback-reader".to_string())
            .spawn(move || {
                // Probe the device once before entering the loop so a missing
                // driver fails `new` synchronously.
                let probe = {
                    // SAFETY: static null-terminated path.
                    unsafe {
                        CreateFileW(
                            CONTROL_PATH,
                            GENERIC_READ.0,
                            FILE_SHARE_READ | FILE_SHARE_WRITE,
                            None,
                            OPEN_EXISTING,
                            FILE_FLAGS_AND_ATTRIBUTES(0),
                            None,
                        )
                    }
                };
                match probe {
                    Ok(handle) => {
                        // SAFETY: probe handle, closed immediately.
                        unsafe {
                            let _ = CloseHandle(handle);
                        }
                        if ready_tx.send(Ok(())).is_ok()
                            && let Err(e) = reader_loop(&thread_shared, sample_rate, channels)
                        {
                            warn!("uds-loopback reader failed: {e:#}");
                        }
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })
            .context("cannot spawn the uds-loopback reader thread")?;
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                shared.quit.store(true, Ordering::Relaxed);
                let _ = thread.join();
                return Err(e).context("UDS audio control device unavailable");
            }
            Err(_) => {
                shared.quit.store(true, Ordering::Relaxed);
                let _ = thread.join();
                bail!("uds-loopback reader thread died during init");
            }
        }
        info!("UDS driver loopback capture started (48 kHz stereo)");
        Ok(Self {
            sample_rate,
            channels,
            frame_samples,
            shared,
            thread: Some(thread),
            underrun_samples: 0,
            last_underrun_warn: 0,
        })
    }
}

impl Drop for UdsLoopbackSource {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl AudioSource for UdsLoopbackSource {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    fn fill_frame(&mut self, out: &mut [i16]) -> anyhow::Result<()> {
        let expected = self.frame_samples * usize::from(self.channels);
        anyhow::ensure!(
            out.len() == expected,
            "audio frame buffer must be exactly {expected} samples, got {}",
            out.len()
        );
        let missing = {
            let mut ring = self.shared.ring.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            ring.fill(out)
        };
        if missing > 0 {
            self.underrun_samples += missing as u64;
            if self.last_underrun_warn == 0
                || self.underrun_samples - self.last_underrun_warn >= UNDERRUN_WARN_SAMPLES
            {
                warn!(
                    missing,
                    total = self.underrun_samples,
                    "uds-loopback starved; emitting silence"
                );
                self.last_underrun_warn = self.underrun_samples.max(1);
            }
            if self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
                bail!("uds-loopback reader thread ended");
            }
        }
        Ok(())
    }
}
