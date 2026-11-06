// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

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

use std::time::{Duration, Instant};

use codec::{AudioCodecType, AudioPreset};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::audio::AudioSpec;

/// Mic mode from the CLI (`--mic` / `--mic-tone`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum MicMode {
    /// No mic (default: never hot-mic the user by surprise).
    #[default]
    Off,
    /// Capture the default input device.
    Device,
    /// Capture replaced by a synthetic sine of the given frequency in Hz
    /// (deterministic E2E without audio hardware).
    Tone(f64),
}

use channels::audio::input::MicSource;
#[cfg(feature = "audio")]
use channels::audio::input::CpalMicSource;

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


/// Client-side mic entry point: created from the CLI mode, started when the
/// server announces audio-in support, drained by the session loop.
pub struct MicClient {
    mode: MicMode,
    clock_start: Instant,
    /// Receives encoded datagram payloads once the pipeline is running.
    rx: Option<mpsc::Receiver<Vec<u8>>>,
    /// Last announced audio-in format, kept to restart capture after a
    /// runtime toggle (`ClientCommand::SetMic`).
    spec: Option<AudioSpec>,
    /// Consumer toggle (§15.3): capture only runs while allowed.
    allowed: bool,
}

impl MicClient {
    /// Creates the client; capture starts on [`MicClient::enable`].
    #[must_use]
    pub fn new(mode: MicMode, clock_start: Instant) -> Self {
        Self {
            mode,
            clock_start,
            rx: None,
            spec: None,
            allowed: true,
        }
    }

    /// Applies the server's audio-in announcement (§9.2): builds the encoder
    /// for the announced format and starts the capture thread. A second
    /// announcement while running is ignored (the PoC format is fixed).
    pub fn enable(&mut self, spec: &AudioSpec) {
        self.spec = Some(*spec);
        if self.rx.is_some() || self.mode == MicMode::Off || !self.allowed {
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

    /// Consumer toggle (§15.3, `ClientCommand::SetMic`): `false` stops the
    /// capture thread, `true` restarts it from the last announced format
    /// (if any and the mode is not off).
    pub fn set_allowed(&mut self, allowed: bool) {
        if self.allowed == allowed {
            return;
        }
        self.allowed = allowed;
        if allowed {
            let spec = self.spec;
            if let Some(spec) = spec {
                self.enable(&spec);
            }
        } else {
            // Dropping the receiver closes the channel; the capture thread
            // exits on its next tick (`tx.is_closed`).
            self.rx = None;
        }
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
