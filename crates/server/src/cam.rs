// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Webcam receive pipeline (§18.57), the video counterpart of the mic
//! pipeline in `mic.rs`:
//!
//! ```text
//! session loop ──raw fragment──▶ cam thread: Reassembler ─▶ strip codec
//!                                 wire-id ─▶ VP9 decode ─▶ CamSink
//! ```
//!
//! The client VP9-encodes its camera and sends fragments on `WEBCAM`
//! (lossy leg) in exactly the display fragmentation format
//! ([`protocol::fragment`]), each frame opening with the codec wire id
//! (u16 BE, §8.5) like display frames do. A fragment gap kills the whole
//! frame; when the reassembler counts a new drop the [`CamSignals`] flag
//! asks the session loop for a `WebcamRequestKeyframe` (rate-limited there,
//! same as the display path).
//!
//! The [`CamSink`] trait is the stability seam (§18.57): today [`NullSink`]
//! (defensive discard) and [`RawSink`] (deterministic, hardware-free E2E
//! verification); the Media Foundation Virtual Camera sink plugs in here
//! later (F4.6).

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use codec::{EncodedPacket, VideoCodecType};
use protocol::fragment::{ReassembledFrame, Reassembler};
use tracing::{debug, info, warn};

/// How long a partial frame may go untouched before eviction (and a drop
/// count). Webcam fps is ≥ 5 in any sane configuration, so 500 ms covers
/// several frames of network slack.
const STALE_FRAME_AGE: Duration = Duration::from_millis(500);

/// File magic of the [`RawSink`] format.
const RAW_MAGIC: &[u8; 8] = b"URPCAM01";

/// Destination of decoded webcam frames: tight BGRA, one call per frame.
/// Implementations must not block for long and must not panic (they run on
/// the cam thread; a failure would kill webcam redirection for the session).
pub trait CamSink: Send {
    /// Consumes one decoded frame captured at `capture_ts_ms` (client clock).
    fn write_frame(&mut self, frame: &codec::DecodedFrame, capture_ts_ms: u32);
}

/// Signals the cam thread shares with the session loop.
pub struct CamSignals {
    /// Set when fragment loss was detected; the session loop clears it by
    /// sending `WebcamRequestKeyframe` (rate-limited).
    pub keyframe_needed: AtomicBool,
    /// Frames decoded and delivered to the sink.
    pub frames_decoded: AtomicU64,
    /// Frames dropped incomplete (fragment loss).
    pub frames_dropped: AtomicU64,
}

/// Defensive sink: discards everything, counting frames. Used when no real
/// sink is configured, so a client that streams anyway cannot break
/// anything.
pub struct NullSink {
    frames: u64,
}

impl NullSink {
    /// Creates a null sink.
    #[must_use]
    pub fn new() -> Self {
        Self { frames: 0 }
    }

    /// Frames discarded so far.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames
    }
}

impl Default for NullSink {
    fn default() -> Self {
        Self::new()
    }
}

impl CamSink for NullSink {
    fn write_frame(&mut self, frame: &codec::DecodedFrame, capture_ts_ms: u32) {
        let _ = (frame, capture_ts_ms);
        self.frames += 1;
        if self.frames % 150 == 1 {
            debug!(frames = self.frames, "null cam sink discarding webcam frames");
        }
    }
}

/// Raw file sink: writes decoded frames for E2E verification (§18.57):
///
/// ```text
/// | "URPCAM01" (8B) | then per frame:
/// | width (u32 LE) | height (u32 LE) | capture_ts_ms (u32 LE) |
/// | data_len (u32 LE) | BGRA data ... |
/// ```
pub struct RawSink {
    file: Option<std::io::BufWriter<std::fs::File>>,
    frames: u64,
    path: String,
}

impl RawSink {
    /// Creates the sink, truncating `path` and writing the magic.
    pub fn create(path: &str) -> anyhow::Result<Self> {
        let mut file = std::io::BufWriter::new(
            std::fs::File::create(path).with_context(|| format!("cannot create {path}"))?,
        );
        file.write_all(RAW_MAGIC)?;
        file.flush()?;
        info!(path, "cam sink: recording to raw BGRA");
        Ok(Self {
            file: Some(file),
            frames: 0,
            path: path.to_string(),
        })
    }

    /// Frames written so far.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames
    }
}

impl CamSink for RawSink {
    fn write_frame(&mut self, frame: &codec::DecodedFrame, capture_ts_ms: u32) {
        let Some(file) = &mut self.file else { return };
        let len = u32::try_from(frame.data.len()).unwrap_or(u32::MAX);
        let header: Vec<u8> = [
            frame.width.to_le_bytes(),
            frame.height.to_le_bytes(),
            capture_ts_ms.to_le_bytes(),
            len.to_le_bytes(),
        ]
        .concat();
        if file.write_all(&header).and_then(|()| file.write_all(&frame.data)).is_err() {
            warn!(path = %self.path, "cam sink: write failed; sink disabled");
            self.file = None;
            return;
        }
        self.frames += 1;
    }
}

/// The cam receive pipeline state, thread-independent for testability: feed
/// raw `WEBCAM` fragments with [`CamPump::push_payload`].
struct CamPump {
    decoder: Box<dyn codec::VideoDecoder>,
    reasm: Reassembler,
    sink: Arc<Mutex<Box<dyn CamSink>>>,
    signals: Arc<CamSignals>,
    /// Last value of `Reassembler::dropped_frames` already accounted.
    dropped_seen: u64,
    last_log: Instant,
}

impl CamPump {
    fn new(sink: Box<dyn CamSink>, signals: Arc<CamSignals>) -> codec::Result<Self> {
        Ok(Self {
            decoder: codec::build_decoder(VideoCodecType::Vp9)?,
            reasm: Reassembler::new(),
            sink: Arc::new(Mutex::new(sink)),
            signals,
            dropped_seen: 0,
            last_log: Instant::now(),
        })
    }

    /// Feeds one raw `WEBCAM` fragment into the reassembler; a complete
    /// frame is decoded and delivered to the sink. Newly detected drops set
    /// the keyframe signal.
    fn push_payload(&mut self, payload: &[u8]) {
        match self.reasm.push(payload) {
            Ok(Some(frame)) => self.decode_and_sink(&frame),
            Ok(None) => {}
            Err(e) => warn!("bad webcam fragment: {e}"),
        }
        self.account_drops();
        if self.last_log.elapsed() >= Duration::from_secs(5) {
            info!(
                decoded = self.signals.frames_decoded.load(Ordering::Relaxed),
                dropped = self.signals.frames_dropped.load(Ordering::Relaxed),
                "webcam-in"
            );
            self.last_log = Instant::now();
        }
    }

    /// Decodes one reassembled frame and hands it to the sink.
    fn decode_and_sink(&mut self, frame: &ReassembledFrame) {
        // Every frame opens with the codec wire id (u16 BE, §8.5): the tag,
        // not the negotiated config, is authoritative for the decoder.
        if frame.data.len() < 2 {
            warn!("webcam frame too short for its codec tag; dropped");
            return;
        }
        let codec_id = u16::from_be_bytes([frame.data[0], frame.data[1]]);
        if codec_id != VideoCodecType::Vp9.wire_id() {
            warn!(codec_id, "webcam frame with unexpected codec tag; dropped");
            return;
        }
        let packet = EncodedPacket {
            data: frame.data[2..].to_vec(),
            keyframe: false, // irrelevant for decoding
        };
        match self.decoder.decode(&packet) {
            Ok(decoded) => {
                self.signals.frames_decoded.fetch_add(1, Ordering::Relaxed);
                self.sink
                    .lock()
                    .expect("cam sink")
                    .write_frame(&decoded, frame.capture_ts_ms);
            }
            Err(e) => warn!("webcam decode failed: {e}"),
        }
    }

    /// Evicts stale partials (loss right before a paused camera) and sets
    /// the keyframe signal when the drop count moved.
    fn evict_stale(&mut self) {
        self.reasm.evict_stale(STALE_FRAME_AGE);
        self.account_drops();
    }

    /// Moves new reassembler drops into the shared counters and the
    /// keyframe-needed flag.
    fn account_drops(&mut self) {
        let dropped = self.reasm.dropped_frames();
        let new = dropped - self.dropped_seen;
        if new > 0 {
            self.dropped_seen = dropped;
            self.signals.frames_dropped.fetch_add(new, Ordering::Relaxed);
            self.signals.keyframe_needed.store(true, Ordering::Relaxed);
        }
    }
}

/// Cam thread body: applies fragments; on receive timeouts evicts stale
/// partials so a loss is noticed even when no further traffic arrives.
fn run(rx: &Receiver<Vec<u8>>, mut pump: CamPump) {
    loop {
        match rx.recv_timeout(STALE_FRAME_AGE) {
            Ok(payload) => pump.push_payload(&payload),
            Err(RecvTimeoutError::Timeout) => pump.evict_stale(),
            Err(RecvTimeoutError::Disconnected) => return, // session over
        }
    }
}

/// Starts the webcam receive pipeline for one session (§18.57): a thread
/// owning reassembler + VP9 decoder + `sink`, fed raw `WEBCAM` fragments
/// through the returned channel (unbounded and non-blocking on the send
/// side, like the mic pipeline).
///
/// Returns `None` (webcam redirection disabled with a warning) when the VP9
/// decoder is not compiled in.
pub fn spawn_cam(sink: Box<dyn CamSink>) -> Option<(Sender<Vec<u8>>, Arc<CamSignals>)> {
    let signals = Arc::new(CamSignals {
        keyframe_needed: AtomicBool::new(false),
        frames_decoded: AtomicU64::new(0),
        frames_dropped: AtomicU64::new(0),
    });
    let pump = match CamPump::new(sink, Arc::clone(&signals)) {
        Ok(p) => p,
        Err(e) => {
            warn!("webcam redirection disabled: {e}");
            return None;
        }
    };
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("cam-receive".to_string())
        .spawn(move || run(&rx, pump))
        .ok()?;
    Some((tx, signals))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codec::VideoEncoder as _;

    /// Sink that counts frames and remembers the last geometry/timestamp.
    struct CountSink {
        frames: u64,
        last: (u32, u32, u32),
    }
    impl CamSink for CountSink {
        fn write_frame(&mut self, frame: &codec::DecodedFrame, capture_ts_ms: u32) {
            self.frames += 1;
            self.last = (frame.width, frame.height, capture_ts_ms);
        }
    }

    /// A pump whose sink handle stays observable from the test.
    struct TestPump {
        pump: CamPump,
        sink: Arc<Mutex<CountSink>>,
    }

    fn test_pump() -> TestPump {
        // Forwarding sink so the test keeps an observable handle on the
        // counts while the pump owns its boxed sink as in production.
        struct Fwd(Arc<Mutex<CountSink>>);
        impl CamSink for Fwd {
            fn write_frame(&mut self, frame: &codec::DecodedFrame, ts: u32) {
                self.0.lock().expect("count sink").write_frame(frame, ts);
            }
        }
        let count = Arc::new(Mutex::new(CountSink {
            frames: 0,
            last: (0, 0, 0),
        }));
        let signals = Arc::new(CamSignals {
            keyframe_needed: AtomicBool::new(false),
            frames_decoded: AtomicU64::new(0),
            frames_dropped: AtomicU64::new(0),
        });
        TestPump {
            pump: CamPump::new(Box::new(Fwd(Arc::clone(&count))), signals).unwrap(),
            sink: count,
        }
    }

    /// Encodes `count` VP9 frames of `w`x`h` (ids starting at `first_id`),
    /// each prefixed with the codec wire id and fragmented, and returns the
    /// flat fragment list. `halve_id` loses half of that frame's fragments.
    fn encode_fragments(
        w: u32,
        h: u32,
        first_id: u32,
        count: u32,
        halve_id: Option<u32>,
    ) -> Vec<Vec<u8>> {
        let mut enc = codec::Vp9Encoder::new(w, h, 300).unwrap();
        let mut out = Vec::new();
        for i in 0..count {
            let id = first_id + i;
            // Pseudo-random pixels: solid colors encode to a single tiny
            // fragment, and a "halved" one-fragment frame leaves no partial
            // to evict — defeating the gap test.
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            for (j, p) in pixels.iter_mut().enumerate() {
                let h = u32::try_from(j % (u32::MAX as usize))
                    .expect("frame index fits u32")
                    .wrapping_mul(2_654_435_761)
                    .wrapping_add(id);
                *p = u8::try_from((h >> 16) & 0xFF).expect("masked to a byte");
            }
            let view = codec::VideoFrame {
                width: w,
                height: h,
                stride: (w * 4) as usize,
                data: &pixels,
            };
            let packet = enc.encode(&view).unwrap();
            let mut element = Vec::with_capacity(2 + packet.data.len());
            element.extend_from_slice(&VideoCodecType::Vp9.wire_id().to_be_bytes());
            element.extend_from_slice(&packet.data);
            let mut frags = protocol::fragment::Fragmenter::split(
                protocol::webcam::STREAM_ID,
                id,
                id * 33,
                &element,
            );
            if Some(id) == halve_id {
                frags.truncate(frags.len() / 2); // lose half the fragments
            }
            out.extend(frags);
        }
        out
    }

    /// VP9 is always available in tests (dev-dependency with `codec-vp9`).
    #[test]
    fn pump_decodes_fragmented_frames() {
        let mut tp = test_pump();
        for frag in encode_fragments(640, 480, 0, 3, None) {
            tp.pump.push_payload(&frag);
        }
        let sink = tp.sink.lock().expect("sink");
        assert_eq!(sink.frames, 3);
        assert_eq!(sink.last, (640, 480, 2 * 33));
        assert_eq!(tp.pump.signals.frames_decoded.load(Ordering::Relaxed), 3);
        assert!(!tp.pump.signals.keyframe_needed.load(Ordering::Relaxed));
    }

    #[test]
    fn gap_sets_keyframe_signal() {
        let mut tp = test_pump();
        // Frame 0 complete; frame 1 loses half its fragments; frames 2..=66
        // push it out of the tracking window (TRACK_WINDOW = 64).
        for frag in encode_fragments(640, 480, 0, 1, None) {
            tp.pump.push_payload(&frag);
        }
        for frag in encode_fragments(640, 480, 1, 66, Some(1)) {
            tp.pump.push_payload(&frag);
        }
        assert!(tp.pump.signals.keyframe_needed.load(Ordering::Relaxed));
        assert!(tp.pump.signals.frames_dropped.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn null_sink_counts_and_discards() {
        let mut sink = NullSink::new();
        let frame = codec::DecodedFrame {
            width: 2,
            height: 2,
            stride: 8,
            data: vec![0; 16],
        };
        sink.write_frame(&frame, 10);
        sink.write_frame(&frame, 20);
        assert_eq!(sink.frames(), 2);
    }

    /// Writes two known frames through a RawSink and validates the file:
    /// magic + exact per-frame records.
    #[test]
    fn raw_sink_roundtrip() {
        let path = std::env::temp_dir().join(format!(
            "uds-rp-cam-test-{}-{}.raw",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let path = path.to_string_lossy().into_owned();
        let f1 = codec::DecodedFrame {
            width: 2,
            height: 1,
            stride: 8,
            data: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        let f2 = codec::DecodedFrame {
            width: 1,
            height: 2,
            stride: 4,
            data: vec![9, 10, 11, 12, 13, 14, 15, 16],
        };
        {
            let mut sink = RawSink::create(&path).unwrap();
            sink.write_frame(&f1, 100);
            sink.write_frame(&f2, 200);
            assert_eq!(sink.frames(), 2);
        }
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(&bytes[0..8], b"URPCAM01");
        let mut off = 8;
        let read_frame = |bytes: &[u8], off: &mut usize| -> (u32, u32, u32, Vec<u8>) {
            let g = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
            let (w, h, ts, len) = (g(*off), g(*off + 4), g(*off + 8), g(*off + 12));
            let data = bytes[*off + 16..*off + 16 + len as usize].to_vec();
            *off += 16 + len as usize;
            (w, h, ts, data)
        };
        let (w, h, ts, data) = read_frame(&bytes, &mut off);
        assert_eq!((w, h, ts), (2, 1, 100));
        assert_eq!(data, f1.data);
        let (w, h, ts, data) = read_frame(&bytes, &mut off);
        assert_eq!((w, h, ts), (1, 2, 200));
        assert_eq!(data, f2.data);
        assert_eq!(off, bytes.len());
    }
}
