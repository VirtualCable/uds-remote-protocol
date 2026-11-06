// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! WASAPI loopback capture source (Windows only, design doc §9).
//!
//! Captures what the default render endpoint is playing (system audio) via
//! `IAudioClient` loopback in shared mode, polling packets on a dedicated
//! capture thread:
//!
//! ```text
//! capture thread (owns the COM objects, ~5 ms poll):
//!   GetBuffer ─▶ DevicePipeline (i16 → stereo → 48 kHz) ─▶ CaptureRing
//! audio thread (`fill_frame`, every 20 ms):
//!   CaptureRing ─▶ one frame, silence on underrun
//! ```
//!
//! COM objects never leave the capture thread: `new` spawns it, does the
//! whole init there and reports the outcome back, so an init failure (no
//! render endpoint, Audiosrv stopped) is visible to the caller. The audio
//! thread only ever touches the shared ring — a `fill_frame` never blocks on
//! WASAPI, never panics, and turns a starved ring into silence; a dead
//! capture thread fails the source so the session drops audio loudly.

// The `windows` crate FFI methods take raw pointers; implicit borrows are
// the idiomatic call style for that API.
#![allow(clippy::borrow_as_ptr)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context as _, bail};
use tracing::{info, warn};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, IAudioCaptureClient, IAudioClient,
    IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};

use super::audio::AudioSource;
use channels::audio::dsp::{CaptureRing, DevicePipeline, SampleFormat};

/// `wFormatTag` values (u16 in `WAVEFORMATEX`; the windows crate exposes
/// them as u32 constants, so local copies keep the compares literal).
const WAVE_FORMAT_PCM_TAG: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT_TAG: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE_TAG: u16 = 0xFFFE;

/// `AUDCLNT_BUFFERFLAGS_SILENT`: the packet is all silence and its data
/// bytes are undefined.
const BUFFERFLAGS_SILENT: u32 = 0x2;

/// Shared-mode buffer request: 200 ms in 100 ns units. Generous on purpose:
/// the capture thread polls every ~5 ms, so the buffer only has to absorb
/// scheduling jitter, and a large buffer never loses packets.
const BUFFER_DURATION_HNS: i64 = 2_000_000;

/// Capture-thread poll interval (~half the default 10 ms device period).
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Ring capacity: 500 ms of wire-format audio. Overflow drops the oldest
/// samples (fresh desktop audio matters more than continuity).
const RING_MS: u32 = 500;

/// Underrun warning cadence: warn on the first underrun, then at most once
/// per ~10 s of synthesized silence (48 000 stereo samples ≈ 5 s of audio;
/// two of those per warning).
const UNDERRUN_WARN_SAMPLES: u64 = 480_000;

/// Parsed device mix format.
struct MixFormat {
    format: SampleFormat,
    channels: u8,
    sample_rate: u32,
}

/// State owned by the capture thread: the COM objects and the conversion
/// pipeline. Never leaves the thread (COM apartment discipline).
struct CaptureState {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    pipeline: DevicePipeline,
    block_align: usize,
}

impl CaptureState {
    /// Opens loopback capture on the default render endpoint and starts it.
    /// Must run on the capture thread (COM init happens here).
    fn init(out_rate: u32, out_channels: u8) -> anyhow::Result<Self> {
        // SAFETY: standard per-thread COM init; S_FALSE (already initialized
        // as MTA) is accepted by `.ok()`. Balanced by CoUninitialize when the
        // capture thread ends.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .context("CoInitializeEx failed")?;
        // SAFETY: coclass activation with valid out-params.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .context("MMDeviceEnumerator creation failed (is Audiosrv running?)")?;
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .context("no default audio render endpoint")?;
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .context("IAudioClient activation failed")?;

        // SAFETY: the returned buffer is owned by us (freed below with
        // CoTaskMemFree) and valid until then.
        let mix = unsafe { client.GetMixFormat() }.context("GetMixFormat failed")?;
        anyhow::ensure!(!mix.is_null(), "GetMixFormat returned null");
        let parsed = parse_mix_format(mix);
        // SAFETY: loopback shared-mode init with the device's own mix
        // format; `mix` is still valid.
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                BUFFER_DURATION_HNS,
                0,
                mix,
                None,
            )
        }
        .context("IAudioClient::Initialize (loopback) failed")?;
        // SAFETY: `mix` came from GetMixFormat and is freed exactly once.
        unsafe { CoTaskMemFree(Some(mix.cast_const().cast())) };
        let mix = parsed?;

        info!(
            format = ?mix.format,
            channels = mix.channels,
            sample_rate = mix.sample_rate,
            "WASAPI loopback mix format"
        );
        let pipeline =
            DevicePipeline::new(mix.format, mix.channels, mix.sample_rate, out_channels, out_rate)
                .context("unsupported mix format")?;
        // SAFETY: the client is initialized and alive.
        let capture: IAudioCaptureClient =
            unsafe { client.GetService() }.context("IAudioCaptureClient service failed")?;
        // SAFETY: starting the capture loopback client.
        unsafe { client.Start() }.context("IAudioClient::Start failed")?;
        info!(out_rate, out_channels, "WASAPI loopback capture started");
        Ok(Self {
            client,
            capture,
            block_align: mix.format.bytes() * usize::from(mix.channels),
            pipeline,
        })
    }

    /// Polls packets and feeds the ring until `quit` is set or WASAPI fails.
    fn run(&mut self, shared: &Shared) {
        while !shared.quit.load(Ordering::Relaxed) {
            if let Err(e) = self.drain_packets(shared) {
                warn!("WASAPI capture failed: {e:#}");
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        // SAFETY: the client was started in `init`.
        let _ = unsafe { self.client.Stop() };
        info!("WASAPI loopback capture stopped");
    }

    /// Drains every pending packet into the shared ring.
    fn drain_packets(&mut self, shared: &Shared) -> anyhow::Result<()> {
        loop {
            // SAFETY: the capture client is alive; no out-params.
            let packet_frames =
                unsafe { self.capture.GetNextPacketSize() }.context("GetNextPacketSize failed")?;
            if packet_frames == 0 {
                return Ok(());
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            // SAFETY: all out-params are valid; the buffer is released
            // exactly once below, on every path.
            unsafe { self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
                .context("GetBuffer failed")?;
            let result = self.convert_packet(data, frames, flags, shared);
            // SAFETY: `frames` is the frame count obtained above.
            unsafe { self.capture.ReleaseBuffer(frames) }.context("ReleaseBuffer failed")?;
            result?;
        }
    }

    /// Converts one captured packet (already bounds-checked by frame count)
    /// and pushes it into the ring.
    fn convert_packet(
        &mut self,
        data: *mut u8,
        frames: u32,
        flags: u32,
        shared: &Shared,
    ) -> anyhow::Result<()> {
        let frames = usize::try_from(frames).unwrap_or(usize::MAX);
        anyhow::ensure!(frames == 0 || !data.is_null(), "GetBuffer returned null data");
        // SAFETY: `data` points to `frames * block_align` readable bytes
        // owned by the capture client until ReleaseBuffer (called after
        // this function returns).
        let bytes = if frames == 0 || data.is_null() {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(data, frames * self.block_align) }
        };
        let silent = flags & BUFFERFLAGS_SILENT != 0;
        // A poisoned ring must not kill capture: recover the inner state.
        let mut ring = shared.ring.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        self.pipeline.push_packet(bytes, frames, silent, &mut ring);
        Ok(())
    }
}

/// Reads the mix format returned by `GetMixFormat` (a `WAVEFORMATEX`,
/// usually a `WAVEFORMATEXTENSIBLE` on modern Windows).
fn parse_mix_format(mix: *const WAVEFORMATEX) -> anyhow::Result<MixFormat> {
    // SAFETY: `mix` is a valid GetMixFormat buffer. The structs are packed,
    // so read them by value with the unaligned accessors and copy every
    // field to a local (referencing a packed field is unaligned UB).
    let fmt = unsafe { mix.read_unaligned() };
    let (tag, channels_raw, sample_rate, bits, cb_size) = (
        fmt.wFormatTag,
        fmt.nChannels,
        fmt.nSamplesPerSec,
        fmt.wBitsPerSample,
        fmt.cbSize,
    );
    anyhow::ensure!(channels_raw >= 1, "mix format reports zero channels");
    let channels = u8::try_from(channels_raw).context("mix format channels > 255")?;
    let format = match tag {
        WAVE_FORMAT_IEEE_FLOAT_TAG => {
            anyhow::ensure!(bits == 32, "float mix format is not 32-bit");
            SampleFormat::F32
        }
        WAVE_FORMAT_PCM_TAG => {
            anyhow::ensure!(bits == 16, "PCM mix format is not 16-bit ({bits} bits)");
            SampleFormat::I16
        }
        WAVE_FORMAT_EXTENSIBLE_TAG => {
            // cbSize covers the fields beyond WAVEFORMATEX (22 bytes).
            anyhow::ensure!(cb_size >= 22, "truncated WAVEFORMATEXTENSIBLE");
            // SAFETY: same buffer, reinterpreted as the extensible layout;
            // cbSize checked above. Copied to a local (packed field).
            let sub_format = unsafe { mix.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() }.SubFormat;
            if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                SampleFormat::F32
            } else if sub_format == KSDATAFORMAT_SUBTYPE_PCM {
                anyhow::ensure!(bits == 16, "extensible PCM mix format is not 16-bit");
                SampleFormat::I16
            } else {
                bail!("unsupported mix subformat {sub_format:?}");
            }
        }
        other => bail!("unsupported mix format tag {other:#x}"),
    };
    Ok(MixFormat {
        format,
        channels,
        sample_rate,
    })
}

/// State shared between the capture thread and the audio thread.
struct Shared {
    ring: Mutex<CaptureRing>,
    /// Set by `Drop` to end the capture thread.
    quit: AtomicBool,
}

/// WASAPI loopback [`AudioSource`]: captures the default render endpoint's
/// output. See the module docs for the threading model.
pub struct WasapiLoopbackSource {
    sample_rate: u32,
    channels: u8,
    frame_samples: usize,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// Underrun samples synthesized so far (drives throttled warnings).
    underrun_samples: u64,
    /// Underrun total at the last warning (throttle).
    last_underrun_warn: u64,
}

impl WasapiLoopbackSource {
    /// Starts loopback capture producing the given wire shape (48 kHz stereo
    /// 20 ms frames for the Opus presets). Fails when there is no render
    /// endpoint or the audio service is unavailable.
    pub fn new(sample_rate: u32, channels: u8, frame_samples: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(sample_rate >= 8000, "implausible sample rate {sample_rate}");
        anyhow::ensure!((1..=2).contains(&channels), "channels must be 1 or 2");
        anyhow::ensure!(frame_samples > 0, "empty frames");
        let ring_samples =
            sample_rate as usize * usize::from(channels) * RING_MS as usize / 1000;
        let shared = Arc::new(Shared {
            ring: Mutex::new(CaptureRing::new(ring_samples)),
            quit: AtomicBool::new(false),
        });
        // Init (and all COM traffic) happens on the capture thread; the
        // outcome comes back over this channel so a failure is reported to
        // the caller instead of producing a silently-dead source.
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("wasapi-capture".to_string())
            .spawn(move || {
                match CaptureState::init(sample_rate, channels) {
                    Ok(mut state) => {
                        if ready_tx.send(Ok(())).is_ok() {
                            state.run(&thread_shared);
                        }
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
                // SAFETY: balances CoInitializeEx in `CaptureState::init`;
                // no COM object outlives this thread.
                unsafe { CoUninitialize() };
            })
            .context("cannot spawn the WASAPI capture thread")?;
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                shared.quit.store(true, Ordering::Relaxed);
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                shared.quit.store(true, Ordering::Relaxed);
                let _ = thread.join();
                bail!("WASAPI capture thread died during init");
            }
        }
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

impl Drop for WasapiLoopbackSource {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl AudioSource for WasapiLoopbackSource {
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
                    "WASAPI capture starved; emitting silence"
                );
                self.last_underrun_warn = self.underrun_samples.max(1);
            }
            // A finished capture thread can only produce silence from now
            // on: fail the source so the session drops audio loudly.
            if self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
                bail!("WASAPI capture thread ended");
            }
        }
        Ok(())
    }
}
