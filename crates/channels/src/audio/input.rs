// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Microphone capture device side: the [`MicSource`] frame-pull abstraction
//! and the cpal input-device implementation (behind the `audio` feature).
//! The protocol side (Opus encode, wire pacing, test tone) lives in
//! `client`.
//!
//! The input callback converts every sample format to f32 bytes and pushes
//! them through the shared DSP chain (remix to mono, resample to 48 kHz)
//! into a bounded ring; [`MicSource::fill_frame`] drains exactly one frame,
//! emitting silence on underrun. The wire format is fixed 48 kHz mono
//! (§9.2); the DSP chain adapts whatever the device delivers.

#[cfg(feature = "audio")]
use std::sync::{Arc, Mutex};

#[cfg(feature = "audio")]
use tracing::{info, warn};

#[cfg(feature = "audio")]
use codec::audio_dsp::{CaptureRing, DevicePipeline, SampleFormat};

/// A source of mic frames: interleaved signed 16-bit mono PCM, exactly
/// `frame_samples` samples per frame (the audio-in wire shape, §9.2).
pub trait MicSource: Send {
    /// Fills `out` with the next frame; `out` holds exactly one frame.
    fn fill_frame(&mut self, out: &mut [i16]) -> anyhow::Result<()>;
}

/// cpal input-device source: the default input device through the DSP
/// chain into a bounded ring (500 ms of 48 kHz mono, mirroring the
/// server-side policy).
#[cfg(feature = "audio")]
pub struct CpalMicSource {
    ring: Arc<Mutex<CaptureRing>>,
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
    #[must_use]
    pub fn new(frame_samples: usize) -> Option<Self> {
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
        let pipeline = match DevicePipeline::new(SampleFormat::F32, in_channels, in_rate, 1, 48_000)
        {
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
        let ring = Arc::new(Mutex::new(CaptureRing::new(48_000 / 2)));
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
    mut pipeline: DevicePipeline,
    ring: Arc<Mutex<CaptureRing>>,
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
    pipeline: DevicePipeline,
    ring: Arc<Mutex<CaptureRing>>,
) -> Result<cpal::Stream, cpal::Error> {
    let stream_config = config.config();
    let channels = usize::from(config.channels());
    match config.sample_format() {
        cpal::SampleFormat::F32 => {
            build_input_stream_typed::<f32>(device, stream_config, pipeline, ring, channels)
        }
        cpal::SampleFormat::I16 => {
            build_input_stream_typed::<i16>(device, stream_config, pipeline, ring, channels)
        }
        cpal::SampleFormat::I32 => {
            build_input_stream_typed::<i32>(device, stream_config, pipeline, ring, channels)
        }
        cpal::SampleFormat::U8 => {
            build_input_stream_typed::<u8>(device, stream_config, pipeline, ring, channels)
        }
        cpal::SampleFormat::U16 => {
            build_input_stream_typed::<u16>(device, stream_config, pipeline, ring, channels)
        }
        cpal::SampleFormat::F64 => {
            build_input_stream_typed::<f64>(device, stream_config, pipeline, ring, channels)
        }
        other => Err(cpal::Error::with_message(
            cpal::ErrorKind::UnsupportedConfig,
            format!("unsupported input sample format {other:?}"),
        )),
    }
}
