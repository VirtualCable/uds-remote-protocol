//! Audio playback pipeline (design doc §9.1).
//!
//! Topology: the async receive loops hand raw `AUDIO_OUT` payloads to a
//! dedicated audio thread through a channel. The thread owns the jitter
//! buffer, the Opus decoder and the playback side, paced at the announced
//! frame duration:
//!
//! ```text
//! recv loop ──AudioJob──▶ audio thread: jitter.pop() ─▶ decode (PLC on
//!                          loss) ─▶ PlaybackBuffer ─▶ cpal callback
//! ```
//!
//! The cpal callback never allocates and never panics: on underrun it emits
//! silence, and a poisoned/contended buffer lock degrades to silence for
//! that callback instead of unwinding (§9.1). Without the `audio` feature
//! (no codec-opus) the pipeline still receives and counts packets but cannot
//! decode; without an output device it decodes into a null sink so the
//! pipeline stays testable headless.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use client::jitter::{JitterBuffer, JitterOutput};
use codec::{AudioCodecType, AudioPreset};
use tracing::{info, warn};

/// Live device stream handle. Without the `audio` feature there is no cpal,
/// so the type degenerates to `()` and playback is always the null sink.
#[cfg(feature = "audio")]
type DeviceStream = cpal::Stream;
/// Live device stream handle. Without the `audio` feature there is no cpal,
/// so the type degenerates to `()` and playback is always the null sink.
#[cfg(not(feature = "audio"))]
type DeviceStream = ();

/// Audio stream parameters announced by the server (`AudioConfig`, §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSpec {
    /// `AudioCodecType` wire ID.
    pub codec: u16,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channels (1 = mono, 2 = stereo).
    pub channels: u8,
    /// Target bitrate in kbps (informational on the decode side).
    pub bitrate_kbps: u32,
    /// Frame duration in milliseconds.
    pub frame_ms: u8,
    /// In-band FEC enabled on the encoder.
    pub fec: bool,
}

/// Jobs for the audio thread; a single channel keeps config and packets
/// ordered (the config always precedes the packets it describes).
#[derive(Debug)]
enum AudioJob {
    /// Stream announced (or re-announced) by the server.
    Config(AudioSpec),
    /// One raw `AUDIO_OUT` payload (audio header + codec frame).
    Packet(Vec<u8>),
}

/// Client-side audio entry point: feed it stream announcements and raw
/// packets; playback happens on its thread.
pub struct AudioClient {
    tx: Option<Sender<AudioJob>>,
}

impl AudioClient {
    /// Creates the client; when `enabled` is false, packets are counted and
    /// dropped without decoding.
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        if !enabled {
            return Self { tx: None };
        }
        let (tx, rx) = channel();
        std::thread::spawn(move || run(&rx));
        Self { tx: Some(tx) }
    }

    /// Applies a stream announcement from the server.
    pub fn configure(&mut self, spec: AudioSpec) {
        if let Some(tx) = &self.tx
            && tx.send(AudioJob::Config(spec)).is_err()
        {
            warn!("audio thread ended; audio disabled");
            self.tx = None;
        }
    }

    /// Feeds one raw `AUDIO_OUT` payload.
    pub fn feed(&mut self, payload: &[u8]) {
        let Some(tx) = &self.tx else { return };
        if tx.send(AudioJob::Packet(payload.to_vec())).is_err() {
            warn!("audio thread ended; audio disabled");
            self.tx = None;
        }
    }
}

/// Circular playback buffer between the network pump and the audio device
/// callback (§9.1): overflow drains the oldest samples down to the target,
/// underrun surfaces as silence (handled by the consumer side).
///
/// Pure and synchronous: unit-tested without any audio device.
#[derive(Debug)]
pub struct PlaybackBuffer {
    samples: VecDeque<i16>,
    /// Buffered length above which the oldest samples are drained.
    max_samples: usize,
    /// Length the buffer is drained to on overflow (~1 jitter window).
    target_samples: usize,
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
            drained_samples: 0,
            underrun_samples: 0,
        }
    }

    /// Buffered length in samples (interleaved).
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
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
            return s;
        }
        self.underrun_samples += 1;
        0
    }
}

/// Shared handle between the pump thread and the cpal callback.
type SharedBuffer = Arc<Mutex<PlaybackBuffer>>;

/// One active audio stream on the pump thread.
struct ActiveStream {
    decoder: Box<dyn codec::AudioDecoder>,
    jitter: JitterBuffer,
    /// Device playback; `None` is the null sink (headless / no device).
    playback: Option<SharedBuffer>,
    /// Keeps the device stream alive; dropped with the stream state.
    _stream: Option<DeviceStream>,
    /// Scratch PCM buffer for one decoded frame (interleaved).
    pcm: Vec<i16>,
    frame_time: Duration,
    next_tick: Instant,
    decoded_frames: u64,
    plc_frames: u64,
    last_log: Instant,
}

impl ActiveStream {
    fn new(spec: &AudioSpec) -> Option<Self> {
        let Some(codec) = AudioCodecType::from_wire_id(spec.codec) else {
            warn!(codec = spec.codec, "unknown audio codec id; stream dropped");
            return None;
        };
        let preset = AudioPreset {
            codec,
            sample_rate: spec.sample_rate,
            channels: spec.channels,
            bitrate_kbps: spec.bitrate_kbps,
            complexity: 5, // encoder-side knob; irrelevant for decoding
            frame_ms: spec.frame_ms,
            fec: spec.fec,
        };
        let decoder = match codec::build_audio_decoder(&preset) {
            Ok(d) => d,
            Err(e) => {
                warn!("audio decoder unavailable: {e}; stream dropped");
                return None;
            }
        };
        let (playback, stream) = start_playback(spec.sample_rate, spec.channels);
        info!(
            sample_rate = spec.sample_rate,
            channels = spec.channels,
            bitrate_kbps = spec.bitrate_kbps,
            fec = spec.fec,
            playback = playback.is_some(),
            "audio stream announced"
        );
        Some(Self {
            decoder,
            jitter: JitterBuffer::new(),
            playback,
            _stream: stream,
            pcm: vec![0i16; preset.frame_samples() * usize::from(spec.channels)],
            frame_time: Duration::from_millis(u64::from(spec.frame_ms)),
            next_tick: Instant::now(),
            decoded_frames: 0,
            plc_frames: 0,
            last_log: Instant::now(),
        })
    }

    /// Feeds one raw payload into the jitter buffer.
    fn push_packet(&mut self, payload: &[u8]) {
        match protocol::audio::decode(payload) {
            Ok((header, frame)) => {
                // capture_ts_ms is carried for future A/V lip-sync (§9.1);
                // playback is clocked by the device for now.
                self.jitter.push(header.seq, frame.to_vec());
            }
            Err(e) => warn!("bad audio datagram: {e}"),
        }
    }

    /// Pops one frame from the jitter buffer, decodes it (PLC on loss) and
    /// queues it for playback. Called once per frame duration.
    fn tick(&mut self) {
        let Some(output) = self.jitter.pop() else {
            return; // still priming
        };
        let packet = match &output {
            JitterOutput::Packet(bytes) => Some(bytes.as_slice()),
            JitterOutput::Plc => None,
        };
        match self.decoder.decode(packet, &mut self.pcm) {
            Ok(_) => {
                if matches!(output, JitterOutput::Plc) {
                    self.plc_frames += 1;
                }
                self.decoded_frames += 1;
                if let Some(buf) = &self.playback {
                    if let Ok(mut b) = buf.lock() {
                        b.push(&self.pcm);
                    } else {
                        warn!("playback buffer poisoned; frame dropped");
                    }
                }
                // Null sink: decoded and counted, discarded.
            }
            Err(e) => warn!("audio decode failed: {e}"),
        }
        if self.last_log.elapsed() >= Duration::from_secs(5) {
            let stats = self.jitter.stats();
            // Playback buffer health: buffered samples now, cumulative
            // overflow drains and underrun silence (§9.1).
            let (buffered, drained, underrun) = match &self.playback {
                Some(buf) => match buf.lock() {
                    Ok(b) => (b.len(), b.drained_samples(), b.underrun_samples()),
                    Err(_) => (0, 0, 0),
                },
                None => (0, 0, 0),
            };
            info!(
                decoded = self.decoded_frames,
                plc = self.plc_frames,
                late_dropped = stats.late_dropped,
                resyncs = stats.resyncs,
                buffered_samples = buffered,
                drained_samples = drained,
                underrun_samples = underrun,
                "audio"
            );
            self.last_log = Instant::now();
        }
    }
}

/// Audio thread main loop: applies jobs and ticks the active stream once
/// per frame duration (absolute pacing, like the server's audio thread).
fn run(rx: &Receiver<AudioJob>) {
    let mut stream: Option<ActiveStream> = None;
    loop {
        let Some(st) = &mut stream else {
            // Nothing configured: block until the first job arrives.
            match rx.recv() {
                Ok(job) => apply(&mut stream, job),
                Err(_) => return, // client shutting down
            }
            continue;
        };
        let timeout = st.next_tick.saturating_duration_since(Instant::now());
        match rx.recv_timeout(timeout) {
            Ok(job) => apply(&mut stream, job),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        // Tick, catching up if the loop woke up late (never more than a few
        // frames; the playback buffer's overflow drain bounds the backlog).
        let stream = stream.as_mut().expect("checked above");
        while Instant::now() >= stream.next_tick {
            stream.tick();
            stream.next_tick += stream.frame_time;
        }
    }
}

fn apply(stream: &mut Option<ActiveStream>, job: AudioJob) {
    match job {
        AudioJob::Config(spec) => *stream = ActiveStream::new(&spec),
        AudioJob::Packet(payload) => {
            if let Some(st) = stream {
                st.push_packet(&payload);
            }
            // Packets before the first config are dropped silently: the
            // server announces before sending (§9).
        }
    }
}

/// Starts device playback over the default output device. Returns the shared
/// buffer plus the live stream, or `(None, None)` (null sink) when no device
/// or no compatible config exists — the pipeline still decodes.
#[cfg(feature = "audio")]
fn start_playback(sample_rate: u32, channels: u8) -> (Option<SharedBuffer>, Option<cpal::Stream>) {
    use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};

    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        warn!("no audio output device; using null sink");
        return (None, None);
    };
    let Some(config) = select_config(
        device.default_output_config().ok(),
        &device
            .supported_output_configs()
            .map_or_else(|_| Vec::new(), Iterator::collect),
        sample_rate,
        channels,
    ) else {
        warn!(sample_rate, channels, "no compatible output config; using null sink");
        return (None, None);
    };
    let shared: SharedBuffer = Arc::new(Mutex::new(PlaybackBuffer::new(sample_rate, channels)));
    let stream = match build_output_stream(&device, &config, Arc::clone(&shared)) {
        Ok(s) => s,
        Err(e) => {
            warn!("cannot open audio output stream: {e}; using null sink");
            return (None, None);
        }
    };
    if let Err(e) = stream.play() {
        warn!("cannot start audio output: {e}; using null sink");
        return (None, None);
    }
    (Some(shared), Some(stream))
}

/// Null sink without the `audio` feature.
#[cfg(not(feature = "audio"))]
fn start_playback(_sample_rate: u32, _channels: u8) -> (Option<SharedBuffer>, Option<DeviceStream>) {
    (None, None)
}

/// Picks an output config: the device default when it already matches the
/// stream shape (WASAPI shared mode only opens at the mix format), else a
/// supported range that covers the stream's sample rate with the same
/// channel count. No resampling and no channel remix in the PoC (§9.1 lists
/// resampling as a later refinement), so an exact-rate candidate is required.
#[cfg(feature = "audio")]
fn select_config(
    default: Option<cpal::SupportedStreamConfig>,
    supported: &[cpal::SupportedStreamConfigRange],
    sample_rate: u32,
    channels: u8,
) -> Option<cpal::SupportedStreamConfig> {
    if let Some(cfg) = default
        && cfg.sample_rate() == sample_rate
        && cfg.channels() == u16::from(channels)
    {
        return Some(cfg);
    }
    supported
        .iter()
        .find(|r| {
            r.channels() == u16::from(channels)
                && r.min_sample_rate() <= sample_rate
                && sample_rate <= r.max_sample_rate()
        })
        .and_then(|r| r.try_with_sample_rate(sample_rate))
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

    /// An `AudioClient` disabled (or built without codec-opus) consumes
    /// packets and configs without decoding — the headless fallback.
    #[test]
    fn disabled_client_swallows_everything() {
        let mut client = AudioClient::new(false);
        client.configure(AudioSpec {
            codec: AudioCodecType::Opus.wire_id(),
            sample_rate: 48_000,
            channels: 2,
            bitrate_kbps: 64,
            frame_ms: 20,
            fec: false,
        });
        client.feed(&[0u8; 16]);
    }

    /// End-to-end on the pump thread: config → encoded packets → decoded
    /// frames into the null sink (no device needed).
    #[cfg(feature = "audio")]
    #[test]
    fn pump_decodes_encoded_frames() {
        let spec = AudioSpec {
            codec: AudioCodecType::Opus.wire_id(),
            sample_rate: 48_000,
            channels: 2,
            bitrate_kbps: 64,
            frame_ms: 20,
            fec: false,
        };
        let (tx, rx) = channel();
        // The worker mirrors `run` with a bounded job count; it reports
        // counters instead of returning the stream (a cpal stream is not
        // guaranteed Send on every platform).
        let worker = std::thread::spawn(move || {
            let mut stream: Option<ActiveStream> = None;
            while let Ok(job) = rx.recv() {
                // An empty payload is the test's tick marker: real packets
                // always carry the 8-byte header plus a non-empty frame.
                let is_tick = matches!(job, AudioJob::Packet(ref p) if p.is_empty());
                if is_tick {
                    if let Some(st) = &mut stream {
                        st.tick();
                        if st.decoded_frames >= 5 {
                            return (st.decoded_frames, st.plc_frames);
                        }
                    }
                    continue;
                }
                apply(&mut stream, job);
            }
            (0, 0)
        });

        tx.send(AudioJob::Config(spec)).unwrap();
        // Encode 5 frames of a synthetic tone and feed them as packets.
        let preset = AudioPreset::default();
        let mut encoder = codec::build_audio_encoder(&preset).unwrap();
        let mut source_pcm = vec![1000i16; preset.frame_samples() * 2];
        for seq in 0..5u32 {
            let mut packet = Vec::new();
            encoder.encode(&source_pcm, &mut packet).unwrap();
            let payload = protocol::audio::encode(
                protocol::audio::AudioHeader {
                    seq,
                    capture_ts_ms: seq * 20,
                },
                &packet,
            );
            tx.send(AudioJob::Packet(payload)).unwrap();
            source_pcm.fill(0);
        }
        for _ in 0..5 {
            tx.send(AudioJob::Packet(Vec::new())).unwrap(); // tick marker
        }
        let (decoded, plc) = worker.join().unwrap();
        assert_eq!(decoded, 5);
        assert_eq!(plc, 0);
    }

    /// A lost packet surfaces as exactly one PLC frame on the pump.
    #[cfg(feature = "audio")]
    #[test]
    fn pump_conceals_a_lost_packet() {
        let spec = AudioSpec {
            codec: AudioCodecType::Opus.wire_id(),
            sample_rate: 48_000,
            channels: 2,
            bitrate_kbps: 64,
            frame_ms: 20,
            fec: false,
        };
        let (tx, rx) = channel();
        let worker = std::thread::spawn(move || {
            let mut stream: Option<ActiveStream> = None;
            while let Ok(job) = rx.recv() {
                let is_tick = matches!(job, AudioJob::Packet(ref p) if p.is_empty());
                if is_tick {
                    if let Some(st) = &mut stream {
                        st.tick();
                        if st.decoded_frames >= 5 {
                            return (st.decoded_frames, st.plc_frames);
                        }
                    }
                    continue;
                }
                apply(&mut stream, job);
            }
            (0, 0)
        });

        tx.send(AudioJob::Config(spec)).unwrap();
        let preset = AudioPreset::default();
        let mut encoder = codec::build_audio_encoder(&preset).unwrap();
        let pcm = vec![500i16; preset.frame_samples() * 2];
        // seq 0, 1, 2, then 4 (3 is lost): the jitter buffer primes on the
        // first three, then conceals the gap.
        for seq in [0u32, 1, 2, 4] {
            let mut packet = Vec::new();
            encoder.encode(&pcm, &mut packet).unwrap();
            let payload = protocol::audio::encode(
                protocol::audio::AudioHeader {
                    seq,
                    capture_ts_ms: seq * 20,
                },
                &packet,
            );
            tx.send(AudioJob::Packet(payload)).unwrap();
        }
        for _ in 0..5 {
            tx.send(AudioJob::Packet(Vec::new())).unwrap(); // tick marker
        }
        let (decoded, plc) = worker.join().unwrap();
        assert_eq!(decoded, 5);
        assert_eq!(plc, 1);
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
            let cfg = select_config(Some(config(2, 48_000)), &[], 48_000, 2).unwrap();
            assert_eq!(cfg.sample_rate(), 48_000);
        }

        #[test]
        fn wrong_rate_default_falls_back_to_supported_range() {
            let supported = [range(2, 44_100, 48_000)];
            let cfg =
                select_config(Some(config(2, 44_100)), &supported, 48_000, 2).unwrap();
            assert_eq!(cfg.sample_rate(), 48_000);
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
