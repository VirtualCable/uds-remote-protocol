// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Windows implementation of udsaudio-ctl. See `main.rs` for the command
//! list. Everything here is Windows-only; COM objects never cross threads
//! (each WASAPI function inits its own apartment).

// The `windows` crate FFI methods take raw pointers; implicit borrows are
// the idiomatic call style for that API (same policy as the server crate).
#![allow(clippy::borrow_as_ptr)]

use std::f32::consts::PI;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, GENERIC_WRITE, HANDLE};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    DEVICE_STATE_ACTIVE, IAudioCaptureClient,
    IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eCapture, eRender,
};
use windows::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToString;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::{PCWSTR, w};

/// `IOCTL_UDSAUDIO_PUSH_MIC` — CTL_CODE(FILE_DEVICE_UNKNOWN=34, 0x800,
/// METHOD_BUFFERED, FILE_ANY_ACCESS). Must match `logic::IOCTL_PUSH_MIC` in
/// the driver.
const IOCTL_PUSH_MIC: u32 = 0x0022_2000;

/// NT path of the driver's control device (DOS symlink).
const CONTROL_PATH: PCWSTR = w!("\\\\.\\UDSAudioCtl");

/// Friendly-name substring of the UDS endpoints (from udsaudio.inx).
const UDS_NAME_NEEDLE: &str = "UDS Virtual Audio Device";

/// Mic wire format: 48 kHz mono s16 (what the driver ring expects).
const MIC_RATE: u32 = 48_000;

/// Pacing quantum for the tone pusher (ms of wall time between pushes).
const PUSH_INTERVAL: Duration = Duration::from_millis(10);

/// WASAPI poll interval for capture loops.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Shared-mode buffer request: 200 ms in 100 ns units.
const BUFFER_DURATION_HNS: i64 = 2_000_000;

/// `AUDCLNT_BUFFERFLAGS_SILENT`.
const BUFFERFLAGS_SILENT: u32 = 0x2;

/// `wFormatTag` values (the windows crate exposes them u32-typed; local
/// copies keep compares literal).
const WAVE_FORMAT_PCM_TAG: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT_TAG: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE_TAG: u16 = 0xFFFE;

/// CLI entry.
pub fn run() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map_or("help", String::as_str);
    match cmd {
        "list" => list_endpoints(),
        "push-tone" => {
            let (seconds, freq) = parse_opts(&args[1..])?;
            push_tone(seconds, freq)
        }
        "verify-mic" => {
            let (seconds, freq) = parse_opts(&args[1..])?;
            verify_mic(seconds, freq)
        }
        "verify-loopback" => {
            let (seconds, freq) = parse_opts(&args[1..])?;
            verify_loopback(seconds, freq)
        }
        "probe-capture" => {
            // Opens a capture stream on the first active endpoint matching
            // the needle (default: UDS) and reports the Initialize result.
            let needle = args.get(1).map_or(UDS_NAME_NEEDLE, String::as_str);
            probe_capture(needle)
        }
        "render-tone" => {
            let (seconds, freq) = parse_opts(&args[1..])?;
            // Render only — no loopback capture, so the driver ring keeps a
            // single consumer (e.g. the server's IOCTL reader under test).
            let _com = ComGuard::init()?;
            let device = find_endpoint(false, UDS_NAME_NEEDLE)?;
            render_tone(&device, seconds, freq, &AtomicBool::new(false))
        }
        _ => {
            eprintln!(
                "usage: udsaudio-ctl <list | push-tone | verify-mic | verify-loopback | render-tone | probe-capture [needle]> [seconds] [freq]"
            );
            std::process::exit(64);
        }
    }
}

/// Parses `[seconds] [freq]` with defaults (5 s, 440 Hz).
fn parse_opts(args: &[String]) -> anyhow::Result<(f32, f32)> {
    let seconds = match args.first() {
        Some(s) => s.parse().context("seconds must be a number")?,
        None => 5.0,
    };
    let freq = match args.get(1) {
        Some(s) => s.parse().context("freq must be a number")?,
        None => 440.0,
    };
    Ok((seconds, freq))
}

// --- tone synthesis ---------------------------------------------------------

/// Simple phase-accumulating sine generator (f32 samples in [-0.5, 0.5]).
struct Tone {
    freq: f32,
    rate: u32,
    pos: u64,
}

impl Tone {
    fn new(freq: f32, rate: u32) -> Self {
        Self { freq, rate, pos: 0 }
    }

    #[allow(clippy::cast_precision_loss)]
    fn next(&mut self) -> f32 {
        let t = self.pos as f32 / self.rate as f32;
        self.pos += 1;
        0.5 * (2.0 * PI * self.freq * t).sin()
    }
}

// --- control device (mic feed) ----------------------------------------------

/// RAII wrapper for the control-device handle.
struct ControlDevice(HANDLE);

impl ControlDevice {
    fn open() -> anyhow::Result<Self> {
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
        Ok(Self(handle))
    }

    /// Pushes one chunk of mic PCM; returns how many bytes the ring took.
    fn push(&self, pcm: &[u8]) -> anyhow::Result<u32> {
        let mut returned = 0u32;
        // SAFETY: the input buffer is valid for `pcm.len()` bytes; no output
        // buffer is used; `returned` is a valid out pointer.
        unsafe {
            DeviceIoControl(
                self.0,
                IOCTL_PUSH_MIC,
                Some(pcm.as_ptr().cast()),
                u32::try_from(pcm.len())?,
                None,
                0,
                Some(&raw mut returned),
                None,
            )
        }
        .context("DeviceIoControl(IOCTL_PUSH_MIC) failed")?;
        Ok(returned)
    }
}

impl Drop for ControlDevice {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and open.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Pushes a tone into the mic ring in real time for `seconds`; returns
/// (bytes pushed, bytes accepted by the ring).
fn push_tone(seconds: f32, freq: f32) -> anyhow::Result<()> {
    let device = ControlDevice::open()?;
    let (pushed, accepted) = run_pusher(&device, seconds, freq, None);
    println!("pushed {pushed} bytes, ring accepted {accepted}");
    if accepted * 2 < pushed {
        println!("warning: the ring dropped more than half of the audio (consumer too slow?)");
    }
    Ok(())
}

/// The pusher loop: sends as much PCM as the wall clock dictates, in chunks,
/// until `seconds` elapse or `stop` is set. Returns (pushed, accepted).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn run_pusher(
    device: &ControlDevice,
    seconds: f32,
    freq: f32,
    stop: Option<&AtomicBool>,
) -> (u64, u64) {
    let mut tone = Tone::new(freq, MIC_RATE);
    let start = Instant::now();
    let limit = Duration::from_secs_f32(seconds);
    let mut pushed = 0u64;
    let mut accepted = 0u64;
    // 2 bytes per frame, MIC_RATE frames per second.
    loop {
        let elapsed = start.elapsed();
        if elapsed >= limit || stop.is_some_and(|s| s.load(Ordering::Relaxed)) {
            break;
        }
        let due_bytes = (elapsed.as_secs_f64() * f64::from(MIC_RATE * 2)) as u64;
        let mut pending = due_bytes.saturating_sub(pushed);
        pending -= pending % 2; // whole frames
        // Cap a single push at 4 KB (kernel METHOD_BUFFERED copies it).
        pending = pending.min(4096);
        if pending > 0 {
            let frames = (pending / 2) as usize;
            let mut buf = Vec::with_capacity(pending as usize);
            for _ in 0..frames {
                buf.extend_from_slice(&((tone.next() * 32767.0) as i16).to_le_bytes());
            }
            match device.push(&buf) {
                Ok(n) => accepted += u64::from(n),
                Err(e) => {
                    eprintln!("push failed: {e:#}");
                    break;
                }
            }
            pushed += pending;
        }
        std::thread::sleep(PUSH_INTERVAL);
    }
    (pushed, accepted)
}

// --- WASAPI helpers -----------------------------------------------------------

/// A parsed mix format: PCM kind, channels, rate.
struct MixFormat {
    is_f32: bool,
    channels: u16,
    rate: u32,
}

impl MixFormat {
    fn block_align(&self) -> usize {
        usize::from(self.channels) * if self.is_f32 { 4 } else { 2 }
    }
}

/// Reads the mix format returned by `IAudioClient::GetMixFormat`
/// (same parsing as the server's `parse_mix_format`).
fn parse_mix_format(mix: *const WAVEFORMATEX) -> anyhow::Result<MixFormat> {
    // SAFETY: `mix` is a valid GetMixFormat buffer. Packed structs: copy
    // fields to locals via unaligned reads.
    let fmt = unsafe { mix.read_unaligned() };
    let (tag, channels, rate, bits, cb_size) = (
        fmt.wFormatTag,
        fmt.nChannels,
        fmt.nSamplesPerSec,
        fmt.wBitsPerSample,
        fmt.cbSize,
    );
    let is_f32 = match tag {
        WAVE_FORMAT_IEEE_FLOAT_TAG => true,
        WAVE_FORMAT_PCM_TAG => {
            anyhow::ensure!(bits == 16, "PCM mix format is not 16-bit ({bits} bits)");
            false
        }
        WAVE_FORMAT_EXTENSIBLE_TAG => {
            anyhow::ensure!(cb_size >= 22, "truncated WAVEFORMATEXTENSIBLE");
            // SAFETY: same buffer reinterpreted; cbSize checked above.
            let sub = unsafe { mix.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() }.SubFormat;
            if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                true
            } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
                anyhow::ensure!(bits == 16, "extensible PCM mix format is not 16-bit");
                false
            } else {
                bail!("unsupported mix subformat {sub:?}");
            }
        }
        other => bail!("unsupported mix format tag {other:#x}"),
    };
    Ok(MixFormat { is_f32, channels, rate })
}

/// Per-thread COM guard.
struct ComGuard;

impl ComGuard {
    fn init() -> anyhow::Result<Self> {
        // SAFETY: standard per-thread COM init; S_FALSE is accepted by .ok().
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .context("CoInitializeEx failed")?;
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: balances the successful CoInitializeEx.
        unsafe { CoUninitialize() };
    }
}

/// Finds the active endpoint (render or capture) whose friendly name
/// contains `needle`.
fn find_endpoint(capture: bool, needle: &str) -> anyhow::Result<IMMDevice> {
    // SAFETY: coclass activation with valid out-params.
    let enumerator: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .context("MMDeviceEnumerator creation failed")?;
    let flow = if capture { eCapture } else { eRender };
    // SAFETY: the enumerator is alive.
    let collection = unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) }
        .context("EnumAudioEndpoints failed")?;
    // SAFETY: the collection is alive.
    let count = unsafe { collection.GetCount() }.context("IMMDeviceCollection::GetCount")?;
    for i in 0..count {
        // SAFETY: index in range.
        let device = unsafe { collection.Item(i) }.context("IMMDeviceCollection::Item")?;
        if let Ok(name) = friendly_name(&device)
            && name.contains(needle)
        {
            println!("using endpoint: {name}");
            return Ok(device);
        }
    }
    bail!("no active endpoint with '{needle}' in its name (run `udsaudio-ctl list`)");
}

/// Reads `PKEY_Device_FriendlyName` of an endpoint.
fn friendly_name(device: &IMMDevice) -> anyhow::Result<String> {
    // SAFETY: the device is alive; STGM_READ is the correct mode.
    let store = unsafe { device.OpenPropertyStore(STGM_READ) }.context("OpenPropertyStore")?;
    let key = PKEY_Device_FriendlyName;
    // SAFETY: the store is alive; `key` is a valid PROPERTYKEY pointer.
    let value = unsafe { store.GetValue(&raw const key) }.context("GetValue")?;
    let mut buf = [0u16; 256];
    // SAFETY: `value` is a valid PROPVARIANT; `buf` is a valid wide buffer.
    unsafe { PropVariantToString(&raw const value, &mut buf) }.context("PropVariantToString")?;
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(String::from_utf16_lossy(&buf[..end]))
}

/// Lists every active render and capture endpoint.
fn list_endpoints() -> anyhow::Result<()> {
    let _com = ComGuard::init()?;
    // SAFETY: coclass activation with valid out-params.
    let enumerator: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .context("MMDeviceEnumerator creation failed")?;
    for (label, flow) in [("render", eRender), ("capture", eCapture)] {
        println!("{label}:");
        // SAFETY: the enumerator is alive.
        let collection = unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) }
            .context("EnumAudioEndpoints failed")?;
        // SAFETY: the collection is alive.
        let count = unsafe { collection.GetCount() }.unwrap_or(0);
        for i in 0..count {
            // SAFETY: index in range.
            let device = unsafe { collection.Item(i) };
            if let Ok(device) = device {
                let name = friendly_name(&device).unwrap_or_else(|_| "<unnamed>".to_string());
                let uds = if name.contains(UDS_NAME_NEEDLE) { "   <-- UDS" } else { "" };
                println!("  {name}{uds}");
            }
        }
    }
    Ok(())
}

/// Captures `seconds` of audio from `device` (loopback when `loopback` is
/// set, plain capture otherwise) and returns the samples as mono f32
/// (channel 0) plus the mix rate.
fn capture_samples(device: &IMMDevice, seconds: f32, loopback: bool) -> anyhow::Result<(Vec<f32>, u32)> {
    // SAFETY: activation on a live device.
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
        .context("IAudioClient activation failed")?;
    // SAFETY: the client is alive; freed below with CoTaskMemFree.
    let mix = unsafe { client.GetMixFormat() }.context("GetMixFormat failed")?;
    anyhow::ensure!(!mix.is_null(), "GetMixFormat returned null");
    let parsed = parse_mix_format(mix)?;
    println!(
        "mix format: {} {} ch {} Hz",
        if parsed.is_f32 { "f32" } else { "s16" },
        parsed.channels,
        parsed.rate
    );
    let flags = if loopback { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
    // SAFETY: shared-mode init with the device's own mix format.
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_DURATION_HNS, 0, mix, None) }
        .context("IAudioClient::Initialize failed")?;
    // SAFETY: `mix` came from GetMixFormat and is freed exactly once.
    unsafe { CoTaskMemFree(Some(mix.cast_const().cast())) };
    // SAFETY: the client is initialized.
    let capture: IAudioCaptureClient = unsafe { client.GetService() }.context("IAudioCaptureClient")?;
    // SAFETY: starting an initialized client.
    unsafe { client.Start() }.context("IAudioClient::Start")?;

    let mut samples = Vec::new();
    let deadline = Instant::now() + Duration::from_secs_f32(seconds);
    let block = parsed.block_align();
    while Instant::now() < deadline {
        // SAFETY: the capture client is alive; no out-params.
        let packet = unsafe { capture.GetNextPacketSize() }.context("GetNextPacketSize")?;
        if packet == 0 {
            std::thread::sleep(POLL_INTERVAL);
            continue;
        }
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut frames = 0u32;
        let mut flags = 0u32;
        // SAFETY: all out-params valid; buffer released below on all paths.
        unsafe { capture.GetBuffer(&raw mut data, &raw mut frames, &raw mut flags, None, None) }
            .context("GetBuffer")?;
        let frames_usize = frames as usize;
        if frames_usize > 0 {
            if flags & BUFFERFLAGS_SILENT != 0 || data.is_null() {
                samples.resize(samples.len() + frames_usize, 0.0);
            } else {
                // SAFETY: `data` points to frames*block readable bytes owned
                // by the capture client until ReleaseBuffer.
                let bytes =
                    unsafe { std::slice::from_raw_parts(data, frames_usize * block) };
                for frame in 0..frames_usize {
                    let off = frame * block;
                    let s = if parsed.is_f32 {
                        f32::from_le_bytes(bytes[off..off + 4].try_into()?)
                    } else {
                        f32::from(i16::from_le_bytes(bytes[off..off + 2].try_into()?)) / 32768.0
                    };
                    samples.push(s);
                }
            }
        }
        // SAFETY: `frames` is the count obtained above.
        unsafe { capture.ReleaseBuffer(frames) }.context("ReleaseBuffer")?;
    }
    // SAFETY: stopping a started client.
    unsafe { client.Stop() }.ok();
    Ok((samples, parsed.rate))
}

// --- analysis ---------------------------------------------------------------

/// Goertzel power at `freq` over `samples`, plus overall RMS.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn analyze(samples: &[f32], rate: u32, freq: f32) -> (f32, f32, f32) {
    let w = 2.0 * PI * freq / rate as f32;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0f32, 0f32);
    let mut sum_sq = 0f64;
    for &x in samples {
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
        sum_sq += f64::from(x) * f64::from(x);
    }
    let power = s1.mul_add(s1, s2 * s2) - coeff * s1 * s2;
    let rms = (sum_sq / samples.len().max(1) as f64).sqrt() as f32;
    // Ratio of tone power to total signal power (0..1 for a pure tone).
    let total = sum_sq as f32;
    let ratio = if total > 0.0 { (power / total).clamp(0.0, 1.0) } else { 0.0 };
    (ratio, rms, power)
}

/// Prints the analysis and exits the process 0 (PASS) / 2 (FAIL).
fn verdict(what: &str, samples: &[f32], rate: u32, freq: f32) -> ! {
    let (ratio, rms, _power) = analyze(samples, rate, freq);
    println!("{what}: {} samples, rms={rms:.5}, tone ratio={ratio:.3}", samples.len());
    if ratio > 0.3 && rms > 0.005 {
        println!("PASS: {freq} Hz tone detected in {what}");
        std::process::exit(0);
    }
    println!("FAIL: no {freq} Hz tone in {what}");
    std::process::exit(2);
}

// --- E2E subcommands ----------------------------------------------------------

/// Diagnostics: opens a shared-mode capture stream on the first capture
/// endpoint matching `needle` and reports whether `Initialize` succeeds.
/// Used to isolate endpoint/AudioDG failures from ring content issues.
fn probe_capture(needle: &str) -> anyhow::Result<()> {
    let _com = ComGuard::init()?;
    let device = find_endpoint(true, needle)?;
    // SAFETY: activation on a live device.
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
        .context("IAudioClient activation failed")?;
    // SAFETY: the client is alive; freed below with CoTaskMemFree.
    let mix = unsafe { client.GetMixFormat() }.context("GetMixFormat failed")?;
    anyhow::ensure!(!mix.is_null(), "GetMixFormat returned null");
    let parsed = parse_mix_format(mix)?;
    println!(
        "mix format: {} {} ch {} Hz",
        if parsed.is_f32 { "f32" } else { "s16" },
        parsed.channels,
        parsed.rate
    );
    // SAFETY: shared-mode init with the device's own mix format.
    let init = unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, BUFFER_DURATION_HNS, 0, mix, None) };
    // SAFETY: `mix` came from GetMixFormat and is freed exactly once.
    unsafe { CoTaskMemFree(Some(mix.cast_const().cast())) };
    match init {
        Ok(()) => {
            println!("probe: IAudioClient::Initialize (shared) OK");
            // SAFETY: starting/stopping an initialized client.
            let start = unsafe { client.Start() };
            println!("probe: IAudioClient::Start -> {start:?}");
            unsafe { client.Stop() }.ok();
        }
        Err(e) => {
            println!("probe: IAudioClient::Initialize (shared) failed: {e:?}");
        }
    }
    // Exclusive mode bypasses the AudioDG shared engine: if this succeeds
    // while shared fails, the block lives in the audio service path, not
    // in the driver or the KS pin security.
    let client2: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
        .context("IAudioClient activation failed")?;
    let mix2 = unsafe { client2.GetMixFormat() }.context("GetMixFormat failed")?;
    let init2 = unsafe {
        client2.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, 0, BUFFER_DURATION_HNS, BUFFER_DURATION_HNS, mix2, None)
    };
    unsafe { CoTaskMemFree(Some(mix2.cast_const().cast())) };
    match init2 {
        Ok(()) => println!("probe: IAudioClient::Initialize (exclusive) OK"),
        Err(e) => println!("probe: IAudioClient::Initialize (exclusive) failed: {e:?}"),
    }
    Ok(())
}


/// Pushes the tone into the mic control device while capturing from the UDS
/// microphone endpoint; the captured signal must contain the tone.
fn verify_mic(seconds: f32, freq: f32) -> anyhow::Result<()> {
    let stop = AtomicBool::new(false);
    let mut samples = None;
    std::thread::scope(|s| -> anyhow::Result<()> {
        // Pusher thread: owns its own control-device handle, no COM needed.
        let pusher_thread = s.spawn(|| -> anyhow::Result<(u64, u64)> {
            let device = ControlDevice::open()?;
            Ok(run_pusher(&device, seconds + 1.0, freq, Some(&stop)))
        });
        // Give the pusher a head start so the ring has data when capture starts.
        std::thread::sleep(Duration::from_millis(500));
        let result = (|| -> anyhow::Result<(Vec<f32>, u32)> {
            let _com = ComGuard::init()?;
            let device = find_endpoint(true, UDS_NAME_NEEDLE)?;
            capture_samples(&device, seconds, false)
        })();
        stop.store(true, Ordering::Relaxed);
        let (pushed, accepted) = pusher_thread.join().expect("pusher thread panicked")?;
        println!("pusher: pushed {pushed} bytes, ring accepted {accepted}");
        samples = Some(result?);
        Ok(())
    })?;
    let (samples, rate) = samples.expect("set above");
    verdict("mic capture", &samples, rate, freq)
}

/// Renders the tone to the UDS speaker while capturing its loopback; the
/// loopback signal must contain the tone (proves the sink→loopback copy).
fn verify_loopback(seconds: f32, freq: f32) -> anyhow::Result<()> {
    let stop = AtomicBool::new(false);
    let mut samples = None;
    std::thread::scope(|s| -> anyhow::Result<()> {
        let renderer = s.spawn(|| -> anyhow::Result<()> {
            let _com = ComGuard::init()?;
            let device = find_endpoint(false, UDS_NAME_NEEDLE)?;
            render_tone(&device, seconds + 1.0, freq, &stop)
        });
        // Let the render engine reach RUN state before capturing.
        std::thread::sleep(Duration::from_millis(500));
        let result = (|| -> anyhow::Result<(Vec<f32>, u32)> {
            let _com = ComGuard::init()?;
            let device = find_endpoint(false, UDS_NAME_NEEDLE)?;
            capture_samples(&device, seconds, true)
        })();
        stop.store(true, Ordering::Relaxed);
        renderer.join().expect("render thread panicked")?;
        samples = Some(result?);
        Ok(())
    })?;
    let (samples, rate) = samples.expect("set above");
    verdict("loopback capture", &samples, rate, freq)
}

/// Renders a tone to `device` (shared mode, mix format) until `seconds`
/// elapse or `stop` is set.
#[allow(clippy::cast_possible_truncation)]
fn render_tone(device: &IMMDevice, seconds: f32, freq: f32, stop: &AtomicBool) -> anyhow::Result<()> {
    // SAFETY: activation on a live device.
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
        .context("IAudioClient activation failed")?;
    // SAFETY: the client is alive; freed below with CoTaskMemFree.
    let mix = unsafe { client.GetMixFormat() }.context("GetMixFormat failed")?;
    anyhow::ensure!(!mix.is_null(), "GetMixFormat returned null");
    let parsed = parse_mix_format(mix)?;
    // SAFETY: shared-mode init with the device's own mix format.
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, BUFFER_DURATION_HNS, 0, mix, None) }
        .context("IAudioClient::Initialize failed")?;
    // SAFETY: `mix` came from GetMixFormat and is freed exactly once.
    unsafe { CoTaskMemFree(Some(mix.cast_const().cast())) };
    // SAFETY: the client is initialized.
    let render: IAudioRenderClient = unsafe { client.GetService() }.context("IAudioRenderClient")?;
    // SAFETY: the client is initialized.
    let buffer_frames = unsafe { client.GetBufferSize() }.context("GetBufferSize")?;
    // SAFETY: starting an initialized client.
    unsafe { client.Start() }.context("IAudioClient::Start")?;

    let mut tone = Tone::new(freq, parsed.rate);
    let block = parsed.block_align();
    let deadline = Instant::now() + Duration::from_secs_f32(seconds);
    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        // SAFETY: the client is alive.
        let padding = unsafe { client.GetCurrentPadding() }.context("GetCurrentPadding")?;
        let avail = buffer_frames.saturating_sub(padding);
        if avail > 0 {
            // SAFETY: the render client is alive; the returned buffer holds
            // `avail` frames of `block` bytes and is released below.
            let data = unsafe { render.GetBuffer(avail) }.context("IAudioRenderClient::GetBuffer")?;
            anyhow::ensure!(!data.is_null(), "GetBuffer returned null");
            // SAFETY: writes stay within the buffer (avail*block bytes).
            unsafe {
                for frame in 0..avail as usize {
                    let sample = tone.next();
                    for ch in 0..usize::from(parsed.channels) {
                        let off = frame * block + ch * if parsed.is_f32 { 4 } else { 2 };
                        let dst = data.add(off);
                        if parsed.is_f32 {
                            dst.cast::<f32>().write_unaligned(sample);
                        } else {
                            let v = (sample * 32767.0) as i16;
                            dst.copy_from_nonoverlapping(v.to_le_bytes().as_ptr(), 2);
                        }
                    }
                }
            }
            // SAFETY: `avail` frames written above.
            unsafe { render.ReleaseBuffer(avail, 0) }.context("ReleaseBuffer")?;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    // SAFETY: stopping a started client.
    unsafe { client.Stop() }.ok();
    Ok(())
}
