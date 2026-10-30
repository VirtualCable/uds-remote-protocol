//! Development server: serves a synthetic moving test pattern or a real
//! desktop capture (DXGI, Windows) over the protocol.
//!
//! Usage: `server --key <hex64> [--ticket <hex96>] [--bind host:port]
//!                 [--source synthetic|dxgi] [--output N | --output-name SUBSTR]
//!                 [--preset NAME] [--bitrate KBPS]
//!                 [--width N] [--height N] [--fps N] [--max-frames N]
//!                 [--audio-preset NAME] [--audio-source auto|synthetic|wasapi]
//!                 [--no-audio] [--mic-sink null|wav:PATH]`
//!
//! `--preset` is a name from `codec::PRESETS` (e.g. `raw`, `vp9-balanced`,
//! `vp8-low-latency`); VP8/VP9 presets require the crate features
//! `vp9`/`vp8`. The chosen codec/geometry is announced on the control
//! channel (`DisplayInfo`) before the first frame.
//!
//! `--output N` captures the Nth output of the first adapter that has one
//! (0 = primary, the default); `--output-name SUBSTR` captures the first
//! output whose adapter description or device name contains SUBSTR
//! (case-insensitive), e.g. `--output-name "UDS Virtual Display"` for the
//! IddCx virtual monitor.
//!
//! Audio (§9) is served by default: one Opus frame (preset named by
//! `--audio-preset`, a name from `codec::AUDIO_PRESETS`, default
//! `opus-music`) per datagram on channel `AUDIO_OUT`. `--no-audio` disables
//! it. `--audio-source` picks the capture: `wasapi` (real desktop loopback,
//! Windows with the `windows-capture` feature), `synthetic` (two-tone
//! generator) or `auto` (the default: WASAPI where available, synthetic
//! elsewhere; a WASAPI init failure under `auto` falls back to synthetic
//! with a warning, while an explicit `wasapi` failure disables audio with a
//! loud error).
//!
//! The session ticket is printed on startup when not given; pass it to the
//! test client with `--ticket`.
//!
//! Audio-in (§9.2): `--mic-sink wav:PATH` records the client microphone to a
//! WAV file (48 kHz mono) and announces audio-in support to the client; the
//! default `null` keeps audio-in off (no announcement, packets discarded).
//!
//! Privacy screen (M7): when the client negotiates `FLAG_PRIVACY_SCREEN`
//! (testclient `--privacy`), the physical console displays are covered with
//! a branded lock overlay for the duration of the session (Windows with the
//! `windows-capture` feature; elsewhere the flag is ignored).

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use codec::{AudioPreset, CodecPreset, VideoEncoder as _};
use crypt::consts;
use protocol::fragment::Fragmenter;
use protocol::{self, ControlMessage, CursorMessage, Probe, ProbeAck, channel};
use server::audio::AudioSource;
use server::pacer::{Pacer, PacerRate};
use server::synthetic::SyntheticSource;
use tokio::sync::{Mutex, mpsc};
use transport::{
    DirectListener, EncryptedDatagram, EncryptedStream, ReliableTransport, UdpTransport,
    udp_offer,
};
use tracing::{debug, error, info, warn};

struct Args {
    bind: SocketAddr,
    key: [u8; consts::KEY_LENGTH],
    ticket: Option<[u8; protocol::TICKET_LENGTH]>,
    source: String,
    output: server::capture::OutputSelector,
    preset: String,
    bitrate: Option<u32>,
    width: u32,
    height: u32,
    fps: u32,
    max_frames: Option<u32>,
    audio: bool,
    audio_preset: String,
    audio_source: String,
    mic_sink: String,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        bind: "127.0.0.1:19790".parse().expect("valid default"),
        key: [0; consts::KEY_LENGTH],
        ticket: None,
        source: "synthetic".to_string(),
        output: server::capture::OutputSelector::Index(0),
        preset: "raw".to_string(),
        bitrate: None,
        width: 640,
        height: 480,
        fps: 30,
        max_frames: None,
        audio: true,
        audio_preset: "opus-music".to_string(),
        audio_source: "auto".to_string(),
        mic_sink: "null".to_string(),
    };
    let mut key_seen = false;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        if arg == "--agent-config-stdin" {
            // Orchestrated mode (M3): the service hands us the whole config —
            // secrets included — over an anonymous pipe on stdin, so argv
            // stays clean. Read it before any other flag is processed.
            let frame = read_config_frame()?;
            let config = service_proto::decode_agent_config(&frame)
                .context("decoding the agent config frame")?;
            args.key = config.key;
            args.ticket = Some(config.ticket);
            args.bind = config.bind.parse().context("config bind address")?;
            anyhow::ensure!(
                !config.output_name.is_empty(),
                "config output_name must not be empty"
            );
            args.output = server::capture::OutputSelector::NameContains(config.output_name);
            args.preset = config.preset;
            args.source = "dxgi".to_string();
            key_seen = true;
            continue;
        }
        if arg == "--no-audio" {
            args.audio = false;
            continue;
        }
        let value = it.next().context(format!("missing value for {arg}"))?;
        match arg.as_str() {
            "--bind" => args.bind = value.parse().context("bad --bind")?,
            "--key" => {
                args.key = protocol::hex::decode(&value)?;
                key_seen = true;
            }
            "--ticket" => args.ticket = Some(protocol::hex::decode(&value)?),
            "--source" => args.source = value,
            "--output" => {
                let n: u32 = value.parse().context("bad --output")?;
                args.output = server::capture::OutputSelector::Index(n);
            }
            "--output-name" => {
                anyhow::ensure!(!value.is_empty(), "--output-name must not be empty");
                args.output = server::capture::OutputSelector::NameContains(value);
            }
            "--preset" => args.preset = value,
            "--bitrate" => args.bitrate = Some(value.parse().context("bad --bitrate")?),
            "--width" => args.width = value.parse().context("bad --width")?,
            "--height" => args.height = value.parse().context("bad --height")?,
            "--fps" => args.fps = value.parse().context("bad --fps")?,
            "--max-frames" => args.max_frames = Some(value.parse().context("bad --max-frames")?),
            "--audio-preset" => args.audio_preset = value,
            "--audio-source" => args.audio_source = value,
            "--mic-sink" => args.mic_sink = value,
            other => bail!("unknown argument: {other}"),
        }
    }
    if !key_seen {
        bail!("--key <hex64> or --agent-config-stdin is required");
    }
    Ok(args)
}

/// Reads the service→agent config frame from stdin (until the service closes
/// its write end). Portable: works on any OS, though only the Windows
/// orchestrated mode uses it today.
fn read_config_frame() -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut buf = Vec::with_capacity(256);
    std::io::stdin()
        .lock()
        .read_to_end(&mut buf)
        .context("reading the agent config frame from stdin")?;
    Ok(buf)
}

/// Session legs after UDP detection (§4.2): the reliable control stream is
/// always alive; display traffic goes over the lossy UDP leg when present,
/// or over the stream as fallback.
struct Legs<T: ReliableTransport> {
    /// UDP send half, shared with the paced display-sender task (§12.6).
    udp: Option<Arc<Mutex<transport::DatagramSink<UdpTransport>>>>,
    stream: Box<EncryptedStream<T>>,
    /// Display fragments queued for the paced sender task; `None` on the
    /// stream fallback, where the kernel's TCP flow control already paces.
    frag_tx: Option<mpsc::Sender<Vec<u8>>>,
}

impl<T: ReliableTransport> Legs<T> {
    /// Queues one display fragment. Returns `Ok(false)` when the paced
    /// sender's queue is full and the fragment was dropped: the caller must
    /// treat the frame as broken and force a keyframe on the next one.
    async fn send_fragment(&mut self, frag: &[u8]) -> transport::Result<bool> {
        let Some(tx) = &self.frag_tx else {
            self.stream.send(channel::DISPLAY, frag).await?;
            return Ok(true);
        };
        match tx.try_send(frag.to_vec()) {
            Ok(()) => Ok(true),
            Err(mpsc::error::TrySendError::Full(_)) => Ok(false),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Err(transport::TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "display sender task ended",
                )))
            }
        }
    }

    async fn send_control(&mut self, msg: &[u8]) -> transport::Result<()> {
        match &self.udp {
            Some(d) => d.lock().await.send(channel::CONTROL, msg).await,
            None => self.stream.send(channel::CONTROL, msg).await,
        }
    }

    /// Audio datagrams (§9): one Opus frame per datagram on `AUDIO_OUT`,
    /// over the UDP leg when present, else the reliable stream fallback.
    async fn send_audio(&mut self, payload: &[u8]) -> transport::Result<()> {
        match &self.udp {
            Some(d) => d.lock().await.send(channel::AUDIO_OUT, payload).await,
            None => self.stream.send(channel::AUDIO_OUT, payload).await,
        }
    }

    /// Cursor updates always take the reliable stream: the client keeps the
    /// last shape as state, so losing one would leave a stale cursor (§10).
    async fn send_cursor(&mut self, msg: &[u8]) -> transport::Result<()> {
        self.stream.send(channel::CURSOR, msg).await
    }
}

/// Receives the next UDP datagram; pends forever when the session has no
/// UDP leg (TCP fallback), so it can sit in a `select!` branch.
async fn recv_udp(
    udp: &mut Option<transport::DatagramSource<UdpTransport>>,
    buf: &mut [u8],
) -> transport::Result<(channel::ChannelId, usize)> {
    match udp {
        Some(d) => d.recv_into(buf).await,
        None => std::future::pending().await,
    }
}

/// Paced display sender (§12.6): drains the fragment queue at the shared
/// pacer rate. Runs in its own task so pacing sleeps never block the session
/// loop (probes, input and clipboard must stay responsive — a blocked loop
/// inflates the measured RTT and the quality controller spirals down).
async fn display_sender(
    mut rx: mpsc::Receiver<Vec<u8>>,
    sink: Arc<Mutex<transport::DatagramSink<UdpTransport>>>,
    mut pacer: Pacer,
) {
    info!("display sender task started");
    let mut sent = 0u64;
    while let Some(frag) = rx.recv().await {
        pacer.pace(frag.len()).await;
        if let Err(e) = sink.lock().await.send(channel::DISPLAY, &frag).await {
            warn!("display sender: send failed, ending task: {e}");
            return;
        }
        sent += 1;
        if sent % 500 == 1 {
            info!(sent, "display sender alive");
        }
    }
}

/// Capture thread → main loop messages.
#[derive(Debug)]
enum CaptureMsg {
    /// First message: the source is up (display geometry) or failed.
    Ready(Result<(u32, u32)>),
    /// One captured frame (owned, so the encoder can borrow from it).
    Frame(OwnedFrame),
    /// Cursor shape/visibility change observed while capturing (§10).
    Cursor(server::capture::CursorUpdate),
}

/// Owned frame data crossing the capture-thread boundary.
#[derive(Debug)]
struct OwnedFrame {
    width: u32,
    height: u32,
    stride: usize,
    /// Capture timestamp (ms on the session clock shared with audio) for
    /// A/V lip-sync (§9.1); stamped by the capture thread.
    capture_ts_ms: u32,
    data: Vec<u8>,
}

/// Runs the frame source on its own thread so the async loop never blocks
/// behind capture waits (probes and metrics must be handled on time, §12).
///
/// The thread paces itself from `fps_shared` (updated live by the quality
/// controller): DXGI already blocks up to `frame_time` waiting for desktop
/// damage, so only the synthetic source sleeps explicitly. Frames are
/// stamped with `clock_start`'s clock — the same one audio uses (§9.1).
fn spawn_capture(
    args: &Args,
    fps_shared: Arc<AtomicU32>,
    clock_start: Instant,
) -> mpsc::Receiver<CaptureMsg> {
    let kind = args.source.clone();
    let (width, height, output) = (args.width, args.height, args.output.clone());
    let (tx, rx) = mpsc::channel(2);
    std::thread::spawn(move || {
        let mut source = match Source::new(&kind, &output, width, height) {
            Ok(s) => s,
            Err(e) => {
                let _ = tx.blocking_send(CaptureMsg::Ready(Err(e)));
                return;
            }
        };
        let dims = source.dimensions();
        if tx.blocking_send(CaptureMsg::Ready(Ok(dims))).is_err() {
            return;
        }
        let sleeps = matches!(source, Source::Synthetic(_));
        loop {
            // The session may end while the desktop is idle (no frames to
            // send, so a dead receiver would go unnoticed and the DXGI
            // duplication would leak into the next session, breaking its
            // DuplicateOutput). Check liveness every iteration.
            if tx.is_closed() {
                return;
            }
            let fps = fps_shared.load(Ordering::Relaxed).max(1);
            let frame_time = Duration::from_secs_f64(1.0 / f64::from(fps));
            match source.next_frame(frame_time) {
                Ok(Some(f)) => {
                    let owned = OwnedFrame {
                        width: f.width,
                        height: f.height,
                        stride: f.stride,
                        capture_ts_ms: millis_since(clock_start),
                        data: f.data.to_vec(),
                    };
                    if tx.blocking_send(CaptureMsg::Frame(owned)).is_err() {
                        return; // session over
                    }
                }
                Ok(None) => {} // no desktop damage this window
                Err(e) => {
                    warn!("capture failed: {e:#}");
                    return;
                }
            }
            if let Some(update) = source.take_cursor_update()
                && tx.blocking_send(CaptureMsg::Cursor(update)).is_err()
            {
                return; // session over
            }
            if sleeps {
                std::thread::sleep(frame_time);
            }
        }
    });
    rx
}

fn micros_since(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn millis_since(start: Instant) -> u32 {
    u32::try_from(start.elapsed().as_millis()).unwrap_or(u32::MAX)
}

/// One encoded audio frame crossing the audio-thread boundary.
#[derive(Debug)]
struct AudioPacket {
    /// Per-session sender counter for the client's jitter buffer (§9).
    seq: u32,
    /// Capture timestamp (ms since session start) for A/V lip-sync (§9.1).
    capture_ts_ms: u32,
    /// The encoded Opus frame.
    data: Vec<u8>,
}

/// Spawns the audio pipeline thread (§9): source → silence gate (PoC DTX) →
/// Opus encoder → one message per frame, paced at the preset's frame
/// duration. The congestion controller's knobs (§12.5) are applied to the
/// encoder at frame boundaries.
///
/// The source is created inside the thread (COM objects are bound to their
/// apartment thread), which reports readiness back before this function
/// returns: on init failure it returns `None` (audio disabled) instead of
/// announcing a stream that would never send.
fn spawn_audio(
    preset: AudioPreset,
    clock_start: Instant,
    source_kind: &str,
    control: server::audio::AudioControl,
) -> Option<mpsc::Receiver<AudioPacket>> {
    let mut encoder = match codec::build_audio_encoder(&preset) {
        Ok(e) => e,
        Err(e) => {
            warn!(preset = preset.bitrate_kbps, "audio disabled: {e}");
            return None;
        }
    };
    let kind = source_kind.to_string();
    let (tx, rx) = mpsc::channel(8);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let frame_time = Duration::from_millis(u64::from(preset.frame_ms));
    std::thread::spawn(move || {
        let Some(mut source) = create_audio_source(&kind, &preset) else {
            return; // readiness channel dropped: the caller sees the failure
        };
        if ready_tx.send(()).is_err() {
            return; // session over while starting
        }
        let mut gate = server::audio::SilenceGate::new();
        let mut pcm = vec![0i16; preset.frame_samples() * usize::from(preset.channels)];
        let mut packet = Vec::with_capacity(512);
        let mut seq = 0u32;
        // Last control values applied to the encoder (§12.5).
        let mut applied = (preset.bitrate_kbps, preset.fec);
        // Absolute pacing: accumulate the frame period instead of sleeping a
        // fixed duration, so encode time does not drift the audio clock.
        let mut next_tick = Instant::now();
        loop {
            // Same liveness rule as the capture thread: die with the session.
            if tx.is_closed() {
                return;
            }
            // Congestion control (§12.5): apply pending bitrate/FEC changes
            // at the frame boundary, before encoding.
            server::audio::apply_control(&mut *encoder, &control, &mut applied);
            if let Err(e) = source.fill_frame(&mut pcm) {
                warn!("audio source failed: {e:#}");
                return;
            }
            // PoC DTX (§9.1): silent frames go out only as keepalives, and
            // skipped frames do not advance `seq` (the client sees fewer
            // frames, not losses).
            if gate.should_send(&pcm) {
                packet.clear();
                match encoder.encode(&pcm, &mut packet) {
                    Ok(_) => {
                        let msg = AudioPacket {
                            seq,
                            capture_ts_ms: millis_since(clock_start),
                            data: packet.clone(),
                        };
                        if tx.blocking_send(msg).is_err() {
                            return; // session over
                        }
                        seq = seq.wrapping_add(1);
                    }
                    Err(e) => {
                        warn!("audio encode failed: {e}");
                        return;
                    }
                }
            }
            next_tick += frame_time;
            std::thread::sleep(next_tick.saturating_duration_since(Instant::now()));
        }
    });
    // The thread always reports readiness (or dies trying, which drops the
    // sender and errors the recv), so this cannot block forever.
    ready_rx.recv().ok()?;
    Some(rx)
}

/// Resolves `--audio-source` (§9): `synthetic` is the two-tone generator,
/// `wasapi` is real desktop loopback (Windows with the `windows-capture`
/// feature) and `auto` prefers WASAPI with a synthetic fallback. An
/// explicit `wasapi` that cannot start disables audio with a loud error — a
/// silently-fake source would hide the misconfiguration.
fn create_audio_source(kind: &str, preset: &AudioPreset) -> Option<Box<dyn AudioSource>> {
    let synthetic = || {
        Box::new(server::audio::SyntheticSource::new(
            preset.sample_rate,
            preset.channels,
            preset.frame_samples(),
        )) as Box<dyn AudioSource>
    };
    #[cfg(all(windows, feature = "windows-capture"))]
    let wasapi = |explicit: bool| {
        match server::audio_wasapi::WasapiLoopbackSource::new(
            preset.sample_rate,
            preset.channels,
            preset.frame_samples(),
        ) {
            Ok(s) => Some(Box::new(s) as Box<dyn AudioSource>),
            Err(e) => {
                if explicit {
                    error!("WASAPI loopback init failed; audio disabled: {e:#}");
                } else {
                    warn!("WASAPI loopback unavailable ({e:#}); falling back to synthetic audio");
                }
                None
            }
        }
    };
    match kind {
        "synthetic" => Some(synthetic()),
        #[cfg(all(windows, feature = "windows-capture"))]
        "wasapi" => wasapi(true),
        #[cfg(not(all(windows, feature = "windows-capture")))]
        "wasapi" => {
            error!("--audio-source wasapi needs Windows with the windows-capture feature; audio disabled");
            None
        }
        "auto" => {
            #[cfg(all(windows, feature = "windows-capture"))]
            if let Some(s) = wasapi(false) {
                return Some(s);
            }
            Some(synthetic())
        }
        other => {
            warn!(%other, "unknown audio source; audio disabled");
            None
        }
    }
}

/// Receives the next encoded audio frame; pends forever when audio is
/// disabled, so it can sit in a `select!` branch.
async fn recv_audio(rx: &mut Option<mpsc::Receiver<AudioPacket>>) -> Option<AudioPacket> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// Fallback audio ceiling for quality targets when audio is off (§12.5);
/// the active preset's bitrate is used when audio is on.
const AUDIO_CEILING_KBPS: u32 = 64;

/// Display fragment queue depth for the paced sender task (§12.6), in
/// datagrams. ~2.4 MiB: absorbs one large keyframe at typical codecs; when
/// it fills (pacing behind schedule), the current frame is dropped and a
/// keyframe is forced instead of growing latency.
const DISPLAY_QUEUE_DGRAMS: usize = 2048;

/// Feeds one link-quality report into the quality controller. FEC reacts to
/// the loss signal on every report (hysteresis, §12.5); when the level
/// changes, the new targets are applied: video encoder bitrate, capture fps,
/// codec-ladder regime force and the audio thread's bitrate knob (§12.5).
#[allow(clippy::too_many_arguments)]
fn handle_metrics(
    quality: &mut Option<protocol::QualityController>,
    forced_video: &mut bool,
    audio_control: Option<&server::audio::AudioControl>,
    fec_on: &mut bool,
    encoder: &mut ServerEncoder,
    fps_shared: &AtomicU32,
    video_ceiling_kbps: u32,
    audio_ceiling_kbps: u32,
    pacer_rate: &PacerRate,
    rtt_micros: u64,
    jitter_micros: u64,
    loss_permille: u16,
) {
    let loss = f64::from(loss_permille) / 1000.0;
    // FEC is loss-driven, not level-driven: evaluate it on every report.
    if let Some(ac) = audio_control {
        let fec = protocol::fec_update(*fec_on, loss);
        if fec != *fec_on {
            *fec_on = fec;
            ac.set_fec(fec);
            info!(fec, loss_permille, "audio FEC toggled");
        }
    }
    let controller = quality.get_or_insert_with(|| {
        info!(baseline_rtt_micros = rtt_micros, "quality baseline set");
        protocol::QualityController::new(protocol::QualityPolicy::standard(rtt_micros))
    });
    let sample = protocol::Sample {
        rtt_micros,
        jitter_micros,
        loss,
    };
    let Some(event) = controller.update(Instant::now(), sample) else {
        return;
    };
    let t = protocol::targets(
        event.level,
        video_ceiling_kbps,
        audio_ceiling_kbps,
        controller.policy(),
    );
    info!(
        level = event.level,
        degraded = event.degraded,
        video_kbps = t.video_kbps,
        audio_kbps = t.audio_kbps,
        fps = t.fps,
        "quality level changed"
    );
    if let Err(e) = encoder.set_bitrate(t.video_kbps) {
        warn!("set_bitrate failed: {e}");
    }
    // UDP burst pacing follows the video target (§12.6): level changes move
    // the token-bucket rate together with the encoder.
    pacer_rate.set_from_video_kbps(t.video_kbps);
    if let Some(ac) = audio_control {
        // The audio thread applies the new target at the next frame (§12.5).
        ac.set_target_kbps(t.audio_kbps);
    }
    fps_shared.store(t.fps, Ordering::Relaxed);
    match (event.force_video_codec, *forced_video) {
        (true, false) => {
            encoder.force_video_regime(true);
            *forced_video = true;
        }
        (false, true) => {
            encoder.force_video_regime(false);
            *forced_video = false;
        }
        _ => {}
    }
}

/// Answers UDP detection probes (§12.1): the client probes up to N times,
/// so keep replying until probes stop arriving.
async fn answer_probes(
    dgram: &mut EncryptedDatagram<UdpTransport>,
    first: &[u8],
    clock_start: Instant,
) -> Result<()> {
    let mut pending = Some(first.to_vec());
    let mut buf = vec![0u8; 2048];
    loop {
        let dgram_bytes = match pending.take() {
            Some(d) => d,
            None => {
                match tokio::time::timeout(
                    Duration::from_millis(100),
                    dgram.recv_into(&mut buf),
                )
                .await
                {
                    Ok(Ok((channel::CONTROL, n))) => buf[..n].to_vec(),
                    // No more probes (or display traffic already flowing).
                    Ok(_) | Err(_) => return Ok(()),
                }
            }
        };
        let mut plain = vec![0u8; 2048];
        if let Some((channel::CONTROL, n)) = dgram.decrypt_received(&dgram_bytes, &mut plain)?
            && let Ok(probe) = Probe::decode(&plain[..n])
        {
            let now = micros_since(clock_start);
            let ack = ProbeAck {
                t1: probe.t1,
                t2: now,
                t3: now,
            };
            dgram.send(channel::CONTROL, &ack.encode()).await?;
            info!("answered UDP probe");
        }
    }
}

/// Encodes one captured frame and sends its fragments; also re-announces
/// the display when the codec ladder switched regime (§8.5). Returns the
/// next frame id.
async fn handle_frame<T: ReliableTransport>(
    frame: &OwnedFrame,
    legs: &mut Legs<T>,
    encoder: &mut ServerEncoder,
    frame_id: u32,
    width: u32,
    height: u32,
) -> Result<u32> {
    let packet = encoder.encode(&codec::VideoFrame {
        width: frame.width,
        height: frame.height,
        stride: frame.stride,
        data: &frame.data,
    })?;
    if encoder.take_codec_change().is_some() {
        // Codec ladder switched regime: re-announce before the fragments so
        // the peer rebuilds its decoder (§8.5).
        announce_display(legs, encoder, width, height).await?;
    }
    if packet.data.is_empty() {
        return Ok(frame_id); // rect codecs: nothing changed on this frame
    }
    // Every fragment carries the frame's capture timestamp (§9.1).
    let mut dropped = 0u32;
    for frag in Fragmenter::split(0, frame_id, frame.capture_ts_ms, &packet.data) {
        if !legs.send_fragment(&frag).await? {
            dropped += 1;
        }
    }
    if dropped > 0 {
        // The paced queue is behind: this frame went out incomplete, so the
        // next reference must be a keyframe (the client's loss recovery
        // would ask for one anyway — forcing it skips that round trip).
        debug!(frame_id, dropped, "paced queue full; frame dropped, keyframe forced");
        encoder.request_keyframe();
    }
    Ok(frame_id.wrapping_add(1))
}

/// Real injection on Windows (`SendInput`), logging elsewhere (§10).
#[cfg(all(windows, feature = "windows-capture"))]
fn new_injector() -> Box<dyn server::input::InputInjector> {
    Box::new(server::input::SendInputInjector::new())
}

/// Development injector: logs input events without a Windows session.
#[cfg(not(all(windows, feature = "windows-capture")))]
fn new_injector() -> Box<dyn server::input::InputInjector> {
    Box::new(server::input::LogInjector::default())
}

/// Starts the audio pipeline (§9) and announces `AudioConfig` on the
/// control channel. Returns the receiver of encoded frames, the audio
/// bitrate ceiling for quality targets (§12.5) and the control knobs the
/// congestion controller writes to. Best-effort: an unknown preset, a
/// missing codec feature or a source init failure disables audio with a
/// warning instead of failing the session.
#[allow(clippy::type_complexity)] // the tuple is the session's audio state
async fn setup_audio<T: ReliableTransport>(
    args: &Args,
    legs: &mut Legs<T>,
    clock_start: Instant,
) -> Result<(
    Option<mpsc::Receiver<AudioPacket>>,
    u32,
    Option<server::audio::AudioControl>,
)> {
    if !args.audio {
        return Ok((None, AUDIO_CEILING_KBPS, None));
    }
    let Some(preset) = AudioPreset::by_name(&args.audio_preset) else {
        warn!(preset = %args.audio_preset, "unknown audio preset; audio disabled");
        return Ok((None, AUDIO_CEILING_KBPS, None));
    };
    let control = server::audio::AudioControl::new(preset.bitrate_kbps, preset.fec);
    let Some(rx) = spawn_audio(preset, clock_start, &args.audio_source, control.clone()) else {
        return Ok((None, AUDIO_CEILING_KBPS, None));
    };
    let cfg = ControlMessage::AudioConfig {
        codec: codec::AudioCodecType::Opus.wire_id(),
        sample_rate: preset.sample_rate,
        channels: preset.channels,
        bitrate_kbps: preset.bitrate_kbps,
        frame_ms: preset.frame_ms,
        fec: preset.fec,
    };
    legs.send_control(&cfg.encode()?).await?;
    info!(preset = %args.audio_preset, source = %args.audio_source, "audio announced");
    Ok((Some(rx), preset.bitrate_kbps, Some(control)))
}

/// Sets up the audio-in (client microphone) receiving path (§9.2).
///
/// When `--mic-sink` selects a sink other than `null`, spawns the mic
/// thread (jitter + Opus decode + sink), announces the capability to the
/// client with [`ControlMessage::AudioInConfig`] and returns the channel
/// sender that incoming `AUDIO_IN` payloads are forwarded to. Returns
/// `None` — no announce, incoming packets dropped — when the sink is
/// `null`, unknown, or the decoder cannot be built.
fn setup_mic(args: &Args) -> Option<std::sync::mpsc::Sender<Vec<u8>>> {
    if args.mic_sink == "null" {
        return None;
    }
    let sink: Box<dyn server::mic::MicSink> = if let Some(path) = args.mic_sink.strip_prefix("wav:")
    {
        match server::mic::WavSink::create(path) {
            Ok(s) => Box::new(s),
            Err(e) => {
                warn!(path, "mic wav sink init failed: {e}; audio-in disabled");
                return None;
            }
        }
    } else {
        warn!(sink = %args.mic_sink, "unknown --mic-sink; audio-in disabled");
        return None;
    };
    let tx = server::mic::spawn_mic(sink)?;
    info!(sink = %args.mic_sink, "audio-in announced");
    Some(tx)
}

/// Announces the audio-in decode parameters to the client (§9.2). The
/// client sends raw Opus frames pre-jitter, so `jitter_ms`/`buffer_frames`
/// are server-side concerns and are not part of the wire message.
async fn announce_mic<T: ReliableTransport>(legs: &mut Legs<T>) -> Result<()> {
    let preset = server::mic::audio_in_preset();
    legs.send_control(
        &ControlMessage::AudioInConfig {
            codec: codec::AudioCodecType::Opus.wire_id(),
            sample_rate: preset.sample_rate,
            channels: preset.channels,
            bitrate_kbps: preset.bitrate_kbps,
            frame_ms: preset.frame_ms,
            fec: preset.fec,
        }
        .encode()?,
    )
    .await?;
    Ok(())
}

/// Forwards one `AUDIO_IN` payload to the mic thread. A send failure means
/// the thread ended; disable forwarding for the rest of the session instead
/// of failing the connection.
fn forward_mic(tx: &mut Option<std::sync::mpsc::Sender<Vec<u8>>>, payload: &[u8]) {
    let Some(t) = tx else { return };
    if t.send(payload.to_vec()).is_err() {
        warn!("mic thread ended; audio-in disabled for this session");
        *tx = None;
    }
}

// The session loop owns capture, display, audio and mic branches; splitting
// it further would only scatter the shared session state.
#[allow(clippy::too_many_lines)]
async fn serve<T: ReliableTransport>(
    raw: T,
    ticket: &[u8; protocol::TICKET_LENGTH],
    key: &[u8; consts::KEY_LENGTH],
    args: &Args,
) -> Result<()> {
    let mut stream = EncryptedStream::server_handshake(raw, key, None).await?;
    info!("handshake done");

    // Attach-on-demand (M6): raising the named event asks the service to
    // attach the IddCx monitor; it detaches after an idle timeout once the
    // session (and this guard) ends. Then wait for the virtual display to
    // actually appear before starting capture. Both are best-effort: with
    // no orchestrating service (dev runs) capture just uses what exists.
    #[cfg(all(windows, feature = "windows-capture"))]
    let _remote_signal = match server::remote_signal::raise() {
        Ok(signal) => Some(signal),
        Err(e) => {
            warn!("cannot raise the remote-session signal: {e:#}");
            None
        }
    };
    #[cfg(all(windows, feature = "windows-capture"))]
    server::display_topology::wait_for_uds_display(Duration::from_secs(15));

    // Privacy screen (M7): the client asks for it with FLAG_PRIVACY_SCREEN;
    // the guard covers the physical displays until the session ends and is
    // best-effort (a failure is logged, never fatal to the session).
    #[cfg(all(windows, feature = "windows-capture"))]
    let _privacy_guard = if stream
        .peer_hello()
        .is_some_and(|h| h.flags & protocol::FLAG_PRIVACY_SCREEN != 0)
    {
        match server::privacy::activate() {
            Ok(guard) => {
                info!("privacy screen active (client request)");
                Some(guard)
            }
            Err(e) => {
                warn!("privacy screen unavailable: {e:#}");
                None
            }
        }
    } else {
        None
    };

    let mut udp_bind = args.bind;
    udp_bind.set_port(0);
    let (pending, token) = udp_offer(&mut stream, &udp_bind.to_string(), ticket, key).await?;

    let clock_start = Instant::now();
    let udp = match tokio::time::timeout(Duration::from_secs(3), pending.wait_first()).await {
        Ok(Ok((udp, first))) => {
            let keys = crypt::derive_datagram_keys(ticket, key)?;
            let mut dgram = EncryptedDatagram::new(udp, &keys.s2c, &keys.c2s, &token);
            answer_probes(&mut dgram, &first, clock_start).await?;
            info!("display leg: UDP");
            Some(dgram)
        }
        other => {
            if let Ok(Err(e)) = other {
                warn!("UDP accept failed: {e}");
            } else {
                info!("UDP detection timed out");
            }
            info!("display leg: TCP fallback");
            None
        }
    };
    let (pacer_rate, _) = PacerRate::new();
    // Split the UDP leg: the source stays in this loop (probes, mic), the
    // sink is shared with the paced display-sender task (§12.6).
    let (udp_sink, mut udp_src, frag_tx) = match udp {
        Some(dgram) => {
            let (sink, src) = dgram.into_split();
            let sink = Arc::new(Mutex::new(sink));
            let (tx, rx) = mpsc::channel::<Vec<u8>>(DISPLAY_QUEUE_DGRAMS);
            tokio::spawn(display_sender(rx, Arc::clone(&sink), Pacer::new(pacer_rate.clone())));
            (Some(sink), Some(src), Some(tx))
        }
        None => (None, None, None),
    };
    let mut legs = Legs { udp: udp_sink, stream: Box::new(stream), frag_tx };

    let fps_shared = Arc::new(AtomicU32::new(args.fps.max(1)));
    let mut frame_rx = spawn_capture(args, Arc::clone(&fps_shared), clock_start);
    let (width, height) = match frame_rx.recv().await {
        Some(CaptureMsg::Ready(r)) => r?,
        other => bail!("capture thread ended before reporting readiness ({other:?})"),
    };
    let mut encoder = build_encoder(args, width, height)?;

    // Announce the display codec/geometry before the first frame (§8.3/§8.4).
    announce_display(&mut legs, &mut encoder, width, height).await?;

    // Audio endpoint policy (§9, driver-audio plan §8): point the session
    // default render endpoint at the UDS virtual speakers so app audio
    // reaches our loopback capture; the previous default is restored when
    // the session ends. Best-effort, and must happen BEFORE the WASAPI
    // capture below opens the default endpoint.
    #[cfg(all(windows, feature = "windows-capture"))]
    let _audio_endpoint_guard = if args.audio {
        match server::audio_endpoint::switch_default_to_uds() {
            Ok(guard) => guard,
            Err(e) => {
                warn!("cannot switch the default audio endpoint: {e:#}");
                None
            }
        }
    } else {
        None
    };

    // Audio (§9): announced right after the display, only when the pipeline
    // actually started (best-effort: a missing codec feature disables it).
    let (mut audio_rx, audio_ceiling_kbps, audio_control) =
        setup_audio(args, &mut legs, clock_start).await?;

    // Audio-in (§9.2): client microphone. Announced only when a sink was
    // configured and the decode thread started; otherwise AUDIO_IN packets
    // are dropped.
    let mut mic_tx = setup_mic(args);
    if mic_tx.is_some() {
        announce_mic(&mut legs).await?;
    }

    // Clipboard (M8): OS bridge + wire session state. Best-effort — when the
    // platform has no backend the channel simply stays silent.
    let mut clipboard = server::clipboard::spawn();
    if clipboard.is_some() {
        info!("clipboard bridge active");
    }
    let mut clip_session = protocol::ClipboardSession::new();

    // Link-quality state (§12): created on the first MetricsReport, with the
    // first reported RTT as the baseline.
    let mut ctl = ControlState {
        quality: None,
        forced_video: false,
        last_keyframe_req: None,
        injector: new_injector(),
        video_ceiling_kbps: args.bitrate.unwrap_or(codec::DEFAULT_BITRATE_KBPS),
        audio_ceiling_kbps,
        audio_control,
        fec_on: false,
        pacer_rate,
    };

    let mut udp_buf = vec![0u8; 2048];
    // Sized for the largest stream record (~64 KiB): cursor shapes (§10)
    // can be a few KiB.
    let mut ctrl_buf = vec![0u8; 65_536];
    // Reusable buffer for audio datagram payloads (header + Opus frame).
    let mut audio_buf = Vec::with_capacity(1024);
    let mut frame_id = 0u32;
    loop {
        tokio::select! {
            msg = frame_rx.recv() => {
                match msg {
                    Some(CaptureMsg::Frame(frame)) => {
                        frame_id =
                            handle_frame(&frame, &mut legs, &mut encoder, frame_id, width, height).await?;
                        if args.max_frames.is_some_and(|max| frame_id >= max) {
                            info!(frame_id, "max frames reached, closing session");
                            return Ok(());
                        }
                    }
                    Some(CaptureMsg::Cursor(update)) => {
                        send_cursor_update(&mut legs, &update).await?;
                    }
                    other => bail!("capture thread ended ({other:?})"),
                }
            }
            msg = recv_audio(&mut audio_rx) => {
                if let Some(packet) = msg {
                    audio_buf.clear();
                    protocol::audio::encode_into(
                        protocol::audio::AudioHeader {
                            seq: packet.seq,
                            capture_ts_ms: packet.capture_ts_ms,
                        },
                        &packet.data,
                        &mut audio_buf,
                    );
                    legs.send_audio(&audio_buf).await?;
                } else {
                    // The audio thread died (encode/source failure): log
                    // once and disable the branch (None would re-fire
                    // immediately, spinning the loop).
                    warn!("audio thread ended; audio disabled for this session");
                    audio_rx = None;
                }
            }
            res = recv_udp(&mut udp_src, &mut udp_buf) => {
                // Continuous probing (§12.1): answer latency probes for the
                // whole session, not only during UDP detection.
                let (ch, n) = res?;
                if ch == channel::CONTROL
                    && let Ok(probe) = Probe::decode(&udp_buf[..n])
                {
                    let now = micros_since(clock_start);
                    let ack = ProbeAck { t1: probe.t1, t2: now, t3: now };
                    legs.send_control(&ack.encode()).await?;
                } else if ch == channel::AUDIO_IN {
                    forward_mic(&mut mic_tx, &udp_buf[..n]);
                }
            }
            res = legs.stream.recv_into(&mut ctrl_buf) => {
                let (ch, n) = res?;
                if ch == channel::AUDIO_IN {
                    forward_mic(&mut mic_tx, &ctrl_buf[..n]);
                } else if ch == channel::CLIPBOARD {
                    handle_clipboard_packet(&mut legs, &mut clip_session, clipboard.as_ref(), &ctrl_buf[..n]).await;
                } else {
                    handle_stream_packet(&mut ctl, &mut encoder, &fps_shared, ch, &ctrl_buf[..n]);
                }
            }
            ev = recv_clipboard(&mut clipboard) => {
                match ev {
                    Some(server::clipboard::ClipboardEvent::TextChanged(text)) => {
                        let offer = clip_session.local_text_changed(text.as_bytes());
                        if let Err(e) = legs.stream.send(channel::CLIPBOARD, &offer.encode()).await {
                            warn!("clipboard offer send failed: {e}");
                        }
                    }
                    None => {
                        warn!("clipboard thread ended; clipboard disabled for this session");
                        clipboard = None;
                    }
                }
            }
        }
    }
}

/// Awaits the next OS clipboard event; pends forever when there is no
/// backend (the branch is then inert) and yields `None` when the backend
/// thread died.
async fn recv_clipboard(
    bridge: &mut Option<server::clipboard::ClipboardBridge>,
) -> Option<server::clipboard::ClipboardEvent> {
    match bridge {
        Some(b) => b.events.recv().await,
        None => std::future::pending().await,
    }
}

/// Handles one packet from the clipboard channel (M8): offers are pulled,
/// requests answered from the session cache, and data applied to the OS
/// clipboard through the backend.
async fn handle_clipboard_packet<T: ReliableTransport>(
    legs: &mut Legs<T>,
    session: &mut protocol::ClipboardSession,
    bridge: Option<&server::clipboard::ClipboardBridge>,
    buf: &[u8],
) {
    use protocol::ClipboardMessage as M;
    match M::decode(buf) {
        Ok(M::Offer { seq, formats }) => {
            if let Some(req) = session.peer_offer(seq, formats)
                && let Err(e) = legs.stream.send(channel::CLIPBOARD, &req.encode()).await
            {
                warn!("clipboard request send failed: {e}");
            }
        }
        Ok(M::Request { seq, format }) => {
            let resp = session.peer_request(seq, format);
            if let Err(e) = legs.stream.send(channel::CLIPBOARD, &resp.encode()).await {
                warn!("clipboard data send failed: {e}");
            }
        }
        Ok(M::Data {
            seq,
            format,
            payload,
        }) => {
            if let Some(bytes) = session.peer_data(seq, format, &payload) {
                if let Ok(text) = String::from_utf8(bytes) {
                    if let Some(b) = bridge {
                        b.send_command(server::clipboard::ClipboardCommand::SetText(text));
                    }
                } else {
                    warn!("clipboard data is not valid UTF-8");
                }
            }
        }
        Ok(M::DataFailed { seq }) => session.peer_data_failed(seq),
        Err(e) => warn!("bad clipboard message: {e}"),
    }
}

/// Encodes and sends one cursor update (visibility and/or shape) on the
/// reliable stream (§10).
async fn send_cursor_update<T: ReliableTransport>(
    legs: &mut Legs<T>,
    update: &server::capture::CursorUpdate,
) -> transport::Result<()> {
    if let Some(visible) = update.visible {
        legs.send_cursor(&CursorMessage::Visibility { visible }.encode())
            .await?;
    }
    if let Some(shape) = &update.shape {
        let msg = CursorMessage::Shape {
            width: shape.width,
            height: shape.height,
            hot_x: shape.hot_x,
            hot_y: shape.hot_y,
            pixels: shape.rgba.clone(),
        }
        .encode();
        // A giant cursor must not kill the session: stream records cap at
        // MAX_PLAINTEXT (~64 KiB); oversized shapes are skipped (§10).
        if msg.len() > consts::MAX_PLAINTEXT {
            warn!(
                width = shape.width,
                height = shape.height,
                "cursor shape exceeds stream record size; skipped"
            );
        } else {
            legs.send_cursor(&msg).await?;
        }
    }
    Ok(())
}

/// Mutable session state for control/input handling (§10/§12).
struct ControlState {
    /// Quality controller; created on the first MetricsReport.
    quality: Option<protocol::QualityController>,
    /// True while congestion forces the codec ladder into its video regime.
    forced_video: bool,
    /// Keyframe requests are rate-limited server-side too (riesgos §2.1).
    last_keyframe_req: Option<Instant>,
    /// Platform input injector (SendInput on Windows, log elsewhere).
    injector: Box<dyn server::input::InputInjector>,
    /// Negotiated video bitrate ceiling for quality targets.
    video_ceiling_kbps: u32,
    /// Negotiated audio bitrate ceiling for quality targets (the active
    /// preset's bitrate, §12.5).
    audio_ceiling_kbps: u32,
    /// Control knobs of the audio encoder thread (§12.5); `None` when audio
    /// is off.
    audio_control: Option<server::audio::AudioControl>,
    /// Current FEC state (for the loss hysteresis, §12.5).
    fec_on: bool,
    /// UDP burst pacer rate knob (§12.6); retuned on every level change.
    pacer_rate: PacerRate,
}

/// Handles one packet from the reliable stream: input events (§10) and
/// control messages (§12).
fn handle_stream_packet(
    ctl: &mut ControlState,
    encoder: &mut ServerEncoder,
    fps_shared: &AtomicU32,
    ch: u16,
    buf: &[u8],
) {
    if ch == channel::INPUT {
        // Input channel (§10): fixed wire format, injected via SendInput on
        // Windows (logged elsewhere).
        match protocol::InputMessage::decode(buf) {
            Ok(msg) => server::input::dispatch(&mut *ctl.injector, &msg),
            Err(e) => warn!("bad input message: {e}"),
        }
        return;
    }
    if ch != channel::CONTROL {
        return;
    }
    match ControlMessage::decode(buf) {
        Ok(ControlMessage::MetricsReport { rtt_micros, jitter_micros, loss_permille }) => {
            handle_metrics(
                &mut ctl.quality,
                &mut ctl.forced_video,
                ctl.audio_control.as_ref(),
                &mut ctl.fec_on,
                encoder,
                fps_shared,
                ctl.video_ceiling_kbps,
                ctl.audio_ceiling_kbps,
                &ctl.pacer_rate,
                rtt_micros,
                jitter_micros,
                loss_permille,
            );
        }
        Ok(ControlMessage::RequestKeyframe { display_id }) => {
            if ctl.last_keyframe_req.is_none_or(|t| t.elapsed() >= Duration::from_millis(500)) {
                info!(display_id, "keyframe requested by client (frame loss)");
                encoder.request_keyframe();
                ctl.last_keyframe_req = Some(Instant::now());
            }
        }
        Ok(_) => {} // other control messages: not handled here yet
        Err(e) => warn!("undecodable control message: {e}"),
    }
}

/// Announces the display codec/geometry on the control channel (§8.3/§8.4);
/// also used after a codec-ladder regime switch (§8.5).
async fn announce_display<T: ReliableTransport>(
    legs: &mut Legs<T>,
    encoder: &mut ServerEncoder,
    width: u32,
    height: u32,
) -> Result<()> {
    let info = ControlMessage::DisplayInfo {
        display_id: 0,
        width,
        height,
        codec: encoder.codec_type().wire_id(),
    };
    legs.send_control(&info.encode()?).await?;
    Ok(())
}

/// Display encoder used by the server: either a plain preset encoder or the
/// codec ladder (§8.5), kept concrete so congestion control can force its
/// regime (§12.5) without downcasting a trait object.
enum ServerEncoder {
    #[cfg(feature = "ladder")]
    Ladder(Box<codec::LadderEncoder>),
    Simple(Box<dyn codec::VideoEncoder>),
}

impl codec::VideoEncoder for ServerEncoder {
    fn encode(&mut self, frame: &codec::VideoFrame<'_>) -> codec::Result<codec::EncodedPacket> {
        match self {
            #[cfg(feature = "ladder")]
            Self::Ladder(e) => e.encode(frame),
            Self::Simple(e) => e.encode(frame),
        }
    }

    fn request_keyframe(&mut self) {
        match self {
            #[cfg(feature = "ladder")]
            Self::Ladder(e) => e.request_keyframe(),
            Self::Simple(e) => e.request_keyframe(),
        }
    }

    fn set_bitrate(&mut self, kbps: u32) -> codec::Result<()> {
        match self {
            #[cfg(feature = "ladder")]
            Self::Ladder(e) => e.set_bitrate(kbps),
            Self::Simple(e) => e.set_bitrate(kbps),
        }
    }

    fn codec_type(&self) -> codec::VideoCodecType {
        match self {
            #[cfg(feature = "ladder")]
            Self::Ladder(e) => e.codec_type(),
            Self::Simple(e) => e.codec_type(),
        }
    }

    fn take_codec_change(&mut self) -> Option<codec::VideoCodecType> {
        match self {
            #[cfg(feature = "ladder")]
            Self::Ladder(e) => e.take_codec_change(),
            Self::Simple(e) => e.take_codec_change(),
        }
    }
}

impl ServerEncoder {
    /// Orders the codec ladder into its video regime (VP9, real rate control)
    /// or releases it back to the base regime. No-op for plain encoders.
    fn force_video_regime(&mut self, on: bool) {
        #[cfg(feature = "ladder")]
        if let Self::Ladder(e) = self {
            e.force_level(if on { codec::Level::High } else { codec::Level::Low });
        }
        #[cfg(not(feature = "ladder"))]
        let _ = (self, on);
    }
}

/// Builds the display encoder from `--preset`: a named preset, or the codec
/// ladder (`--preset ladder`, feature `ladder`) which starts in rects-JPEG
/// and switches to VP9 on sustained video-like change (§8.5).
fn build_encoder(args: &Args, width: u32, height: u32) -> Result<ServerEncoder> {
    #[cfg(feature = "ladder")]
    if args.preset == "ladder" {
        let kbps = args.bitrate.unwrap_or(codec::DEFAULT_BITRATE_KBPS);
        let encoder = codec::LadderEncoder::new(
            width,
            height,
            codec::LadderPolicy::balanced(),
            kbps,
            80,
        )?;
        info!(width, height, kbps, "codec ladder ready (rects-jpeg <-> vp9)");
        return Ok(ServerEncoder::Ladder(Box::new(encoder)));
    }
    let preset = CodecPreset::by_name(&args.preset)
        .with_context(|| format!("unknown preset: {}", args.preset))?;
    let preset = match args.bitrate {
        Some(kbps) => preset.with_bitrate(kbps),
        None => preset,
    };
    let encoder = preset.build_encoder(width, height)?;
    info!(preset = preset.name, codec = ?preset.codec, width, height, "display encoder ready");
    Ok(ServerEncoder::Simple(encoder))
}

/// Frame producer selected by `--source`: the synthetic pattern everywhere,
/// or the real desktop via DXGI on Windows (`windows-capture` feature).
enum Source {
    Synthetic(SyntheticSource),
    #[cfg(all(windows, feature = "windows-capture"))]
    Dxgi {
        source: Box<server::dxgi::DxgiSource>,
        /// Keeps the last captured frame alive so the encoder can borrow it.
        last: Option<server::capture::CapturedFrame>,
    },
}

impl Source {
    fn new(
        kind: &str,
        selector: &server::capture::OutputSelector,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let _ = selector; // used only by the Windows DXGI arm
        match kind {
            "synthetic" => Ok(Self::Synthetic(SyntheticSource::new(width, height))),
            #[cfg(all(windows, feature = "windows-capture"))]
            "dxgi" => Ok(Self::Dxgi {
                source: Box::new(server::dxgi::DxgiSource::new(selector)?),
                last: None,
            }),
            other => bail!(
                "unknown or unavailable source: {other} (dxgi requires Windows + windows-capture feature)"
            ),
        }
    }

    /// Frame geometry `(width, height)` of this source.
    fn dimensions(&self) -> (u32, u32) {
        match self {
            Self::Synthetic(s) => s.dimensions(),
            #[cfg(all(windows, feature = "windows-capture"))]
            Self::Dxgi { source, .. } => {
                use server::capture::FrameSource as _;
                source.dimensions()
            }
        }
    }

    /// Next frame to encode, or `None` when nothing changed on the desktop.
    ///
    /// May block up to `timeout` (DXGI waits for desktop damage); callers on
    /// an async runtime should use [`tokio::task::block_in_place`].
    // Result is only fallible on the Windows DXGI arm; the synthetic arm never
    // fails, hence the lint allowance for non-Windows builds.
    #[allow(clippy::unnecessary_wraps)]
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<codec::VideoFrame<'_>>> {
        match self {
            Self::Synthetic(s) => {
                let _ = timeout;
                Ok(Some(s.next_frame()))
            }
            #[cfg(all(windows, feature = "windows-capture"))]
            Self::Dxgi { source, last } => {
                use server::capture::FrameSource as _;
                *last = source.next_frame(timeout)?;
                match last {
                    Some(f) if !f.dirty_rects.is_empty() => Ok(Some(codec::VideoFrame {
                        width: f.width,
                        height: f.height,
                        stride: f.stride,
                        data: &f.data,
                    })),
                    _ => Ok(None),
                }
            }
        }
    }

    /// Cursor updates observed since the last call (DXGI only; the synthetic
    /// source has no cursor).
    fn take_cursor_update(&mut self) -> Option<server::capture::CursorUpdate> {
        match self {
            Self::Synthetic(_) => None,
            #[cfg(all(windows, feature = "windows-capture"))]
            Self::Dxgi { source, .. } => {
                use server::capture::FrameSource as _;
                source.take_cursor_update()
            }
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("server: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    // Default to `info` when RUST_LOG is unset: the service spawns us with a
    // plain SYSTEM environment and the agent log is the only diagnostics
    // channel (no console with CREATE_NO_WINDOW).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = parse_args()?;
    // Orchestrated mode (Windows agent): hold the session mutex so the
    // service can watch us and a second agent cannot start. Manual/dev runs
    // without a peer agent are unaffected.
    #[cfg(all(windows, feature = "windows-capture"))]
    let _session_mutex = server::session_mutex::acquire()?;
    // The OS does not always extend the desktop onto a freshly attached
    // IddCx monitor (VMs on the Basic Render driver don't); DXGI can only
    // duplicate outputs that are part of the desktop.
    #[cfg(all(windows, feature = "windows-capture"))]
    server::display_topology::ensure_uds_displays_attached(1920, 1080, 60);
    let ticket: [u8; protocol::TICKET_LENGTH] = if let Some(t) = args.ticket {
        t
    } else {
        let mut t = [0u8; protocol::TICKET_LENGTH];
        getrandom::fill(&mut t).map_err(|e| anyhow::anyhow!("ticket generation: {e}"))?;
        t
    };
    println!("ticket: {}", protocol::hex::encode(&ticket));

    let listener = DirectListener::bind(&args.bind.to_string()).await?;
    info!(bind = %args.bind, "listening");
    loop {
        let (raw, peer) = listener.accept().await?;
        info!(%peer, "client connected");
        if let Err(e) = serve(raw, &ticket, &args.key, &args).await {
            warn!(%peer, "session ended: {e:#}");
        }
    }
}
