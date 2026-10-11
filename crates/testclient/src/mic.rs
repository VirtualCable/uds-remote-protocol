//! Microphone capture pipeline (design doc §9.2), the mirror image of the
//! audio-out playback pipeline in `audio.rs`:
//!
//! ```text
//! mic source (cpal input device or test tone) ─▶ mic thread, 20 ms pacing:
//!   fill_frame ─▶ Opus encode (announced format, 48 kHz mono) ─▶ datagram
//!   payload ─▶ mpsc ─▶ session loop sends it on AUDIO_IN
//! ```
//!
//! Capture only starts after the server announces audio-in support
//! (`AudioInConfig`, §9.2): a server without a real mic sink must never
//! receive audio. Without the `audio` feature there is no cpal and no Opus;
//! the test tone still works wherever the encoder does.

#[cfg(feature = "audio")]
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use codec::{AudioCodecType, AudioPreset};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::audio::AudioSpec;

/// Mic mode from the CLI (`--mic` / `--mic-test-tone`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MicMode {
    /// No mic (default: never hot-mic the user by surprise).
    Off,
    /// Capture the default input device.
    Device,
    /// Capture replaced by a synthetic sine of the given frequency in Hz
    /// (deterministic E2E without audio hardware).
    Tone(f64),
}

/// A source of mic frames: interleaved signed 16-bit mono PCM, exactly
/// `frame_samples` samples per frame (the audio-in wire shape, §9.2).
trait MicSource: Send {
    /// Fills `out` with the next frame; `out` holds exactly one frame.
    fn fill_frame(&mut self, out: &mut [i16]) -> anyhow::Result<()>;
}

/// Synthetic mic source: a single sine tone with continuous phase across
/// frames (mono counterpart of the server's `SyntheticSource`).
struct ToneSource {
    freq: f64,
    frame_samples: usize,
    /// Phase accumulator (radians), kept across frames.
    phase: f64,
}

impl ToneSource {
    fn new(freq: f64, frame_samples: usize) -> Self {
        Self {
            freq,
            frame_samples,
            phase: 0.0,
        }
    }
}

/// Test-tone amplitude as a fraction of the i16 range (same level as the
/// server's synthetic source).
const TONE_AMPLITUDE: f64 = 0.3;

impl MicSource for ToneSource {
    fn fill_frame(&mut self, out: &mut [i16]) -> anyhow::Result<()> {
        anyhow::ensure!(
            out.len() == self.frame_samples,
            "mic frame buffer must be exactly {} samples, got {}",
            self.frame_samples,
            out.len()
        );
        let step = self.freq * 2.0 * std::f64::consts::PI / 48_000.0;
        for s in out.iter_mut() {
            // The sine is bounded; the cast cannot truncate meaningfully.
            #[allow(clippy::cast_possible_truncation)]
            let v = (self.phase.sin() * TONE_AMPLITUDE * f64::from(i16::MAX)) as i16;
            *s = v;
            self.phase += step;
            if self.phase > 2.0 * std::f64::consts::PI {
                self.phase -= 2.0 * std::f64::consts::PI;
            }
        }
        Ok(())
    }
}

/// cpal input-device source (`audio` feature): the input callback converts
/// every sample format to f32 bytes and pushes them through the shared DSP
/// chain (remix to mono, resample to 48 kHz) into a bounded ring;
/// `fill_frame` drains exactly one frame, emitting silence on underrun.
#[cfg(feature = "audio")]
struct CpalMicSource {
    ring: Arc<Mutex<codec::audio_dsp::CaptureRing>>,
    /// Keeps the input stream alive; dropped with the source.
    _stream: cpal::Stream,
    frame_samples: usize,
    /// Samples synthesized as silence on underrun (throttled warnings).
    underrun_samples: u64,
    /// Underrun total at the last warning.
    last_underrun_warn: u64,
}

/// Underrun warning cadence: first underrun, then at most one warning per
/// ~10 s of synthesized silence (480 000 mono samples).
#[cfg(feature = "audio")]
const UNDERRUN_WARN_SAMPLES: u64 = 480_000;

#[cfg(feature = "audio")]
impl CpalMicSource {
    /// Opens the default input device; returns `None` (with a warning) when
    /// there is no input device or it cannot be opened — the mic then stays
    /// off for the session.
    fn new(frame_samples: usize) -> Option<Self> {
        use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};

        let host = cpal::default_host();
        let Some(device) = host.default_input_device() else {
            warn!("no audio input device; mic disabled");
            return None;
        };
        let config = match device.default_input_config() {
            Ok(c) => c,
            Err(e) => {
                warn!("cannot read input device config: {e}; mic disabled");
                return None;
            }
        };
        let in_rate = config.sample_rate();
        let in_channels = u8::try_from(config.channels()).unwrap_or(u8::MAX);
        // The wire format is fixed 48 kHz mono (§9.2); the DSP chain adapts.
        let pipeline = match codec::audio_dsp::DevicePipeline::new(
            codec::audio_dsp::SampleFormat::F32,
            in_channels,
            in_rate,
            1,
            48_000,
        ) {
            Ok(p) => p,
            Err(e) => {
                warn!("unsupported input device shape ({e}); mic disabled");
                return None;
            }
        };
        info!(
            input_rate = in_rate,
            input_channels = in_channels,
            "mic capture opened"
        );
        // Ring holds 500 ms of 48 kHz mono, mirroring the server-side policy.
        let ring = Arc::new(Mutex::new(codec::audio_dsp::CaptureRing::new(48_000 / 2)));
        let stream = match build_input_stream(&device, &config, pipeline, Arc::clone(&ring)) {
            Ok(s) => s,
            Err(e) => {
                warn!("cannot open audio input stream: {e}; mic disabled");
                return None;
            }
        };
        if let Err(e) = stream.play() {
            warn!("cannot start mic capture: {e}; mic disabled");
            return None;
        }
        Some(Self {
            ring,
            _stream: stream,
            frame_samples,
            underrun_samples: 0,
            last_underrun_warn: 0,
        })
    }
}

#[cfg(feature = "audio")]
impl MicSource for CpalMicSource {
    fn fill_frame(&mut self, out: &mut [i16]) -> anyhow::Result<()> {
        anyhow::ensure!(
            out.len() == self.frame_samples,
            "mic frame buffer must be exactly {} samples, got {}",
            self.frame_samples,
            out.len()
        );
        let missing = {
            // A poisoned ring degrades to silence, never a panic on the
            // audio path.
            let mut ring = self.ring.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
                    "mic capture starved; emitting silence"
                );
                self.last_underrun_warn = self.underrun_samples.max(1);
            }
        }
        Ok(())
    }
}

/// Builds an input stream typed to the config's sample format. The callback
/// normalizes every format to f32 (the DSP chain's input format), converts
/// to little-endian bytes and feeds the shared pipeline + ring; it never
/// panics and degrades to a dropped packet on a poisoned lock.
#[cfg(feature = "audio")]
fn build_input_stream_typed<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut pipeline: codec::audio_dsp::DevicePipeline,
    ring: Arc<Mutex<codec::audio_dsp::CaptureRing>>,
    channels: usize,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample + Send + 'static,
    f32: cpal::FromSample<T>,
{
    use cpal::traits::DeviceTrait as _;

    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let mut bytes = Vec::with_capacity(data.len() * 4);
            for s in data {
                bytes.extend_from_slice(&s.to_sample::<f32>().to_le_bytes());
            }
            let frames = data.len() / channels.max(1);
            let mut ring = ring.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            pipeline.push_packet(&bytes, frames, false, &mut ring);
        },
        // Device errors can fire once per period on a misbehaving driver;
        // log the first and then every 500th.
        {
            let mut errors = 0u64;
            move |err| {
                if errors == 0 || errors.is_multiple_of(500) {
                    warn!("mic stream error: {err}");
                }
                errors += 1;
            }
        },
        None,
    )
}

/// Builds the cpal input stream, dispatching on the config's sample format.
#[cfg(feature = "audio")]
fn build_input_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    pipeline: codec::audio_dsp::DevicePipeline,
    ring: Arc<Mutex<codec::audio_dsp::CaptureRing>>,
) -> Result<cpal::Stream, cpal::Error> {
    use cpal::SampleFormat;
    let stream_config = config.config();
    let channels = usize::from(config.channels());
    match config.sample_format() {
        SampleFormat::F32 => {
            build_input_stream_typed::<f32>(device, stream_config, pipeline, ring, channels)
        }
        SampleFormat::I16 => {
            build_input_stream_typed::<i16>(device, stream_config, pipeline, ring, channels)
        }
        SampleFormat::I32 => {
            build_input_stream_typed::<i32>(device, stream_config, pipeline, ring, channels)
        }
        SampleFormat::U8 => {
            build_input_stream_typed::<u8>(device, stream_config, pipeline, ring, channels)
        }
        SampleFormat::U16 => {
            build_input_stream_typed::<u16>(device, stream_config, pipeline, ring, channels)
        }
        SampleFormat::F64 => {
            build_input_stream_typed::<f64>(device, stream_config, pipeline, ring, channels)
        }
        other => Err(cpal::Error::with_message(
            cpal::ErrorKind::UnsupportedConfig,
            format!("unsupported input sample format {other:?}"),
        )),
    }
}

/// Client-side mic entry point: created from the CLI mode, started when the
/// server announces audio-in support, drained by the session loop.
pub struct MicClient {
    mode: MicMode,
    clock_start: Instant,
    /// Receives encoded datagram payloads once the pipeline is running.
    rx: Option<mpsc::Receiver<Vec<u8>>>,
}

impl MicClient {
    /// Creates the client; capture starts on [`MicClient::enable`].
    #[must_use]
    pub fn new(mode: MicMode, clock_start: Instant) -> Self {
        Self {
            mode,
            clock_start,
            rx: None,
        }
    }

    /// Applies the server's audio-in announcement (§9.2): builds the encoder
    /// for the announced format and starts the capture thread. A second
    /// announcement while running is ignored (the PoC format is fixed).
    pub fn enable(&mut self, spec: &AudioSpec) {
        if self.rx.is_some() || self.mode == MicMode::Off {
            return;
        }
        let Some(codec_type) = AudioCodecType::from_wire_id(spec.codec) else {
            warn!(codec = spec.codec, "unknown audio-in codec id; mic disabled");
            return;
        };
        let preset = AudioPreset {
            codec: codec_type,
            sample_rate: spec.sample_rate,
            channels: spec.channels,
            bitrate_kbps: spec.bitrate_kbps,
            complexity: 5, // encoder knob; the PoC accepts the default
            frame_ms: spec.frame_ms,
            fec: spec.fec,
        };
        let mut encoder = match codec::build_audio_encoder(&preset) {
            Ok(e) => e,
            Err(e) => {
                warn!("audio-in encoder unavailable: {e}; mic disabled");
                return;
            }
        };
        let frame_samples = preset.frame_samples();
        let source: Box<dyn MicSource> = match self.mode {
            MicMode::Off => unreachable!("checked above"),
            MicMode::Tone(freq) => Box::new(ToneSource::new(freq, frame_samples)),
            MicMode::Device => {
                #[cfg(feature = "audio")]
                match CpalMicSource::new(frame_samples) {
                    Some(s) => Box::new(s),
                    None => return, // already warned
                }
                #[cfg(not(feature = "audio"))]
                {
                    warn!("mic capture needs the `audio` feature (cpal); mic disabled");
                    return;
                }
            }
        };
        let (tx, rx) = mpsc::channel(8);
        let frame_time = Duration::from_millis(u64::from(preset.frame_ms));
        let clock_start = self.clock_start;
        std::thread::spawn(move || run(source, &mut *encoder, &tx, frame_samples, frame_time, clock_start));
        info!(
            sample_rate = spec.sample_rate,
            channels = spec.channels,
            bitrate_kbps = spec.bitrate_kbps,
            "audio-in announced by server; mic capture started"
        );
        self.rx = Some(rx);
    }

    /// Receives the next encoded datagram payload; pends forever while the
    /// mic is off, so it can sit in a `select!` branch.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        match &mut self.rx {
            Some(rx) => rx.recv().await,
            None => std::future::pending().await,
        }
    }

    /// Disables the mic (e.g. after the capture thread died); a later
    /// announcement re-enables it.
    pub fn disable(&mut self) {
        self.rx = None;
    }
}

/// Mic thread main loop: one frame per tick (absolute pacing, like the
/// server's audio thread), Opus-encoded and wrapped in the audio datagram
/// header (`seq` from 0, `capture_ts_ms` from the client clock).
fn run(
    mut source: Box<dyn MicSource>,
    encoder: &mut dyn codec::AudioEncoder,
    tx: &mpsc::Sender<Vec<u8>>,
    frame_samples: usize,
    frame_time: Duration,
    clock_start: Instant,
) {
    let mut pcm = vec![0i16; frame_samples];
    let mut opus_frame = Vec::with_capacity(256);
    let mut payload = Vec::with_capacity(512);
    let mut seq = 0u32;
    let mut next_tick = Instant::now();
    loop {
        // Die with the session.
        if tx.is_closed() {
            return;
        }
        if let Err(e) = source.fill_frame(&mut pcm) {
            warn!("mic source failed: {e:#}");
            return;
        }
        opus_frame.clear();
        match encoder.encode(&pcm, &mut opus_frame) {
            Ok(_) => {
                payload.clear();
                protocol::audio::encode_into(
                    protocol::audio::AudioHeader {
                        seq,
                        capture_ts_ms: u32::try_from(clock_start.elapsed().as_millis())
                            .unwrap_or(u32::MAX),
                    },
                    &opus_frame,
                    &mut payload,
                );
                if tx.blocking_send(payload.clone()).is_err() {
                    return; // session over
                }
                seq = seq.wrapping_add(1);
            }
            Err(e) => {
                warn!("mic encode failed: {e}");
                return;
            }
        }
        next_tick += frame_time;
        std::thread::sleep(next_tick.saturating_duration_since(Instant::now()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_source_fills_exact_frames() {
        let mut src = ToneSource::new(440.0, 960);
        let mut buf = vec![0i16; 960];
        src.fill_frame(&mut buf).unwrap();
        assert!(buf.iter().any(|&s| s != 0), "not silence");
        assert!(src.fill_frame(&mut [0i16; 100]).is_err(), "wrong size is an error");
    }

    /// The test tone must be the requested frequency (rising zero crossings
    /// over one second of audio).
    #[test]
    fn tone_source_frequency_is_accurate() {
        let mut src = ToneSource::new(440.0, 960);
        let mut all = Vec::new();
        for _ in 0..50 {
            // 50 frames × 20 ms = 1 s.
            let mut buf = vec![0i16; 960];
            src.fill_frame(&mut buf).unwrap();
            all.extend_from_slice(&buf);
        }
        let crossings = all.windows(2).filter(|w| w[0] <= 0 && w[1] > 0).count();
        assert!(crossings.abs_diff(440) <= 2, "measured {crossings} Hz");
    }

    #[test]
    fn tone_source_phase_is_continuous() {
        let mut src = ToneSource::new(440.0, 960);
        let mut all = vec![0i16; 4 * 960];
        for frame in all.chunks_mut(960) {
            src.fill_frame(frame).unwrap();
        }
        let max_step = all
            .windows(2)
            .map(|w| i32::from(w[1]) - i32::from(w[0]))
            .map(i32::abs)
            .max()
            .expect("non-empty");
        // 440 Hz at 0.3 amplitude: max |Δ| ≈ 0.3 * 32767 * 2π * 440/48000 ≈ 567.
        assert!(max_step < 800, "frame boundary click: {max_step}");
    }

    /// An off-mode client never produces packets, announced or not.
    #[test]
    fn off_mode_stays_off() {
        let mut mic = MicClient::new(MicMode::Off, Instant::now());
        mic.enable(&AudioSpec {
            codec: AudioCodecType::Opus.wire_id(),
            sample_rate: 48_000,
            channels: 1,
            bitrate_kbps: 32,
            frame_ms: 20,
            fec: true,
        });
        assert!(mic.rx.is_none());
    }

    /// The tone pipeline end to end: enable → encoded datagrams with
    /// sequential seq numbers and decodable payloads.
    #[cfg(feature = "audio")]
    #[tokio::test]
    async fn tone_pipeline_produces_decodable_datagrams() {
        let mut mic = MicClient::new(MicMode::Tone(440.0), Instant::now());
        mic.enable(&AudioSpec {
            codec: AudioCodecType::Opus.wire_id(),
            sample_rate: 48_000,
            channels: 1,
            bitrate_kbps: 32,
            frame_ms: 20,
            fec: true,
        });
        assert!(mic.rx.is_some(), "tone mode needs no device");
        let mut decoder = codec::build_audio_decoder(&codec::AudioPreset::by_name("opus-voice").unwrap()).unwrap();
        let mut pcm = vec![0i16; 960];
        let mut prev_seq = None;
        for _ in 0..5 {
            let payload = tokio::time::timeout(Duration::from_secs(2), mic.recv())
                .await
                .expect("packet within 2 s")
                .expect("pipeline alive");
            let (header, frame) = protocol::audio::decode(&payload).unwrap();
            if let Some(prev) = prev_seq {
                assert_eq!(header.seq, prev + 1, "seq is contiguous");
            }
            prev_seq = Some(header.seq);
            decoder.decode(Some(frame), &mut pcm).unwrap();
        }
    }
}
