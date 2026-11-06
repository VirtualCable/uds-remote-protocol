// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Audio output device side: the circular playback buffer (transport-free,
//! unit-tested) and the cpal output-stream plumbing (behind the `audio`
//! feature). The protocol side (jitter, Opus, A/V clock) lives in `client`
//! and consumes this from its pump thread.
//!
//! Config selection lessons (from uds-client's audio): prefer an F32
//! config — it matches the usual WASAPI shared-mode mix format, and
//! picking a non-matching format is what made headsets fail with
//! UnsupportedConfig — and when no exact-rate config exists, fall back to
//! the device default rate and let the caller resample (see
//! [`PlaybackStream::sample_rate`]).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[cfg(feature = "audio")]
use tracing::warn;

/// Live device stream handle. Without the `audio` feature there is no cpal,
/// so the type degenerates to `()` and playback is always the null sink.
#[cfg(feature = "audio")]
type DeviceStream = cpal::Stream;
/// Live device stream handle (no `audio` feature).
#[cfg(not(feature = "audio"))]
type DeviceStream = ();

/// Circular playback buffer between the network pump and the audio device
/// callback (§9.1): overflow drains the oldest samples down to the target,
/// underrun surfaces as silence (handled by the consumer side).
///
/// The buffer also tracks the absolute play position (samples consumed +
/// drained + underrun silence) so the audio thread can map the playback
/// cursor onto the server's capture clock for A/V lip-sync (§9.1).
///
/// Pure and synchronous: unit-tested without any audio device.
#[derive(Debug)]
pub struct PlaybackBuffer {
    samples: VecDeque<i16>,
    /// Buffered length above which the oldest samples are drained.
    max_samples: usize,
    /// Length the buffer is drained to on overflow (~1 jitter window).
    target_samples: usize,
    /// Samples actually consumed by the device callback (cumulative).
    consumed_samples: u64,
    /// Samples dropped by overflow drains (cumulative).
    drained_samples: u64,
    /// Samples the consumer asked for while the buffer was empty.
    underrun_samples: u64,
}

/// Overflow threshold: 500 ms of buffered audio.
const MAX_BUFFERED_MS: u32 = 500;
/// Drain target after an overflow: 60 ms (~1 jitter window of 3 frames).
const DRAIN_TARGET_MS: u32 = 60;

impl PlaybackBuffer {
    /// Creates a buffer with the default latency policy for the given stream
    /// shape (§9.1).
    #[must_use]
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        let per_ms = sample_rate as usize * usize::from(channels) / 1000;
        Self::with_limits(
            per_ms * MAX_BUFFERED_MS as usize,
            per_ms * DRAIN_TARGET_MS as usize,
        )
    }

    /// Creates a buffer with explicit limits (tests).
    #[must_use]
    pub fn with_limits(max_samples: usize, target_samples: usize) -> Self {
        Self {
            samples: VecDeque::new(),
            max_samples,
            target_samples,
            consumed_samples: 0,
            drained_samples: 0,
            underrun_samples: 0,
        }
    }

    /// Buffered length in samples (interleaved).
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// True when no samples are buffered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Absolute play position: samples already played, skipped by drains or
    /// filled with underrun silence (§9.1). Advances in real time even
    /// during underruns, so the A/V cursor never stalls.
    #[must_use]
    pub fn play_pos(&self) -> u64 {
        self.consumed_samples + self.drained_samples + self.underrun_samples
    }

    /// Absolute position at which the next pushed sample is queued. When an
    /// underrun overhang exists (the device played silence past the end),
    /// newly pushed samples start playing at the play position instead —
    /// timestamp anchors are registered at `max(push_pos, play_pos)`.
    #[must_use]
    pub fn push_pos(&self) -> u64 {
        self.consumed_samples + self.drained_samples + self.samples.len() as u64
    }

    /// Samples dropped by overflow drains so far.
    #[must_use]
    pub fn drained_samples(&self) -> u64 {
        self.drained_samples
    }

    /// Samples requested while empty so far (underrun accounting).
    #[must_use]
    pub fn underrun_samples(&self) -> u64 {
        self.underrun_samples
    }

    /// Pushes one decoded frame; drains the oldest samples when the buffer
    /// exceeds the overflow threshold (§9.1).
    pub fn push(&mut self, frame: &[i16]) {
        self.samples.extend(frame);
        if self.samples.len() > self.max_samples {
            let drop = self.samples.len() - self.target_samples;
            self.samples.drain(..drop);
            self.drained_samples += drop as u64;
        }
    }

    /// Pops one sample for the device callback, or silence on underrun.
    /// Allocation-free and wait-free on the buffer itself.
    pub fn pop_or_silence(&mut self) -> i16 {
        if let Some(s) = self.samples.pop_front() {
            self.consumed_samples += 1;
            return s;
        }
        self.underrun_samples += 1;
        0
    }
}

/// Shared handle between the pump thread and the cpal callback.
pub type SharedBuffer = Arc<Mutex<PlaybackBuffer>>;

/// One open playback stream: the shared buffer (push decoded PCM here),
/// the live device stream, and the rate the device actually runs at.
pub struct PlaybackStream {
    /// Queue of decoded PCM the device callback drains.
    pub buffer: SharedBuffer,
    /// Keeps the device stream alive; playback ends when dropped.
    pub stream: DeviceStream,
    /// Sample rate of the opened stream: the requested rate when the device
    /// supports it, else the device default (the pump MUST resample decoded
    /// frames to this rate — `channels::audio::dsp::LinearResampler`).
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
            if let Ok(mut buf) = buffer.lock() {
                for sample in data.iter_mut() {
                    *sample = T::from_sample(buf.pop_or_silence());
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
    use super::*;

    #[test]
    fn push_then_pop_in_order() {
        let mut buf = PlaybackBuffer::with_limits(100, 50);
        buf.push(&[1, 2, 3]);
        assert_eq!(buf.pop_or_silence(), 1);
        assert_eq!(buf.pop_or_silence(), 2);
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn underrun_is_silence_and_counted() {
        let mut buf = PlaybackBuffer::with_limits(100, 50);
        buf.push(&[7]);
        assert_eq!(buf.pop_or_silence(), 7);
        assert_eq!(buf.pop_or_silence(), 0);
        assert_eq!(buf.pop_or_silence(), 0);
        assert_eq!(buf.underrun_samples(), 2);
    }

    #[test]
    fn overflow_drains_oldest_down_to_target() {
        let mut buf = PlaybackBuffer::with_limits(10, 4);
        for _ in 0..11 {
            buf.push(&[1]);
        }
        // After the 11th push: 11 > 10 → drain to 4 samples.
        assert_eq!(buf.len(), 4);
        assert_eq!(buf.drained_samples(), 7);
    }

    #[test]
    fn no_drain_below_threshold() {
        let mut buf = PlaybackBuffer::with_limits(10, 4);
        for _ in 0..10 {
            buf.push(&[5]);
        }
        assert_eq!(buf.len(), 10);
        assert_eq!(buf.drained_samples(), 0);
    }

    #[test]
    fn default_policy_scales_with_shape() {
        let buf = PlaybackBuffer::new(48_000, 2);
        assert_eq!(buf.max_samples, 48_000 * 2 * 500 / 1000);
        assert_eq!(buf.target_samples, 48_000 * 2 * 60 / 1000);
    }

    #[test]
    fn play_pos_tracks_consumed_drained_and_underrun() {
        let mut buf = PlaybackBuffer::with_limits(10, 4);
        assert_eq!(buf.play_pos(), 0);
        assert_eq!(buf.push_pos(), 0);
        buf.push(&[1, 2, 3]);
        assert_eq!(buf.push_pos(), 3, "queued samples count as pushed");
        assert_eq!(buf.pop_or_silence(), 1);
        assert_eq!(buf.play_pos(), 1, "consumed sample advances the cursor");
        buf.push(&[0; 9]);
        let drained = buf.drained_samples();
        assert!(drained > 0);
        assert_eq!(buf.play_pos(), 1 + drained, "drained samples skip the cursor");
        // Underrun: empty the buffer, then pop silence.
        while !buf.is_empty() {
            buf.pop_or_silence();
        }
        let pos = buf.play_pos();
        assert_eq!(buf.pop_or_silence(), 0);
        assert_eq!(buf.play_pos(), pos + 1, "underrun advances the cursor in real time");
    }

    #[cfg(feature = "audio")]
    mod config_selection {
        use super::*;
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
