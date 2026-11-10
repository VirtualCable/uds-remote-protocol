// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

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
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use codec::audio_dsp::LinearResampler;
use codec::audio_dsp::SharedBuffer;

use crate::integrations::audio_out::{AudioOutput, AudioOutputFactory};

use crate::jitter::{JitterBuffer, JitterOutput};
use codec::{AudioCodecType, AudioPreset};
use tracing::{info, warn};

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

/// Raises the current thread's scheduling priority one notch (Windows:
/// `THREAD_PRIORITY_ABOVE_NORMAL`); a no-op elsewhere. See the spawn site
/// for why (device-buffer underruns during process-wide CPU bursts).
#[cfg(windows)]
fn raise_current_thread_priority() {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    };
    // SAFETY: GetCurrentThread returns a pseudo-handle to the calling
    // thread, always valid for SetThreadPriority.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
}

/// No-op outside Windows (lab diagnostics showed no starvation there; the
/// platform-specific knob can be added when it does).
#[cfg(not(windows))]
fn raise_current_thread_priority() {}

impl AudioClient {
    /// Creates the client; when `enabled` is false, packets are counted and
    /// dropped without decoding. With `dump`, every decoded frame is also
    /// appended to that file as raw s16le interleaved PCM (lab/E2E aid —
    /// the format is the announced spec's rate and channel count).
    #[must_use]
    pub fn new(
        enabled: bool,
        dump: Option<std::path::PathBuf>,
        stats: Arc<crate::stats::SessionStats>,
        output_factory: Option<AudioOutputFactory>,
    ) -> Self {
        let now_ms = Arc::new(AtomicI64::new(-1));
        if !enabled {
            return Self { tx: None, now_ms };
        }
        let (tx, rx) = channel();
        let shared = Arc::clone(&now_ms);
        std::thread::spawn(move || {
            // The pump feeds the device buffer; a CPU burst elsewhere in the
            // process (wgpu surface recreate on fullscreen, §18.66) can
            // starve it past the buffer depth and the device plays silence
            // (the "petardazo"). One notch above normal wins the ties
            // against the GUI/GPU workers without starving anything — this
            // is not HIGHEST/TIME_CRITICAL, the scheduler still preempts us.
            raise_current_thread_priority();
            run(&rx, &shared, dump.as_deref(), &stats, output_factory);
        });
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

    /// Shared audio-clock cell for the decode worker thread (§9.1): it runs
    /// the A/V sync decisions off the network task.
    #[must_use]
    pub(crate) fn shared_clock(&self) -> Arc<AtomicI64> {
        Arc::clone(&self.now_ms)
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
    /// Pump-side resampler: Some when the output device could not do the
    /// stream rate and opened at its own default rate (§9.1 refinement).
    resampler: Option<LinearResampler>,
    /// Scratch for one resampled frame (device rate).
    resampled: Vec<i16>,
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
    _out: Option<Box<dyn AudioOutput>>,
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
    /// Live-session stats accumulator (§15.3 debug aid).
    stats: Arc<crate::stats::SessionStats>,
}

impl ActiveStream {
    fn new(
        spec: &AudioSpec,
        now_ms: Arc<AtomicI64>,
        dump_path: Option<&std::path::Path>,
        stats: Arc<crate::stats::SessionStats>,
        output_factory: Option<&AudioOutputFactory>,
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
        let output = output_factory.and_then(|f| f(spec.sample_rate, spec.channels));
        let playback = output.as_ref().map(|o| o.buffer());
        // Resample at the pump when the device runs at another rate; the
        // buffer and the clock map then work in device-rate samples.
        let device_rate = output
            .as_ref()
            .map_or(spec.sample_rate, |o| o.sample_rate());
        let resampler = (device_rate != spec.sample_rate)
            .then(|| LinearResampler::new(spec.sample_rate, device_rate, spec.channels));
        // Pre-buffer dump goes to `<path>.1`; the post-buffer one (what the
        // device callback actually pulls, underrun silence included) is
        // `<path>.2`, wired by the consumer into the output stream.
        let dump = dump_path.and_then(|path| {
            let mut pre = path.as_os_str().to_os_string();
            pre.push(".1");
            match std::fs::File::create(&pre) {
                Ok(file) => {
                    info!(path = %std::path::Path::new(&pre).display(), "dumping decoded audio as raw s16le PCM");
                    Some(std::io::BufWriter::new(file))
                }
                Err(e) => {
                    warn!(path = %std::path::Path::new(&pre).display(), "cannot open the audio dump file: {e}");
                    None
                }
            }
        });
        let clock_map = playback
            .is_some()
            .then(|| AudioClockMap::new(device_rate, spec.channels));
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
            // 6 frames = 120 ms (double the §9.1 default): the playback
            // buffer mirrors this depth, and process-wide CPU bursts (wgpu
            // surface recreate on fullscreen, §18.66) were measured stalling
            // the pump past 60 ms — the device played silence while the
            // jitter had audio ready. The A/V clock maps the real cursor,
            // so the extra latency is compensated in lip-sync, not added.
            jitter: JitterBuffer::with_depth(6),
            playback,
            resampler,
            resampled: Vec::new(),
            clock_map,
            last_frame_ts: None,
            now_ms,
            _out: output,
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
            stats,
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

    /// Queues the decoded frame (resampling first when the device rate
    /// differs from the stream rate) into the playback buffer, anchoring
    /// the A/V clock at the position the frame will actually play (§9.1).
    fn queue_frame(&mut self, frame_ts: u32) {
        let Some(buf) = &self.playback else {
            return;
        };
        let frame = match &mut self.resampler {
            Some(rs) => {
                self.resampled.clear();
                rs.process(&self.pcm, &mut self.resampled);
                &self.resampled
            }
            None => &self.pcm,
        };
        if let Ok(mut b) = buf.lock() {
            // Anchor before the push: after an underrun overhang these
            // samples play at the play position, not at the queue tail.
            if let Some(clock) = &mut self.clock_map {
                let pos = b.push_pos().max(b.play_pos());
                clock.push_anchor(pos, frame_ts);
            }
            b.push(frame);
        } else {
            warn!("playback buffer poisoned; frame dropped");
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
                self.queue_frame(frame_ts);
                // Null sink: decoded and counted, discarded.
            }
            Err(e) => warn!("audio decode failed: {e}"),
        }
        // Publish the audio cursor for the presentation loop (§9.1) when
        // the clock is live: after a stale period the first few frames keep
        // the unusable marker so one stray packet cannot re-engage the
        // hold-against-audio path.
        let mut diag: Option<(u64, Option<u32>)> = None;
        if self.frames_since_stale >= CLOCK_REARM_FRAMES
            && let (Some(buf), Some(clock)) = (&self.playback, &mut self.clock_map)
            && let Ok(b) = buf.lock()
        {
            let pos = b.play_pos();
            clock.prune(pos);
            let ts = clock.ts_at(pos);
            if let Some(ts) = ts {
                self.now_ms.store(i64::from(ts), Ordering::Relaxed);
            }
            diag = Some((pos, ts));
        }
        // Live stats for the consumer overlay (§15.3): cheap atomic stores,
        // refreshed once per frame tick so rates stay current.
        let jitter_stats = self.jitter.stats();
        let (buffered, drained, underrun) = match &self.playback {
            Some(buf) => match buf.lock() {
                Ok(b) => (b.len(), b.drained_samples(), b.underrun_samples()),
                Err(_) => (0, 0, 0),
            },
            None => (0, 0, 0),
        };
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a frame buffer is far below u32::MAX samples"
        )]
        self.stats.set_audio(
            self.decoded_frames,
            self.plc_frames,
            jitter_stats.resyncs,
            underrun,
            buffered as u32,
        );
        if self.last_log.elapsed() >= Duration::from_secs(5) {
            let (play_pos, clock_ts) = diag.unwrap_or((0, None));
            info!(
                decoded = self.decoded_frames,
                plc = self.plc_frames,
                late_dropped = jitter_stats.late_dropped,
                resyncs = jitter_stats.resyncs,
                buffered_samples = buffered,
                drained_samples = drained,
                underrun_samples = underrun,
                play_pos,
                clock_ts,
                published_now_ms = self.now_ms.load(Ordering::Relaxed),
                "audio"
            );
            self.last_log = Instant::now();
        }
    }
}

/// Audio thread main loop: applies jobs and ticks the active stream once
/// per frame duration (absolute pacing, like the server's audio thread).
/// Publishes the audio playback cursor (capture-clock ms) to `now_ms`.
#[allow(clippy::needless_pass_by_value)] // owned: moved into the pump thread
fn run(
    rx: &Receiver<AudioJob>,
    now_ms: &Arc<AtomicI64>,
    dump: Option<&std::path::Path>,
    stats: &Arc<crate::stats::SessionStats>,
    output_factory: Option<AudioOutputFactory>,
) {
    let mut stream: Option<ActiveStream> = None;
    loop {
        let Some(st) = &mut stream else {
            // Nothing configured: block until the first job arrives.
            match rx.recv() {
                Ok(job) => apply(
                    &mut stream,
                    job,
                    now_ms,
                    dump,
                    stats,
                    output_factory.as_ref(),
                ),
                Err(_) => return, // client shutting down
            }
            continue;
        };
        let timeout = st.next_tick.saturating_duration_since(Instant::now());
        match rx.recv_timeout(timeout) {
            Ok(job) => apply(
                &mut stream,
                job,
                now_ms,
                dump,
                stats,
                output_factory.as_ref(),
            ),
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
    stats: &Arc<crate::stats::SessionStats>,
    output_factory: Option<&AudioOutputFactory>,
) {
    match job {
        AudioJob::Config(spec) => {
            *stream = ActiveStream::new(
                &spec,
                Arc::clone(now_ms),
                dump,
                Arc::clone(stats),
                output_factory,
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "audio")]
    use codec::audio_dsp::PlaybackBuffer;

    #[test]
    fn clock_staleness_tracks_decode_activity() {
        let now = Instant::now();
        let ago = |d: Duration| {
            now.checked_sub(d)
                .expect("test instant is far from the epoch")
        };
        assert!(clock_is_stale(None, now), "never primed is stale");
        assert!(!clock_is_stale(Some(now), now), "just decoded is live");
        assert!(
            !clock_is_stale(
                Some(ago(
                    CLOCK_STALE_AFTER.saturating_sub(Duration::from_millis(1))
                )),
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
        let mut st = ActiveStream::new(
            &spec,
            Arc::clone(&now_ms),
            None,
            Arc::new(crate::stats::SessionStats::default()),
            None,
        )
        .unwrap();
        // Headless CI has no output device; attach a buffer manually to
        // exercise the clock mapping (the pump logic is device-independent).
        let buf: SharedBuffer = Arc::new(std::sync::Mutex::new(PlaybackBuffer::new(48_000, 2)));
        st.playback = Some(Arc::clone(&buf));
        st.clock_map = Some(AudioClockMap::new(48_000, 2));

        let preset = AudioPreset::default();
        let mut encoder = codec::build_audio_encoder(&preset).unwrap();
        let pcm = vec![700i16; preset.frame_samples() * 2];
        // Six packets prime the depth-6 playback buffer (§9.1); only the
        // first three are ticked out below.
        for seq in 0..6u32 {
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
        let mut client = AudioClient::new(
            false,
            None,
            Arc::new(crate::stats::SessionStats::default()),
            None,
        );
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
            let stats = Arc::new(crate::stats::SessionStats::default());
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
                apply(&mut stream, job, &now_ms, None, &stats, None);
            }
            // Channel closed before the target count: report the real
            // counters so a short tick count fails with a useful diff
            // instead of a hang.
            stream.map_or((0, 0), |st| (st.decoded_frames, st.plc_frames))
        });

        tx.send(AudioJob::Config(spec)).unwrap();
        // Encode 6 frames of a synthetic tone and feed them as packets:
        // the playback jitter buffer primes at depth 6 (§9.1, 120 ms), so
        // fewer packets would never leave the priming state.
        let preset = AudioPreset::default();
        let mut encoder = codec::build_audio_encoder(&preset).unwrap();
        let mut source_pcm = vec![1000i16; preset.frame_samples() * 2];
        for seq in 0..6u32 {
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
        // Extra ticks: the jitter buffer's priming is not tick-exact, so
        // five ticks occasionally decode fewer than five frames. Closing
        // the channel before the join guarantees the worker exits instead
        // of deadlocking on a short tick count.
        for _ in 0..20 {
            tx.send(AudioJob::Packet(Vec::new())).unwrap(); // tick marker
        }
        drop(tx);
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
            let stats = Arc::new(crate::stats::SessionStats::default());
            let mut stream: Option<ActiveStream> = None;
            while let Ok(job) = rx.recv() {
                let is_tick = matches!(job, AudioJob::Packet(ref p) if p.is_empty());
                if is_tick {
                    if let Some(st) = &mut stream {
                        st.tick();
                        // Exit only after the concealed gap: five decoded
                        // frames alone are the pre-loss packets.
                        if st.decoded_frames >= 5 && st.plc_frames >= 1 {
                            return (st.decoded_frames, st.plc_frames);
                        }
                    }
                    continue;
                }
                apply(&mut stream, job, &now_ms, None, &stats, None);
            }
            // Channel closed before the target count: report the real
            // counters so a short tick count fails with a useful diff
            // instead of a hang.
            stream.map_or((0, 0), |st| (st.decoded_frames, st.plc_frames))
        });

        tx.send(AudioJob::Config(spec)).unwrap();
        let preset = AudioPreset::default();
        let mut encoder = codec::build_audio_encoder(&preset).unwrap();
        let pcm = vec![500i16; preset.frame_samples() * 2];
        // seq 0..=4, then 6 (5 is lost): six packets prime the depth-6
        // playback buffer (§9.1), then the gap surfaces as one PLC frame.
        for seq in [0u32, 1, 2, 3, 4, 6] {
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
        for _ in 0..20 {
            tx.send(AudioJob::Packet(Vec::new())).unwrap(); // tick marker
        }
        drop(tx);
        let (decoded, plc) = worker.join().unwrap();
        // Five real packets plus the one concealed gap.
        assert_eq!(decoded, 6);
        assert_eq!(plc, 1);
    }
}
