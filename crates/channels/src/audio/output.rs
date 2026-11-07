// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Audio output device side: the cpal output-stream plumbing (behind the
//! `audio` feature). The playback buffer itself lives in
//! `codec::audio_dsp` (shared with tests and the pump); the protocol side
//! (jitter, Opus, A/V clock) lives in `client` and consumes this from its pump thread.
//!
//! Config selection lessons (from uds-client's audio): prefer an F32
//! config — it matches the usual WASAPI shared-mode mix format, and
//! picking a non-matching format is what made headsets fail with
//! UnsupportedConfig — and when no exact-rate config exists, fall back to
//! the device default rate and let the caller resample (see
//! [`PlaybackStream::sample_rate`]).

#[cfg(feature = "audio")]
use std::sync::{Arc, Mutex};
pub use codec::audio_dsp::{PlaybackBuffer, SharedBuffer};

#[cfg(feature = "audio")]
use tracing::{info, warn};

/// Live device stream handle. Without the `audio` feature there is no cpal,
/// so the type degenerates to `()` and playback is always the null sink.
#[cfg(feature = "audio")]
type DeviceStream = cpal::Stream;
/// Live device stream handle (no `audio` feature).
#[cfg(not(feature = "audio"))]
type DeviceStream = ();


/// One open playback stream: the shared buffer (push decoded PCM here),
/// the live device stream, and the rate the device actually runs at.
pub struct PlaybackStream {
    /// Queue of decoded PCM the device callback drains.
    pub buffer: SharedBuffer,
    /// Keeps the device stream alive; playback ends when dropped.
    pub stream: DeviceStream,
    /// Sample rate of the opened stream: the requested rate when the device
    /// supports it, else the device default (the pump MUST resample decoded
    /// frames to this rate — `codec::audio_dsp::LinearResampler`).
    pub sample_rate: u32,
    /// Channel count of the opened stream.
    pub channels: u8,
}

impl std::fmt::Debug for PlaybackStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaybackStream")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .finish_non_exhaustive()
    }
}

/// Starts device playback over the default output device. Returns `None`
/// (null sink) when no device or no compatible config exists — the pump
/// still decodes.
#[cfg(feature = "audio")]
#[must_use]
pub fn start_playback(sample_rate: u32, channels: u8) -> Option<PlaybackStream> {
    use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};

    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        warn!("no audio output device; using null sink");
        return None;
    };
    let Some((config, device_rate)) = select_config(
        device.default_output_config().ok(),
        &device
            .supported_output_configs()
            .map_or_else(|_| Vec::new(), Iterator::collect),
        sample_rate,
        channels,
    ) else {
        warn!(sample_rate, channels, "no compatible output config; using null sink");
        return None;
    };
    if device_rate != sample_rate {
        warn!(
            stream_rate = sample_rate,
            device_rate, "output device cannot do the stream rate; resampling at the pump"
        );
    }
    let shared: SharedBuffer = Arc::new(Mutex::new(PlaybackBuffer::new(device_rate, channels)));
    let stream = match build_output_stream(&device, &config, Arc::clone(&shared)) {
        Ok(s) => s,
        Err(e) => {
            warn!("cannot open audio output stream: {e}; using null sink");
            return None;
        }
    };
    if let Err(e) = stream.play() {
        warn!("cannot start audio output: {e}; using null sink");
        return None;
    }
    Some(PlaybackStream {
        buffer: shared,
        stream,
        sample_rate: device_rate,
        channels,
    })
}

/// Null sink without the `audio` feature.
#[cfg(not(feature = "audio"))]
#[must_use]
pub fn start_playback(_sample_rate: u32, _channels: u8) -> Option<PlaybackStream> {
    None
}

#[cfg(feature = "audio")]
impl client::integrations::audio_out::AudioOutput for PlaybackStream {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn buffer(&self) -> SharedBuffer {
        Arc::clone(&self.buffer)
    }
}

/// The default audio-output factory: opens the default output device via
/// cpal (`None` = null sink). Injected into the client session as
/// [`Integrations::audio_out`](client::integrations::Integrations).
#[cfg(feature = "audio")]
pub fn default_output_factory() -> client::integrations::audio_out::AudioOutputFactory {
    Box::new(|rate, ch| {
        start_playback(rate, ch).map(|s| Box::new(s) as Box<dyn client::integrations::audio_out::AudioOutput>)
    })
}

/// Without the `audio` feature there is no device: the null-sink factory.
#[cfg(not(feature = "audio"))]
pub fn default_output_factory() -> client::integrations::audio_out::AudioOutputFactory {
    Box::new(|_, _| None)
}

/// Picks an output config and its rate. Preference order (§9.1 + the
/// uds-client headset lesson):
/// 1. The device default when it already matches the stream shape.
/// 2. An exact-rate candidate with the stream's channel count, preferring
///    F32 (the WASAPI shared-mode mix format: picking a non-matching
///    sample format made headsets fail with UnsupportedConfig).
/// 3. The device default at ANY rate (same channels): the pump resamples.
#[cfg(feature = "audio")]
fn select_config(
    default: Option<cpal::SupportedStreamConfig>,
    supported: &[cpal::SupportedStreamConfigRange],
    sample_rate: u32,
    channels: u8,
) -> Option<(cpal::SupportedStreamConfig, u32)> {
    if let Some(cfg) = default
        && cfg.sample_rate() == sample_rate
        && cfg.channels() == u16::from(channels)
    {
        return Some((cfg, sample_rate));
    }
    let exact_rate = |format: Option<cpal::SampleFormat>| {
        supported
            .iter()
            .filter(|r| {
                r.channels() == u16::from(channels)
                    && r.min_sample_rate() <= sample_rate
                    && sample_rate <= r.max_sample_rate()
                    && format.is_none_or(|f| r.sample_format() == f)
            })
            .find_map(|r| r.try_with_sample_rate(sample_rate))
    };
    // F32 first, then any exact-rate format.
    if let Some(cfg) = exact_rate(Some(cpal::SampleFormat::F32)).or_else(|| exact_rate(None)) {
        return Some((cfg, sample_rate));
    }
    // Fallback: the device default at its own rate (same channel count);
    // the caller resamples.
    default
        .filter(|cfg| cfg.channels() == u16::from(channels))
        .map(|cfg| {
            let rate = cfg.sample_rate();
            (cfg, rate)
        })
}

/// Lab diagnostic (§18.66): when set, the output callback also writes every
/// sample it pulls (underrun silence included) as raw s16le to this file —
/// the post-buffer counterpart of the pump's pre-buffer dump. A BufWriter
/// keeps syscalls rare; a stalled flush would glitch playback, so this is
/// strictly an opt-in debug tool, never for production sessions.
#[cfg(feature = "audio")]
static OUTPUT_DUMP: std::sync::Mutex<Option<std::io::BufWriter<std::fs::File>>> =
    std::sync::Mutex::new(None);

/// Sets the post-buffer dump file (see [`OUTPUT_DUMP`]). Replaces any
/// previous file; pass a path that does not exist yet.
#[cfg(feature = "audio")]
pub fn set_output_dump(path: &std::path::Path) {
    use std::io::BufWriter;
    match std::fs::File::create(path) {
        Ok(file) => {
            info!(path = %path.display(), "dumping device-pulled audio as raw s16le PCM");
            *OUTPUT_DUMP.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(BufWriter::with_capacity(1 << 20, file));
        }
        Err(e) => warn!(path = %path.display(), "cannot open the output dump file: {e}"),
    }
}

/// No-op without the `audio` feature.
#[cfg(not(feature = "audio"))]
pub fn set_output_dump(_path: &std::path::Path) {}

/// Builds an output stream typed to the config's sample format, converting
/// our i16 buffer on the fly. The callback is allocation-free and degrades
/// to silence on underrun or a contended/poisoned lock (§9.1).
#[cfg(feature = "audio")]
fn build_output_stream_typed<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    buffer: SharedBuffer,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample + cpal::FromSample<i16> + Send + 'static,
{
    use cpal::traits::DeviceTrait as _;

    // Device errors can fire once per callback (hundreds per second on
    // a misbehaving driver); log the first and then every 500th.
    let mut errors = 0u64;
    device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            use std::io::Write as _;
            let mut dump = OUTPUT_DUMP.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Ok(mut buf) = buffer.lock() {
                for sample in data.iter_mut() {
                    let s = buf.pop_or_silence();
                    if let Some(d) = dump.as_mut() {
                        let _ = d.write_all(&s.to_le_bytes());
                    }
                    *sample = T::from_sample(s);
                }
            } else {
                // Poisoned lock: silence, never panic in the callback.
                data.fill(T::from_sample(0i16));
            }
        },
        move |err| {
            if errors == 0 || errors.is_multiple_of(500) {
                warn!("audio stream error: {err}");
            }
            errors += 1;
        },
        None,
    )
}

#[cfg(feature = "audio")]
fn build_output_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    buffer: SharedBuffer,
) -> Result<cpal::Stream, cpal::Error> {
    use cpal::SampleFormat;
    let stream_config = config.config();
    match config.sample_format() {
        SampleFormat::F32 => build_output_stream_typed::<f32>(device, stream_config, buffer),
        SampleFormat::I16 => build_output_stream_typed::<i16>(device, stream_config, buffer),
        SampleFormat::I32 => build_output_stream_typed::<i32>(device, stream_config, buffer),
        SampleFormat::U8 => build_output_stream_typed::<u8>(device, stream_config, buffer),
        SampleFormat::U16 => build_output_stream_typed::<u16>(device, stream_config, buffer),
        SampleFormat::F64 => build_output_stream_typed::<f64>(device, stream_config, buffer),
        other => Err(cpal::Error::with_message(
            cpal::ErrorKind::UnsupportedConfig,
            format!("unsupported sample format {other:?}"),
        )),
    }
}

#[cfg(test)]
mod tests {

    #[cfg(feature = "audio")]
    mod config_selection {
        use super::super::select_config;
        use cpal::{SampleFormat, SupportedBufferSize, SupportedStreamConfig};

        fn range(channels: u16, min: u32, max: u32) -> cpal::SupportedStreamConfigRange {
            cpal::SupportedStreamConfigRange::new(
                channels,
                min,
                max,
                SupportedBufferSize::Unknown,
                SampleFormat::F32,
            )
        }

        fn config(channels: u16, rate: u32) -> SupportedStreamConfig {
            range(channels, rate, rate).try_with_sample_rate(rate).unwrap()
        }

        #[test]
        fn matching_default_wins() {
            let (cfg, rate) = select_config(Some(config(2, 48_000)), &[], 48_000, 2).unwrap();
            assert_eq!(cfg.sample_rate(), 48_000);
            assert_eq!(rate, 48_000);
        }

        #[test]
        fn wrong_rate_default_falls_back_to_supported_range() {
            let supported = [range(2, 44_100, 48_000)];
            let (cfg, rate) =
                select_config(Some(config(2, 44_100)), &supported, 48_000, 2).unwrap();
            assert_eq!(cfg.sample_rate(), 48_000);
            assert_eq!(rate, 48_000);
        }

        #[test]
        fn unsupported_rate_falls_back_to_device_default_rate() {
            // No exact-rate range: the device default at its own rate wins
            // and the caller resamples (§9.1).
            let supported = [range(2, 8_000, 44_100)];
            let (cfg, rate) =
                select_config(Some(config(2, 44_100)), &supported, 48_000, 2).unwrap();
            assert_eq!(cfg.sample_rate(), 44_100);
            assert_eq!(rate, 44_100);
        }

        #[test]
        fn channel_mismatch_is_rejected() {
            let supported = [range(6, 44_100, 48_000)];
            assert!(select_config(Some(config(6, 48_000)), &supported, 48_000, 2).is_none());
        }

        #[test]
        fn no_compatible_config_is_none() {
            let supported = [range(2, 8_000, 44_100)];
            assert!(select_config(None, &supported, 48_000, 2).is_none());
        }
    }
}
