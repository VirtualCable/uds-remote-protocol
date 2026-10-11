//! Microphone receive pipeline (design doc §9.2): the client captures its
//! mic, encodes Opus and sends one frame per datagram on channel `AUDIO_IN`;
//! here the datagrams flow through the same jitter buffer used on the client
//! (it lives in `protocol::jitter` precisely because both directions share
//! it), the Opus decoder (PLC on loss) and a [`MicSink`]:
//!
//! ```text
//! session loop ──raw payload──▶ mic thread: jitter.pop() ─▶ decode (PLC on
//!                               loss) ─▶ MicSink::write_pcm
//! ```
//!
//! The thread is paced at the announced frame duration (absolute pacing,
//! like every other audio stage), so PLC ticks even when no packets arrive.
//! The [`MicSink`] trait is the stability seam (§9.2): today [`NullSink`]
//! (defensive discard) and [`WavSink`] (deterministic E2E verification
//! without audio hardware); a Windows virtual-mic driver sink and a Linux
//! PipeWire/Pulse sink plug in here later — explicitly out of scope for the
//! PoC.
//!
//! No DTX upstream: the client sends every frame (the `opus-voice` preset at
//! 32 kbps is cheap); sender-side silence suppression is a future refinement.

use std::io::{Seek as _, SeekFrom, Write as _};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use protocol::jitter::{JitterBuffer, JitterOutput};
use tracing::{debug, info, warn};

/// The fixed audio-in wire format of the PoC (§9.2): Opus 48 kHz mono,
/// 20 ms frames at 32 kbps with in-band FEC (the `opus-voice` preset).
#[must_use]
pub fn audio_in_preset() -> codec::AudioPreset {
    codec::AudioPreset::by_name("opus-voice").expect("opus-voice is in the catalog")
}

/// The destination of decoded microphone audio: 48 kHz mono i16 PCM, one
/// frame per call. Implementations must not block for long and must not
/// panic (they run on the mic thread; a failure would kill audio-in for the
/// session).
pub trait MicSink: Send {
    /// Consumes one decoded frame (`frame_samples` samples).
    fn write_pcm(&mut self, pcm: &[i16]);
}

/// Defensive sink: discards everything, counting frames. Used when no real
/// sink is configured, so a client that sends audio-in anyway cannot break
/// anything (§9.2 fallback defensivo).
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

impl MicSink for NullSink {
    fn write_pcm(&mut self, pcm: &[i16]) {
        let _ = pcm;
        self.frames += 1;
        // One debug line per ~5 s of audio: the counter is visible without
        // spamming the log of a misconfigured peer.
        if self.frames % 250 == 1 {
            debug!(frames = self.frames, "null mic sink discarding audio-in");
        }
    }
}

/// WAV file sink: writes the decoded 48 kHz mono i16 PCM as a standard
/// 16-bit PCM WAV. The header carries placeholder sizes while recording and
/// is patched with the real sizes on `Drop` — the deterministic,
/// hardware-free E2E verification sink (§9.2).
pub struct WavSink {
    file: Option<std::io::BufWriter<std::fs::File>>,
    /// PCM payload bytes written so far (drives the header patch).
    data_bytes: u64,
    path: String,
}

/// Canonical 44-byte PCM WAV header for `data_bytes` of mono i16 audio.
fn wav_header(data_bytes: u64) -> [u8; 44] {
    let rate = audio_in_preset().sample_rate;
    let data32 = u32::try_from(data_bytes).unwrap_or(u32::MAX);
    let riff_size = 36u32.saturating_add(data32);
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&riff_size.to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM format tag
    h[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    h[32..34].copy_from_slice(&2u16.to_le_bytes()); // block align
    h[34..36].copy_from_slice(&16u16.to_le_bytes()); // bits per sample
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data32.to_le_bytes());
    h
}

impl WavSink {
    /// Creates the sink, truncating `path`; the placeholder header is
    /// written immediately so a crash mid-session still leaves a playable
    /// prefix on formats that tolerate zero sizes.
    pub fn create(path: &str) -> anyhow::Result<Self> {
        let mut file = std::io::BufWriter::new(
            std::fs::File::create(path).with_context(|| format!("cannot create {path}"))?,
        );
        file.write_all(&wav_header(0))?;
        file.flush()?;
        info!(path, "mic sink: recording to WAV");
        Ok(Self {
            file: Some(file),
            data_bytes: 0,
            path: path.to_string(),
        })
    }

    /// PCM payload bytes written so far.
    #[must_use]
    pub fn data_bytes(&self) -> u64 {
        self.data_bytes
    }

    /// Flushes and patches the header with the real sizes.
    fn finalize(&mut self) {
        let Some(mut file) = self.file.take() else { return };
        let result = (|| -> anyhow::Result<()> {
            file.flush()?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&wav_header(self.data_bytes))?;
            file.flush()?;
            Ok(())
        })();
        match result {
            Ok(()) => info!(path = %self.path, bytes = self.data_bytes, "mic sink: WAV closed"),
            Err(e) => warn!(path = %self.path, "mic sink: cannot patch WAV header: {e:#}"),
        }
    }
}

impl MicSink for WavSink {
    fn write_pcm(&mut self, pcm: &[i16]) {
        let Some(file) = &mut self.file else { return };
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for s in pcm {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        if let Err(e) = file.write_all(&bytes) {
            warn!(path = %self.path, "mic sink: write failed ({e:#}); sink disabled");
            self.file = None;
            return;
        }
        self.data_bytes += bytes.len() as u64;
    }
}

impl Drop for WavSink {
    fn drop(&mut self) {
        self.finalize();
    }
}

/// The mic receive pipeline state, thread-independent for testability: feed
/// raw `AUDIO_IN` payloads with [`MicPump::push_payload`], call
/// [`MicPump::tick`] once per frame duration.
struct MicPump {
    decoder: Box<dyn codec::AudioDecoder>,
    jitter: JitterBuffer,
    sink: Box<dyn MicSink>,
    /// Scratch PCM for one decoded frame (mono).
    pcm: Vec<i16>,
    decoded_frames: u64,
    plc_frames: u64,
    last_log: Instant,
}

impl MicPump {
    fn new(sink: Box<dyn MicSink>) -> codec::Result<Self> {
        let preset = audio_in_preset();
        let decoder = codec::build_audio_decoder(&preset)?;
        Ok(Self {
            decoder,
            jitter: JitterBuffer::new(),
            sink,
            pcm: vec![0i16; preset.frame_samples()],
            decoded_frames: 0,
            plc_frames: 0,
            last_log: Instant::now(),
        })
    }

    /// Feeds one raw `AUDIO_IN` payload into the jitter buffer.
    fn push_payload(&mut self, payload: &[u8]) {
        match protocol::audio::decode(payload) {
            // capture_ts_ms is unused upstream: there is no playback clock to
            // sync against (the sink consumes as fast as frames decode).
            Ok((header, frame)) => self.jitter.push(header.seq, frame.to_vec()),
            Err(e) => warn!("bad audio-in datagram: {e}"),
        }
    }

    /// Pops one frame from the jitter buffer, decodes it (PLC on loss) and
    /// hands it to the sink. Called once per frame duration.
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
                self.sink.write_pcm(&self.pcm);
            }
            Err(e) => warn!("audio-in decode failed: {e}"),
        }
        if self.last_log.elapsed() >= Duration::from_secs(5) {
            let stats = self.jitter.stats();
            info!(
                decoded = self.decoded_frames,
                plc = self.plc_frames,
                late_dropped = stats.late_dropped,
                resyncs = stats.resyncs,
                "audio-in"
            );
            self.last_log = Instant::now();
        }
    }
}

/// Mic thread main loop: applies payloads and ticks once per frame duration
/// (absolute pacing, like the server's audio-out thread).
fn run(rx: &Receiver<Vec<u8>>, mut pump: MicPump, frame_time: Duration) {
    let mut next_tick = Instant::now();
    loop {
        let timeout = next_tick.saturating_duration_since(Instant::now());
        match rx.recv_timeout(timeout) {
            Ok(payload) => pump.push_payload(&payload),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return, // session over
        }
        while Instant::now() >= next_tick {
            pump.tick();
            next_tick += frame_time;
        }
    }
}

/// Starts the mic receive pipeline for one session (§9.2): a thread owning
/// jitter buffer + Opus decoder + `sink`, fed raw `AUDIO_IN` payloads
/// through the returned channel (unbounded and non-blocking on the send
/// side, like the client's audio channel; a dead thread surfaces as a send
/// error at the forwarding point).
///
/// Returns `None` (audio-in disabled with a warning) when the Opus decoder
/// is not compiled in.
pub fn spawn_mic(sink: Box<dyn MicSink>) -> Option<Sender<Vec<u8>>> {
    let preset = audio_in_preset();
    let pump = match MicPump::new(sink) {
        Ok(p) => p,
        Err(e) => {
            warn!("audio-in disabled: {e}");
            return None;
        }
    };
    let (tx, rx) = channel();
    let frame_time = Duration::from_millis(u64::from(preset.frame_ms));
    std::thread::Builder::new()
        .name("mic-receive".to_string())
        .spawn(move || run(&rx, pump, frame_time))
        .ok()?;
    Some(tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_sink_counts_and_discards() {
        let mut sink = NullSink::new();
        sink.write_pcm(&[0; 960]);
        sink.write_pcm(&[1; 960]);
        assert_eq!(sink.frames(), 2);
    }

    /// Writes known samples through a WavSink and validates the file:
    /// canonical header, patched sizes, exact PCM payload.
    #[test]
    fn wav_sink_roundtrip() {
        let path = std::env::temp_dir().join(format!(
            "uds-rp-mic-test-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let path = path.to_string_lossy().into_owned();
        let samples: Vec<i16> = (0..2000).map(|i| i * 3 - 3000).collect();
        {
            let mut sink = WavSink::create(&path).unwrap();
            sink.write_pcm(&samples[..960]);
            sink.write_pcm(&samples[960..]);
            assert_eq!(sink.data_bytes(), 4000);
        } // Drop patches the header.
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 36 + 4000);
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 1, "mono");
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            48_000,
            "wire rate"
        );
        assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 16, "16-bit");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 4000);
        let pcm: Vec<i16> = bytes[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect();
        assert_eq!(pcm, samples);
    }

    /// End-to-end on the pump: encoded opus-voice frames in → decoded PCM
    /// into the sink, and a lost packet surfaces as exactly one PLC frame.
    #[cfg(feature = "opus")]
    #[test]
    fn pump_decodes_and_conceals() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};
        use protocol::audio::{AudioHeader, encode};

        /// Sink that counts the samples it receives.
        struct CountSink(Arc<AtomicU64>);
        impl MicSink for CountSink {
            fn write_pcm(&mut self, pcm: &[i16]) {
                self.0.fetch_add(pcm.len() as u64, Ordering::Relaxed);
            }
        }

        let written = Arc::new(AtomicU64::new(0));
        let mut pump =
            MicPump::new(Box::new(CountSink(Arc::clone(&written)))).expect("opus decoder");
        let preset = audio_in_preset();
        let mut encoder = codec::build_audio_encoder(&preset).unwrap();
        let pcm = vec![500i16; preset.frame_samples()];
        let mut packet = Vec::new();
        // seq 0, 1, 2 prime the jitter buffer; seq 4 skips 3 (one PLC).
        for seq in [0u32, 1, 2, 4] {
            packet.clear();
            encoder.encode(&pcm, &mut packet).unwrap();
            pump.push_payload(&encode(
                AudioHeader {
                    seq,
                    capture_ts_ms: seq * 20,
                },
                &packet,
            ));
        }
        for _ in 0..5 {
            pump.tick();
        }
        assert_eq!(pump.decoded_frames, 5);
        assert_eq!(pump.plc_frames, 1);
        // One write per decoded frame, 960 mono samples each.
        assert_eq!(written.load(Ordering::Relaxed), 5 * 960);
    }
}
