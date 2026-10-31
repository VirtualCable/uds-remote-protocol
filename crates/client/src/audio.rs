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
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::jitter::{JitterBuffer, JitterOutput};use codec::{AudioCodecType, AudioPreset};
use tracing::{info, warn};

/// Live device stream handle. Without the `audio` feature there is no cpal,
/// so the type degenerates to `()` and playback is always the null sink.
#[cfg(feature = "audio")]
type DeviceStream = cpal::Stream;
/// Live device stream handle. Without the `audio` feature there is no cpal,
/// so the type degenerates to `()` and playback is always the null sink.
#[cfg(not(feature = "audio"))]
type DeviceStream = ();

/// No decodes for this long marks the audio clock unusable: the published
/// cursor freezes at its last anchor, so the A/V sync must fall back to
/// presenting video immediately instead of holding frames against a dead
/// clock (silent desktop ⇒ the server sends no audio packets).
const CLOCK_STALE_AFTER: Duration = Duration::from_millis(500);

/// Decoded frames required after a stale period before the audio clock is
/// trusted again (hysteresis): a single stray packet must not re-engage the
/// hold-against-audio path.
const CLOCK_REARM_FRAMES: u32 = 3;

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
    /// Audio playback cursor on the server's capture clock (ms, §9.1),
    /// published by the audio thread; `-1` = no usable audio clock (no
    /// stream, no output device, or not primed yet).
    now_ms: Arc<AtomicI64>,
}

impl AudioClient {
    /// Creates the client; when `enabled` is false, packets are counted and
    /// dropped without decoding. With `dump`, every decoded frame is also
    /// appended to that file as raw s16le interleaved PCM (lab/E2E aid —
    /// the format is the announced spec's rate and channel count).
    #[must_use]
    pub fn new(enabled: bool, dump: Option<std::path::PathBuf>) -> Self {
        let now_ms = Arc::new(AtomicI64::new(-1));
        if !enabled {
            return Self { tx: None, now_ms };
        }
        let (tx, rx) = channel();
        let shared = Arc::clone(&now_ms);
        std::thread::spawn(move || run(&rx, &shared, dump.as_deref()));
        Self {
            tx: Some(tx),
            now_ms,
        }
    }

    /// Audio playback cursor on the shared capture clock (ms), for A/V
    /// lip-sync decisions (§9.1). `None` means no audio clock is available
    /// (audio disabled, null sink, or not primed): present video at once.
    #[must_use]
    pub fn audio_now_ms(&self) -> Option<u32> {
        let v = self.now_ms.load(Ordering::Relaxed);
        u32::try_from(v).ok()
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
type SharedBuffer = Arc<Mutex<PlaybackBuffer>>;

/// One queued frame's anchor: the absolute play position at which its first
/// sample will be heard, and the capture timestamp of that sample (§9.1).
#[derive(Debug)]
struct Anchor {
    pos: u64,
    ts_ms: u32,
}

/// Maps the playback cursor onto the server's capture clock (§9.1).
///
/// Each decoded frame pushed to the playback buffer registers an
/// [`Anchor`]; `ts_at` interpolates the timestamp of any absolute play
/// position linearly (samples play at a constant device rate). Underrun
/// silence counts as played (the clock advances in real time) and overflow
/// drains count as skipped, both via [`PlaybackBuffer::play_pos`].
#[derive(Debug)]
struct AudioClockMap {
    anchors: VecDeque<Anchor>,
    /// Interleaved samples per millisecond (sample_rate × channels / 1000).
    samples_per_ms: u64,
}

impl AudioClockMap {
    fn new(sample_rate: u32, channels: u8) -> Self {
        Self {
            anchors: VecDeque::new(),
            samples_per_ms: (u64::from(sample_rate) * u64::from(channels) / 1000).max(1),
        }
    }

    /// Registers a frame whose first sample plays at absolute position `pos`.
    fn push_anchor(&mut self, pos: u64, ts_ms: u32) {
        self.anchors.push_back(Anchor { pos, ts_ms });
    }

    /// Capture timestamp (ms) of the sample at absolute play position
    /// `played`; `None` before the first anchor. Positions between anchors
    /// interpolate from the latest anchor at or before `played`; positions
    /// past it (underrun) or before the first (priming) extrapolate.
    fn ts_at(&self, played: u64) -> Option<u32> {
        let anchor = self
            .anchors
            .iter()
            .rev()
            .find(|a| a.pos <= played)
            .or(self.anchors.front())?;
        let offset = played.abs_diff(anchor.pos) / self.samples_per_ms;
        let ts = if played >= anchor.pos {
            i64::from(anchor.ts_ms) + i64::try_from(offset).unwrap_or(i64::MAX)
        } else {
            i64::from(anchor.ts_ms) - i64::try_from(offset).unwrap_or(i64::MAX)
        };
        Some(u32::try_from(ts.clamp(0, i64::from(u32::MAX))).unwrap_or(0))
    }

    /// Drops anchors fully consumed by the play cursor (keeps the latest
    /// one for extrapolation).
    fn prune(&mut self, played: u64) {
        while self.anchors.len() > 1 && self.anchors.get(1).is_some_and(|a| a.pos <= played) {
            self.anchors.pop_front();
        }
    }
}

/// Whether the audio clock is stale: never primed, or no decode within
/// [`CLOCK_STALE_AFTER`] of `now`.
fn clock_is_stale(last_decode_at: Option<Instant>, now: Instant) -> bool {
    last_decode_at.is_none_or(|t| now.duration_since(t) >= CLOCK_STALE_AFTER)
}

/// One active audio stream on the pump thread.
struct ActiveStream {
    decoder: Box<dyn codec::AudioDecoder>,
    jitter: JitterBuffer,
    /// Device playback; `None` is the null sink (headless / no device).
    playback: Option<SharedBuffer>,
    /// Capture-clock mapping for A/V lip-sync (§9.1); `None` with the null
    /// sink — no device clock means no sync reference, and video presents
    /// immediately.
    clock_map: Option<AudioClockMap>,
    /// Last pushed frame's capture ts (for PLC frame estimation).
    last_frame_ts: Option<u32>,
    /// Shared with the `AudioClient` handle: audio cursor in capture-ms,
    /// `-1` until the clock is usable.
    now_ms: Arc<AtomicI64>,
    /// Keeps the device stream alive; dropped with the stream state.
    _stream: Option<DeviceStream>,
    /// Lab aid: every decoded frame is appended here as raw s16le PCM.
    dump: Option<std::io::BufWriter<std::fs::File>>,
    /// Scratch PCM buffer for one decoded frame (interleaved).
    pcm: Vec<i16>,
    /// Frame duration in ms (for PLC timestamp estimation, §9.1).
    frame_ms: u32,
    frame_time: Duration,
    next_tick: Instant,
    decoded_frames: u64,
    plc_frames: u64,
    last_log: Instant,
    /// Last time a frame was decoded (packet or PLC); the A/V clock goes
    /// stale when no decode happens for [`CLOCK_STALE_AFTER`].
    last_decode_at: Option<Instant>,
    /// Consecutive decodes since the clock went stale; the cursor is
    /// published again once it reaches [`CLOCK_REARM_FRAMES`].
    frames_since_stale: u32,
}

impl ActiveStream {
    fn new(
        spec: &AudioSpec,
        now_ms: Arc<AtomicI64>,
        dump_path: Option<&std::path::Path>,
    ) -> Option<Self> {
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
        let dump = dump_path.and_then(|path| match std::fs::File::create(path) {
            Ok(file) => {
                info!(path = %path.display(), "dumping decoded audio as raw s16le PCM");
                Some(std::io::BufWriter::new(file))
            }
            Err(e) => {
                warn!(path = %path.display(), "cannot open the audio dump file: {e}");
                None
            }
        });
        let clock_map = playback
            .is_some()
            .then(|| AudioClockMap::new(spec.sample_rate, spec.channels));
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
            clock_map,
            last_frame_ts: None,
            now_ms,
            _stream: stream,
            dump,
            pcm: vec![0i16; preset.frame_samples() * usize::from(spec.channels)],
            frame_ms: u32::from(spec.frame_ms),
            frame_time: Duration::from_millis(u64::from(spec.frame_ms)),
            next_tick: Instant::now(),
            decoded_frames: 0,
            plc_frames: 0,
            last_log: Instant::now(),
            last_decode_at: None,
            frames_since_stale: 0,
        })
    }

    /// Feeds one raw payload into the jitter buffer. The full payload
    /// (header included) is stored so the capture timestamp survives
    /// reordering and is available when the frame is popped (§9.1).
    fn push_packet(&mut self, payload: &[u8]) {
        match protocol::audio::decode(payload) {
            Ok((header, _)) => self.jitter.push(header.seq, payload.to_vec()),
            Err(e) => warn!("bad audio datagram: {e}"),
        }
    }

    /// Marks the published audio cursor unusable (`-1`) when no frame has
    /// been decoded for [`CLOCK_STALE_AFTER`]. The presentation loop treats
    /// that as "no audio clock" and presents video immediately, so a silent
    /// desktop cannot stall the present queue (§9.1). PLC frames count as
    /// decodes: they conceal real-time gaps and keep the clock alive.
    fn refresh_clock_liveness(&mut self) {
        if clock_is_stale(self.last_decode_at, Instant::now()) {
            self.frames_since_stale = 0;
            self.now_ms.store(-1, Ordering::Relaxed);
        }
    }

    /// Pops one frame from the jitter buffer, decodes it (PLC on loss) and
    /// queues it for playback. Called once per frame duration.
    fn tick(&mut self) {
        // Clock liveness runs every tick, decode or not: with a silent
        // desktop the server sends no audio packets at all, and without this
        // check the published cursor would freeze at its last anchor and
        // stall the whole video presentation queue against a dead clock.
        self.refresh_clock_liveness();
        let Some(output) = self.jitter.pop() else {
            return; // still priming
        };
        let (packet, frame_ts) = match &output {
            JitterOutput::Packet(bytes) => match protocol::audio::decode(bytes) {
                Ok((header, frame)) => {
                    self.last_frame_ts = Some(header.capture_ts_ms);
                    (Some(frame), header.capture_ts_ms)
                }
                Err(e) => {
                    warn!("bad audio datagram: {e}");
                    return;
                }
            },
            // PLC conceals a gap: the concealed samples fill the same time
            // span, so their ts is the previous frame's plus one frame.
            JitterOutput::Plc => {
                let ts = self
                    .last_frame_ts
                    .map_or(0, |t| t.saturating_add(self.frame_ms));
                self.last_frame_ts = Some(ts);
                (None, ts)
            }
        };
        match self.decoder.decode(packet, &mut self.pcm) {
            Ok(_) => {
                if matches!(output, JitterOutput::Plc) {
                    self.plc_frames += 1;
                }
                self.decoded_frames += 1;
                self.last_decode_at = Some(Instant::now());
                self.frames_since_stale = self.frames_since_stale.saturating_add(1);
                if let Some(dump) = &mut self.dump {
                    use std::io::Write as _;
                    for sample in &self.pcm {
                        if dump.write_all(&sample.to_le_bytes()).is_err() {
                            warn!("audio dump write failed; dumping disabled");
                            self.dump = None;
                            break;
                        }
                    }
                }
                if let Some(buf) = &self.playback {
                    if let Ok(mut b) = buf.lock() {
                        // Anchor before the push: after an underrun
                        // overhang these samples play at the play
                        // position, not at the queue tail (§9.1).
                        if let Some(clock) = &mut self.clock_map {
                            let pos = b.push_pos().max(b.play_pos());
                            clock.push_anchor(pos, frame_ts);
                        }
                        b.push(&self.pcm);
                    } else {
                        warn!("playback buffer poisoned; frame dropped");
                    }
                }
                // Null sink: decoded and counted, discarded.
            }
            Err(e) => warn!("audio decode failed: {e}"),
        }
        // Publish the audio cursor for the presentation loop (§9.1) when
        // the clock is live: after a stale period the first few frames keep
        // the unusable marker so one stray packet cannot re-engage the
        // hold-against-audio path.
        if self.frames_since_stale >= CLOCK_REARM_FRAMES
            && let (Some(buf), Some(clock)) = (&self.playback, &mut self.clock_map)
            && let Ok(b) = buf.lock()
        {
            let pos = b.play_pos();
            clock.prune(pos);
            if let Some(ts) = clock.ts_at(pos) {
                self.now_ms.store(i64::from(ts), Ordering::Relaxed);
            }
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
/// Publishes the audio playback cursor (capture-clock ms) to `now_ms`.
fn run(rx: &Receiver<AudioJob>, now_ms: &Arc<AtomicI64>, dump: Option<&std::path::Path>) {
    let mut stream: Option<ActiveStream> = None;
    loop {
        let Some(st) = &mut stream else {
            // Nothing configured: block until the first job arrives.
            match rx.recv() {
                Ok(job) => apply(&mut stream, job, now_ms, dump),
                Err(_) => return, // client shutting down
            }
            continue;
        };
        let timeout = st.next_tick.saturating_duration_since(Instant::now());
        match rx.recv_timeout(timeout) {
            Ok(job) => apply(&mut stream, job, now_ms, dump),
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

fn apply(
    stream: &mut Option<ActiveStream>,
    job: AudioJob,
    now_ms: &Arc<AtomicI64>,
    dump: Option<&std::path::Path>,
) {
    match job {
        AudioJob::Config(spec) => {
            *stream = ActiveStream::new(&spec, Arc::clone(now_ms), dump);
        }
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

    #[test]
    fn play_pos_tracks_consumed_drained_and_underrun() {
        let mut buf = PlaybackBuffer::with_limits(10, 4);
        assert_eq!(buf.play_pos(), 0);
        assert_eq!(buf.push_pos(), 0);
        buf.push(&[1, 2, 3]);
        assert_eq!(buf.push_pos(), 3, "queued samples count as pushed");
        assert_eq!(buf.pop_or_silence(), 1);
        assert_eq!(buf.play_pos(), 1, "consumed sample advances the cursor");
        // Drain: 12 pushed > 10 max → drains to 4, skipping 8-4=... : 12-4=8? No:
        // 3+9=12 pushed total, drain 12-4=8 oldest, consumed 1 of them.
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

    #[test]
    fn clock_staleness_tracks_decode_activity() {
        let now = Instant::now();
        let ago = |d: Duration| now.checked_sub(d).expect("test instant is far from the epoch");
        assert!(clock_is_stale(None, now), "never primed is stale");
        assert!(!clock_is_stale(Some(now), now), "just decoded is live");
        assert!(
            !clock_is_stale(
                Some(ago(CLOCK_STALE_AFTER.saturating_sub(Duration::from_millis(1)))),
                now
            ),
            "a decode inside the window keeps the clock live"
        );
        assert!(
            clock_is_stale(Some(ago(CLOCK_STALE_AFTER)), now),
            "no decode for CLOCK_STALE_AFTER marks the clock stale"
        );
        assert!(
            clock_is_stale(Some(ago(Duration::from_secs(60))), now),
            "a long-silent desktop is stale"
        );
    }

    #[test]
    fn clock_map_interpolates_between_anchors() {
        // 48 kHz mono: 48 samples per ms.
        let mut map = AudioClockMap::new(48_000, 1);
        assert_eq!(map.ts_at(0), None, "no anchors, no clock");
        map.push_anchor(0, 1_000);
        map.push_anchor(960, 1_020); // one 20 ms frame later
        assert_eq!(map.ts_at(0), Some(1_000));
        assert_eq!(map.ts_at(480), Some(1_010));
        assert_eq!(map.ts_at(960), Some(1_020));
        // Past the last anchor (underrun): extrapolate forward.
        assert_eq!(map.ts_at(1_920), Some(1_040));
    }

    #[test]
    fn clock_map_extrapolates_backwards_before_first_anchor() {
        let mut map = AudioClockMap::new(48_000, 1);
        map.push_anchor(480, 1_000);
        assert_eq!(map.ts_at(0), Some(990));
        // Never below zero.
        map.push_anchor(48_000, 500);
        let mut map2 = AudioClockMap::new(48_000, 1);
        map2.push_anchor(48_000, 500);
        assert_eq!(map2.ts_at(0), Some(0));
        drop(map);
    }

    #[test]
    fn clock_map_prune_keeps_the_latest_relevant_anchor() {
        let mut map = AudioClockMap::new(48_000, 1);
        for i in 0..5u64 {
            map.push_anchor(i * 960, 1_000 + u32::try_from(i).unwrap() * 20);
        }
        map.prune(2_000); // play cursor inside anchor 2's span
        assert_eq!(map.anchors.len(), 3, "anchors 2..=4 survive");
        map.prune(100_000);
        assert_eq!(map.anchors.len(), 1, "the latest anchor is always kept");
        assert_eq!(map.ts_at(100_000).unwrap(), 1_080 + (100_000 - 3_840) / 48);
    }

    /// The published audio cursor follows the play position: after three
    /// priming frames (ts 0/20/40), consuming one frame of samples advances
    /// `now_ms` by one frame (§9.1).
    #[cfg(feature = "audio")]
    #[test]
    fn pump_publishes_audio_now_on_the_capture_clock() {
        let spec = AudioSpec {
            codec: AudioCodecType::Opus.wire_id(),
            sample_rate: 48_000,
            channels: 2,
            bitrate_kbps: 64,
            frame_ms: 20,
            fec: false,
        };
        let now_ms = Arc::new(AtomicI64::new(-1));
        let mut st = ActiveStream::new(&spec, Arc::clone(&now_ms), None).unwrap();
        // Headless CI has no output device; attach a buffer manually to
        // exercise the clock mapping (the pump logic is device-independent).
        let buf: SharedBuffer = Arc::new(Mutex::new(PlaybackBuffer::new(48_000, 2)));
        st.playback = Some(Arc::clone(&buf));
        st.clock_map = Some(AudioClockMap::new(48_000, 2));

        let preset = AudioPreset::default();
        let mut encoder = codec::build_audio_encoder(&preset).unwrap();
        let pcm = vec![700i16; preset.frame_samples() * 2];
        for seq in 0..3u32 {
            let mut packet = Vec::new();
            encoder.encode(&pcm, &mut packet).unwrap();
            st.push_packet(&protocol::audio::encode(
                protocol::audio::AudioHeader {
                    seq,
                    capture_ts_ms: seq * 20,
                },
                &packet,
            ));
        }
        for _ in 0..3 {
            st.tick();
        }
        // Primed: 3 frames (1920 samples × 2ch interleaved = 1920*2) queued,
        // play cursor still at 0 → audio_now is the first frame's ts.
        assert_eq!(st.now_ms.load(Ordering::Relaxed), 0);
        // Consume exactly one frame (20 ms × 96 samples/ms stereo).
        {
            let mut b = buf.lock().unwrap();
            for _ in 0..1_920 {
                b.pop_or_silence();
            }
        }
        st.tick();
        assert_eq!(st.now_ms.load(Ordering::Relaxed), 20);
    }

    /// An `AudioClient` disabled (or built without codec-opus) consumes
    /// packets and configs without decoding — the headless fallback.
    #[test]
    fn disabled_client_swallows_everything() {
        let mut client = AudioClient::new(false, None);
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
            let now_ms = Arc::new(AtomicI64::new(-1));
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
                apply(&mut stream, job, &now_ms, None);
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
            let now_ms = Arc::new(AtomicI64::new(-1));
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
                apply(&mut stream, job, &now_ms, None);
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
