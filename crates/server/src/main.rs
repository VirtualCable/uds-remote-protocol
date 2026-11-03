// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Development server: serves a synthetic moving test pattern or a real
//! desktop capture (DXGI, Windows) over the protocol.
//!
//! Usage: `server --key <hex64> [--ticket <hex96>] [--bind host:port]
//!                 [--source synthetic|dxgi] [--output N | --output-name SUBSTR]
//!                 [--preset NAME] [--bitrate KBPS]
//!                 [--width N] [--height N] [--fps N] [--max-frames N]
//!                 [--audio-preset NAME] [--audio-source auto|synthetic|uds-loopback|wasapi]
//!                 [--no-audio] [--mic-sink null|wav:PATH]`
//!
//! `--preset` is a name from `codec::PRESETS` (e.g. `raw`, `vp9-balanced`,
//! `vp8-low-latency`); VP8/VP9 presets require the crate features
//! `vp9`/`vp8`. The chosen codec/geometry is announced on the control
//! channel (`DisplayInfo`) before the first frame.
//!
//! `--output N` captures the Nth output of the first adapter that has one
//! (0 = primary, the default — console mirroring); `--output-name SUBSTR`
//! captures the first output whose adapter description or device name
//! contains SUBSTR (case-insensitive), e.g. `--output-name "QXL"`.
//!
//! Audio (§9) is served by default: one Opus frame (preset named by
//! `--audio-preset`, a name from `codec::AUDIO_PRESETS`, default
//! `opus-music`) per datagram on channel `AUDIO_OUT`. `--no-audio` disables
//! it. `--audio-source` picks the capture: `uds-loopback` (the udsaudio
//! driver's render→loopback ring read via the control device — the preferred
//! source when the driver is installed: it bypasses the engine loopback
//! path, which delivered garbage bursts on stream starts), `wasapi` (real
//! desktop loopback, Windows with the `windows-capture` feature),
//! `synthetic` (two-tone generator) or `auto` (the default: driver loopback,
//! then WASAPI, then synthetic; an explicit source failure disables audio
//! with a loud error).
//!
//! The session ticket is printed on startup when not given; pass it to the
//! test client with `--ticket`.
//!
//! Audio-in (§9.2): `--mic-sink wav:PATH` records the client microphone to a
//! WAV file (48 kHz mono) and announces audio-in support to the client; the
//! default `null` keeps audio-in off (no announcement, packets discarded).
//!
//! Privacy screen (M7): the client may negotiate `FLAG_PRIVACY_SCREEN`
//! (testclient `--privacy`). In console-mirror mode the request is logged
//! and ignored — covering the captured display would show the lock screen
//! to the remote user (see the archive/iddcx-display branch).

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
use tracing::{error, info, warn};

struct Args {
    bind: SocketAddr,
    key: [u8; consts::KEY_LENGTH],
    ticket: Option<[u8; protocol::TICKET_LENGTH]>,
    source: String,
    output: server::capture::OutputSelector,
    preset: String,
    /// Codec-ladder sensitivity overrides ("k=v,..."; empty = defaults).
    ladder: String,
    /// Adaptive-fps overrides ("k=v,..."; empty = disabled, §12.7).
    fps_adapt: String,
    bitrate: Option<u32>,
    width: u32,
    height: u32,
    /// Server-fixed console resolution (§18.37): when set, the client's
    /// HELLO/DesktopResize preference is ignored and this mode (or the
    /// closest supported one) is applied for the session.
    fixed_resolution: Option<(u32, u32)>,
    fps: u32,
    max_frames: Option<u32>,
    audio: bool,
    audio_preset: String,
    audio_source: String,
    mic_sink: String,
    /// Storage spool directory (M8); defaults to the per-user spool.
    spool: Option<std::path::PathBuf>,
    /// Per-session telemetry CSV (§12.8); `None` keeps telemetry log-only.
    stats_csv: Option<std::path::PathBuf>,
    /// Validated-connection handoff from the orchestrator service (§18.32):
    /// when set, the agent does not bind its own TCP listener; it rebuilds
    /// the connected socket and serves exactly one session, then exits.
    handoff: Option<service_proto::HandoffInfo>,
}

#[allow(clippy::too_many_lines)] // config file + CLI + agent-config frame seeding
fn parse_args() -> Result<Args> {
    let mut args = Args {
        bind: "127.0.0.1:19790".parse().expect("valid default"),
        key: [0; consts::KEY_LENGTH],
        ticket: None,
        source: "synthetic".to_string(),
        output: server::capture::OutputSelector::Index(0),
        preset: "raw".to_string(),
        ladder: String::new(),
        fps_adapt: String::new(),
        bitrate: None,
        width: 640,
        height: 480,
        fixed_resolution: None,
        fps: 30,
        max_frames: None,
        audio: true,
        audio_preset: "opus-music".to_string(),
        audio_source: "auto".to_string(),
        mic_sink: "null".to_string(),
        spool: None,
        stats_csv: None,
        handoff: None,
    };
    let mut key_seen = false;
    // A config file (reference/standalone mode) seeds the defaults first;
    // any explicit CLI flag still overrides it.
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    if raw_args.iter().any(|a| a == "--config") {
        key_seen = apply_config_file(&mut args, &raw_args)?;
    }
    let mut it = raw_args.into_iter();
    while let Some(arg) = it.next() {
        if arg == "--config" {
            it.next(); // already applied above
            continue;
        }
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
            // Empty output_name = console mirroring: capture the primary
            // display of the console session.
            args.output = if config.output_name.is_empty() {
                server::capture::OutputSelector::Index(0)
            } else {
                server::capture::OutputSelector::NameContains(config.output_name)
            };
            args.preset = config.preset;
            args.ladder = config.ladder;
            args.fps_adapt = config.fps_adapt;
            if config.fixed_resolution.0 > 0 && config.fixed_resolution.1 > 0 {
                args.fixed_resolution = Some(config.fixed_resolution);
            }
            if !config.stats_csv.is_empty() {
                args.stats_csv = Some(config.stats_csv.into());
            }
            args.handoff = config.handoff;
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
            "--ladder" => args.ladder = value,
            "--fps-adapt" => args.fps_adapt = value,
            "--bitrate" => args.bitrate = Some(value.parse().context("bad --bitrate")?),
            "--width" => args.width = value.parse().context("bad --width")?,
            "--height" => args.height = value.parse().context("bad --height")?,
            "--resolution" => {
                args.fixed_resolution = Some(parse_resolution(&value).context("bad --resolution")?);
            }
            "--fps" => args.fps = value.parse().context("bad --fps")?,
            "--max-frames" => args.max_frames = Some(value.parse().context("bad --max-frames")?),
            "--audio-preset" => args.audio_preset = value,
            "--audio-source" => args.audio_source = value,
            "--mic-sink" => args.mic_sink = value,
            "--spool" => args.spool = Some(value.into()),
            "--stats-csv" => args.stats_csv = Some(value.into()),
            other => bail!("unknown argument: {other}"),
        }
    }
    if !key_seen {
        bail!("--key <hex64> or --agent-config-stdin is required");
    }
    Ok(args)
}

/// Parses a `"WIDTHxHEIGHT"` resolution string (e.g. `1920x1080`).
fn parse_resolution(value: &str) -> Result<(u32, u32)> {
    let (w, h) = value
        .split_once(['x', 'X'])
        .context("expected WIDTHxHEIGHT")?;
    let w: u32 = w.trim().parse().context("bad width")?;
    let h: u32 = h.trim().parse().context("bad height")?;
    anyhow::ensure!(w > 0 && h > 0, "resolution must be non-zero");
    Ok((w, h))
}

/// Applies a TOML config file (reference/standalone mode) to `args`.
/// Returns whether the shared key was set by the file.
fn apply_config_file(args: &mut Args, raw_args: &[String]) -> Result<bool> {
    let pos = raw_args
        .iter()
        .position(|a| a == "--config")
        .expect("caller checked");
    let path = raw_args
        .get(pos + 1)
        .map(std::path::PathBuf::from)
        .or_else(server::config::find_config_file)
        .context("--config given but no config file found")?;
    let config = server::config::load(&path)?;
    info!(path = %path.display(), "loaded config file");
    args.key = protocol::hex::decode(&config.shared_key).context("bad shared_key")?;
    args.bind = config.bind;
    if config.connection_method == server::config::ConnectionMethod::Standalone {
        let standalone = config
            .standalone
            .as_ref()
            .context("connection_method = \"standalone\" requires a [standalone] section")?;
        anyhow::ensure!(
            !standalone.ticket.is_empty(),
            "standalone.ticket must not be empty"
        );
        args.ticket = Some(server::config::ticket_bytes(&standalone.ticket));
        args.width = standalone.width;
        args.height = standalone.height;
        if let Some(resolution) = &standalone.resolution {
            args.fixed_resolution =
                Some(parse_resolution(resolution).context("bad standalone.resolution")?);
        }
        args.fps = standalone.fps;
        args.preset.clone_from(&standalone.preset);
        args.ladder = standalone.ladder.to_wire();
        args.fps_adapt = standalone
            .adaptive_fps
            .as_ref()
            .map(server::config::AdaptiveFpsTuning::to_wire)
            .unwrap_or_default();
        args.stats_csv = standalone.stats_csv.as_ref().map(std::path::PathBuf::from);
        args.audio = standalone.channels.audio;
        info!("standalone mode: fixed ticket active (reference implementation)");
    }
    Ok(true)
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
/// How long a keyframe send may wait for paced-queue room before giving up
/// (the session loop stalls during the wait; audio has its own leg).
const KEYFRAME_QUEUE_WAIT: Duration = Duration::from_millis(25);

/// Safety net (§12.2): loss detection + forced keyframes should keep the
/// predictive chain healthy, but a periodic keyframe bounds the worst-case
/// recovery time even if the whole request path fails.
const KEYFRAME_SAFETY_INTERVAL: Duration = Duration::from_secs(30);

/// Dead-client detection (§18.39): the client probes at least once per
/// second on every leg; no inbound traffic at all for this long means the
/// network is gone (a crashed peer can keep its TCP socket open for hours
/// at the kernel default keepalive). The agent exits so the service frees
/// the session slot and a new client can connect.
const CLIENT_LIVENESS_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a resize may go without delivering a frame before the console
/// mode is reverted (§18.37). Secure-desktop transitions hold the
/// duplication denied for a few seconds, so this must be generous.
const RESIZE_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

struct Legs<T: ReliableTransport> {
    /// UDP send half, shared with the paced display-sender task (§12.6).
    udp: Option<Arc<Mutex<transport::DatagramSink<UdpTransport>>>>,
    stream: Box<EncryptedStream<T>>,
    /// Display fragments queued for the paced sender task; `None` on the
    /// stream fallback, where the kernel's TCP flow control already paces.
    frag_tx: Option<mpsc::Sender<Vec<u8>>>,
}

impl<T: ReliableTransport> Legs<T> {
    /// Minimum free datagram slots the paced queue must keep before the
    /// session loop encodes another frame (§12.6 backpressure): below this,
    /// the capture is skipped instead of piling latency into the queue.
    const FRAG_QUEUE_HEADROOM: usize = 384;

    /// True when the paced queue is congested enough that encoding a new
    /// frame would just pile up delay (§12.6). Always false on the stream
    /// fallback (TCP flow control applies its own backpressure).
    fn frag_queue_limited(&self) -> bool {
        match &self.frag_tx {
            Some(tx) => tx.capacity() < Self::FRAG_QUEUE_HEADROOM,
            None => false,
        }
    }

    /// Free datagram slots in the paced queue (`None` on the stream
    /// fallback, where there is no bounded queue to watch).
    fn frag_queue_free(&self) -> Option<usize> {
        self.frag_tx.as_ref().map(mpsc::Sender::capacity)
    }

    /// Queues one frame's fragments **all-or-nothing**: a partially-sent
    /// frame is undecodable waste that still burns drain rate, so when the
    /// paced queue cannot hold the whole frame nothing is queued at all
    /// (returns `Ok(false)`; the caller must force a keyframe). Only the
    /// session task pushes, so the capacity check cannot race the sends.
    ///
    /// Keyframes are recovery-critical: dropping one restarts the whole
    /// request → encode → big-frame cycle under the same congestion that
    /// caused the loss, so instead of dropping we wait briefly for the
    /// pacer to drain (bounded by [`KEYFRAME_QUEUE_WAIT`]) before giving up.
    async fn send_frame(&mut self, frags: Vec<Vec<u8>>, keyframe: bool) -> transport::Result<bool> {
        let Some(tx) = &self.frag_tx else {
            for frag in &frags {
                self.stream.send(channel::DISPLAY, frag).await?;
            }
            return Ok(true);
        };
        if tx.capacity() < frags.len() {
            if !keyframe {
                return Ok(false);
            }
            let deadline = Instant::now() + KEYFRAME_QUEUE_WAIT;
            while tx.capacity() < frags.len() {
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        for frag in frags {
            match tx.try_send(frag) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    unreachable!("capacity checked and this task is the only producer")
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Err(transport::TransportError::Io(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "display sender task ended",
                    )));
                }
            }
        }
        Ok(true)
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
    // Sends in batches (§12.6): one pacing sleep per batch, not per
    // datagram — per-datagram sleeps fall below the OS timer granularity
    // and cap the drain at ~1000 dgrams/s whatever the configured rate.
    // The token bucket keeps the *average* rate; a batch is exactly the
    // burst the bucket size already allows.
    let mut batch: Vec<Vec<u8>> = Vec::with_capacity(96);
    while let Some(frag) = rx.recv().await {
        let mut bytes = frag.len();
        batch.push(frag);
        let target = pacer.batch_target_bytes();
        while batch.len() < batch.capacity() && bytes < target {
            match rx.try_recv() {
                Ok(frag) => {
                    bytes += frag.len();
                    batch.push(frag);
                }
                // Empty: nothing more queued right now. Disconnected: send
                // what we drained; the outer recv ends the task next turn.
                Err(_) => break,
            }
        }
        pacer.pace(bytes).await;
        let mut sink = sink.lock().await;
        for frag in batch.drain(..) {
            if let Err(e) = sink.send(channel::DISPLAY, &frag).await {
                warn!("display sender: send failed, ending task: {e}");
                return;
            }
        }
        drop(sink);
    }
}

/// Capture source startup report: the display geometry. (Pointer
/// coordinates are desktop-normalized end to end, §18.37, so the output
/// device name is no longer needed for input translation.)
#[derive(Debug)]
struct ReadyInfo {
    width: u32,
    height: u32,
}

/// Capture thread → main loop messages.
#[derive(Debug)]
enum CaptureMsg {
    /// First message: the source is up (display geometry) or failed.
    Ready(Result<ReadyInfo>),
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
) -> (
    mpsc::Receiver<CaptureMsg>,
    std::thread::JoinHandle<()>,
    Arc<AtomicBool>,
) {
    let kind = args.source.clone();
    let (width, height, output) = (args.width, args.height, args.output.clone());
    let (tx, rx) = mpsc::channel(2);
    // Cooperative shutdown for the source's internal retry loops (DXGI
    // reinit can block for the whole reinit budget otherwise, and
    // `stop_capture`'s join would freeze the session loop behind it).
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let handle = std::thread::spawn(move || {
        let mut source = match Source::new(&kind, &output, width, height, &stop_thread) {
            Ok(s) => s,
            Err(e) => {
                let _ = tx.blocking_send(CaptureMsg::Ready(Err(e)));
                return;
            }
        };
        let (width, height) = source.dimensions();
        let ready = ReadyInfo { width, height };
        if tx.blocking_send(CaptureMsg::Ready(Ok(ready))).is_err() {
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
                    // DXGI dies the instant a secure desktop (lock, UAC,
                    // Winlogon) takes the console — usually BEFORE the
                    // watcher's transition reaches the session loop, where a
                    // dead capture channel with no secure switch pending is
                    // fatal (§18.41, §18.52). Wait out a short grace: if a
                    // secure desktop now owns the console, idle until the
                    // stop flag (the secure branch stops/restarts capture
                    // properly); otherwise the failure is real — die.
                    #[cfg(all(windows, feature = "windows-capture"))]
                    if secure_grace_wait(&stop_thread) {
                        info!("capture lost to a secure desktop; idling until the transition ({e:#})");
                        while !stop_thread.load(Ordering::Relaxed) && !tx.is_closed() {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        return;
                    }
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
    (rx, handle, stop)
}

/// Grace window after a capture failure (§18.52): polls for a secure
/// desktop owning the console — the usual reason DXGI dies mid-session —
/// for up to two seconds, honouring the stop flag. `true` means the failure
/// is the secure-desktop transition, not a real capture error.
#[cfg(all(windows, feature = "windows-capture"))]
fn secure_grace_wait(stop: &Arc<AtomicBool>) -> bool {
    for _ in 0..40 {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        if server::secure_helper::secure_desktop_active() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Stops the capture thread: the stop flag breaks any internal retry loop
/// (a DXGI reinit can otherwise block for the whole reinit budget — with a
/// secure desktop owning the console it ALWAYS would, §18.41 — freezing the
/// session loop behind the join), dropping the receiver closes the channel
/// (the thread exits on its next liveness check or failed send), then the
/// join waits for the DXGI duplication to be fully released. Must run BEFORE
/// any console mode change (§18.37): a mode applied with a live duplication
/// poisons capture for the rest of the process on the Basic Render Driver
/// stack — every new duplication then fails with access-lost forever.
fn stop_capture(
    rx_slot: &mut Option<mpsc::Receiver<CaptureMsg>>,
    handle_slot: &mut Option<std::thread::JoinHandle<()>>,
    stop_slot: &mut Option<Arc<AtomicBool>>,
) {
    if let Some(stop) = stop_slot.take() {
        stop.store(true, Ordering::Relaxed);
    }
    rx_slot.take();
    if let Some(handle) = handle_slot.take()
        && handle.join().is_err()
    {
        warn!("capture thread panicked");
    }
}

/// Spawns a fresh capture thread into the slots (after [`stop_capture`]).
fn start_capture(
    args: &Args,
    fps_shared: &Arc<AtomicU32>,
    clock_start: Instant,
    rx_slot: &mut Option<mpsc::Receiver<CaptureMsg>>,
    handle_slot: &mut Option<std::thread::JoinHandle<()>>,
    stop_slot: &mut Option<Arc<AtomicBool>>,
) {
    let (rx, handle, stop) = spawn_capture(args, Arc::clone(fps_shared), clock_start);
    *rx_slot = Some(rx);
    *handle_slot = Some(handle);
    *stop_slot = Some(stop);
}

/// Receives the next capture message; pends forever while the capture
/// thread is down for a display-mode change (§18.37), so it can sit in a
/// `select!` branch.
async fn recv_capture(rx: &mut Option<mpsc::Receiver<CaptureMsg>>) -> Option<CaptureMsg> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// Receives the next secure-desktop transition; pends forever when no
/// watcher is running (non-Windows builds, non-DXGI sources), so it can sit
/// in a `select!` branch.
async fn recv_secure(rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<bool>>) -> bool {
    match rx {
        Some(r) => match r.recv().await {
            Some(active) => active,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

/// Agent-side secure-mode state (§18.41): the helper-pipe threads and the
/// input writer shared with the input dispatcher.
struct SecureState {
    /// Input pipe writer (agent → helper); `None` until the helper connects.
    input: std::sync::Arc<std::sync::Mutex<Option<std::fs::File>>>,
}

/// Shared slot for the SYSTEM input injector's pipe (§18.42): `Some(file)`
/// once the service-spawned injector connects; input is forwarded there
/// instead of being injected locally (medium IL cannot reach elevated
/// windows; SYSTEM can).
type InjectorSlot = std::sync::Arc<std::sync::Mutex<Option<std::fs::File>>>;

/// Spawns the injector-pipe acceptor thread (§18.42): the pipe server waits
/// for the SYSTEM injector; until it connects the slot stays `None` and
/// input falls back to the local (user-token) injector. Portable stub
/// outside Windows: the slot simply never fills.
#[cfg_attr(
    not(all(windows, feature = "windows-capture")),
    expect(unused_variables, reason = "stub: the injector exists only on Windows")
)]
fn spawn_injector_pipe(session_id: u32) -> InjectorSlot {
    let slot: InjectorSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
    #[cfg(all(windows, feature = "windows-capture"))]
    {
        let target = std::sync::Arc::clone(&slot);
        std::thread::Builder::new()
            .name("injector-input".into())
            .spawn(move || {
                match server::secure_pipe::win::accept_pipe(
                    &server::secure_pipe::injector_pipe_name(session_id),
                    false,
                ) {
                    Ok(f) => {
                        info!("SYSTEM input injector connected");
                        *target.lock().expect("injector pipe slot") = Some(f);
                    }
                    Err(e) => warn!("injector pipe: {e:#}"),
                }
            })
            .map_err(|e| warn!("injector pipe thread: {e}"))
            .ok();
    }
    slot
}

/// Spawns the two secure-mode pipe threads (frames acceptor + input
/// acceptor) and returns the capture receiver that feeds the session loop.
/// Only reachable on Windows DXGI builds; the portable stub is never used
/// (the secure branch pends forever without a watcher).
#[cfg_attr(
    not(all(windows, feature = "windows-capture")),
    expect(unused_variables, reason = "stub: the secure branch never fires")
)]
fn spawn_secure_pipes(
    session_id: u32,
    clock_start: Instant,
) -> (mpsc::Receiver<CaptureMsg>, SecureState) {
    let (tx, rx) = mpsc::channel(2);
    #[cfg(all(windows, feature = "windows-capture"))]
    {
        std::thread::Builder::new()
            .name("secure-frames".into())
            .spawn(move || secure_frames_main(session_id, tx, clock_start))
            .map_err(|e| warn!("secure frames thread: {e}"))
            .ok();
    }
    let input = std::sync::Arc::new(std::sync::Mutex::new(None));
    #[cfg(all(windows, feature = "windows-capture"))]
    {
        let slot = std::sync::Arc::clone(&input);
        std::thread::Builder::new()
            .name("secure-input".into())
            .spawn(move || {
                match server::secure_pipe::win::accept_pipe(
                    &server::secure_pipe::input_pipe_name(session_id),
                    false,
                ) {
                    Ok(f) => *slot.lock().expect("input pipe slot") = Some(f),
                    Err(e) => warn!("secure input pipe: {e:#}"),
                }
            })
            .map_err(|e| warn!("secure input thread: {e}"))
            .ok();
    }
    (rx, SecureState { input })
}

/// Frames pipe acceptor loop: one helper connection, then frame messages
/// until EOF (helper exits when the secure desktop goes away).
#[cfg(all(windows, feature = "windows-capture"))]
fn secure_frames_main(session_id: u32, tx: mpsc::Sender<CaptureMsg>, clock_start: Instant) {
    let mut frames = match server::secure_pipe::win::accept_pipe(
        &server::secure_pipe::frames_pipe_name(session_id),
        true,
    ) {
        Ok(f) => f,
        Err(e) => {
            warn!("secure frames pipe: {e:#}");
            return;
        }
    };
    let mut ready_sent = false;
    loop {
        if tx.is_closed() {
            return; // session over
        }
        match server::secure_pipe::read_msg(&mut frames) {
            Ok(Some((server::secure_pipe::TAG_FRAME, payload))) => {
                let Ok((w, h, pixels)) = server::secure_pipe::decode_frame(&payload) else {
                    warn!("secure helper sent a malformed frame");
                    continue;
                };
                if !ready_sent {
                    if tx
                        .blocking_send(CaptureMsg::Ready(Ok(ReadyInfo { width: w, height: h })))
                        .is_err()
                    {
                        return;
                    }
                    ready_sent = true;
                }
                let frame = OwnedFrame {
                    width: w,
                    height: h,
                    stride: w as usize * 4,
                    capture_ts_ms: millis_since(clock_start),
                    data: pixels.to_vec(),
                };
                if tx.blocking_send(CaptureMsg::Frame(frame)).is_err() {
                    return; // session over
                }
            }
            Ok(Some((tag, _))) => warn!(tag, "secure frames pipe: unexpected tag"),
            Ok(None) | Err(_) => return, // helper gone
        }
    }
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
        // Level telemetry (audio counterpart of the display pipeline line):
        // peak/RMS of the source over 5 s windows, so "the client hears
        // nothing / hears noise" is diagnosable from the agent log alone.
        let mut level_peak = 0i32;
        let mut level_sq = 0u64;
        let mut level_samples = 0u64;
        let mut level_last = Instant::now();
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
            for &s in &pcm {
                let v = i32::from(s).abs();
                level_peak = level_peak.max(v);
                level_sq += u64::try_from(v * v).unwrap_or(0); // v >= 0 by abs()
            }
            level_samples += pcm.len() as u64;
            if level_last.elapsed() >= STATS_LOG_INTERVAL {
                #[allow(clippy::cast_precision_loss)] // RMS of i16 peaks is small
                let rms = if level_samples == 0 {
                    0.0
                } else {
                    (level_sq as f64 / level_samples as f64).sqrt()
                };
                info!(
                    peak = level_peak,
                    rms = (rms * 10.0).round() / 10.0,
                    "audio source level"
                );
                level_peak = 0;
                level_sq = 0;
                level_samples = 0;
                level_last = Instant::now();
            }
            // PoC DTX (§9.1): only the silence onset goes out (keepalives
            // disabled — the video leg keeps UDP warm, and ~500 ms-spaced
            // keepalives flapped the client's A/V clock liveness). Skipped
            // frames do not advance `seq` (the client sees fewer frames,
            // not losses).
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
    #[cfg(all(windows, feature = "windows-capture"))]
    let uds_loopback = |explicit: bool| {
        match server::audio_uds::UdsLoopbackSource::new(
            preset.sample_rate,
            preset.channels,
            preset.frame_samples(),
        ) {
            Ok(s) => Some(Box::new(s) as Box<dyn AudioSource>),
            Err(e) => {
                if explicit {
                    error!("UDS driver loopback init failed; audio disabled: {e:#}");
                } else {
                    info!("UDS driver loopback unavailable ({e:#}); trying WASAPI");
                }
                None
            }
        }
    };
    match kind {
        "synthetic" => Some(synthetic()),
        #[cfg(all(windows, feature = "windows-capture"))]
        "uds-loopback" => uds_loopback(true),
        #[cfg(all(windows, feature = "windows-capture"))]
        "wasapi" => wasapi(true),
        #[cfg(not(all(windows, feature = "windows-capture")))]
        "wasapi" => {
            error!("--audio-source wasapi needs Windows with the windows-capture feature; audio disabled");
            None
        }
        "auto" => {
            // Prefer the driver ring: it is entirely under our control and
            // cannot exhibit the engine-loopback garbage bursts. Fall back
            // to WASAPI loopback (driver not installed) and then synthetic.
            #[cfg(all(windows, feature = "windows-capture"))]
            {
                if let Some(s) = uds_loopback(false) {
                    return Some(s);
                }
                if let Some(s) = wasapi(false) {
                    return Some(s);
                }
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
fn handle_metrics(
    ctl: &mut ControlState,
    encoder: &mut ServerEncoder,
    rtt_micros: u64,
    jitter_micros: u64,
    loss_permille: u16,
) {
    let loss = f64::from(loss_permille) / 1000.0;
    ctl.last_metrics = Some((rtt_micros, jitter_micros, loss_permille));
    // FEC is loss-driven, not level-driven: evaluate it on every report.
    if let Some(ac) = &ctl.audio_control {
        let fec = protocol::fec_update(ctl.fec_on, loss);
        if fec != ctl.fec_on {
            ctl.fec_on = fec;
            ac.set_fec(fec);
            info!(fec, loss_permille, "audio FEC toggled");
        }
    }
    let controller = ctl.quality.get_or_insert_with(|| {
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
        ctl.video_ceiling_kbps,
        ctl.audio_ceiling_kbps,
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
    ctl.pacer_rate.set_from_video_kbps(t.video_kbps);
    if let Some(ac) = &ctl.audio_control {
        // The audio thread applies the new target at the next frame (§12.5).
        ac.set_target_kbps(t.audio_kbps);
    }
    // The quality target is a ceiling; the adaptive-fps controller (§12.7)
    // may still hold the capture below it when the encoder is the limit.
    ctl.quality_fps = t.fps;
    ctl.apply_fps();
    match (event.force_video_codec, ctl.forced_video) {
        (true, false) => {
            encoder.force_video_regime(true);
            ctl.forced_video = true;
        }
        (false, true) => {
            encoder.force_video_regime(false);
            ctl.forced_video = false;
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

/// Display-pipeline telemetry accumulator (§12.6 diagnostics): one log
/// line every [`STATS_LOG_INTERVAL`] with sizes, encode times, backpressure
/// and drop counters, the ladder state and the pacer rate. When a CSV path
/// is configured, the same data lands there as one row per interval (§12.8).
#[derive(Debug)]
struct FrameStats {
    frames: u64,
    /// Rect-codec frames with no changes (encode ran, nothing sent).
    empty: u64,
    encoded_bytes: u64,
    max_frame_bytes: usize,
    encode_micros: u64,
    max_encode_micros: u64,
    /// Frames skipped by backpressure before encoding.
    skipped_bp: u64,
    /// Whole frames dropped because the paced queue could not hold them.
    dropped_whole: u64,
    /// Smallest free datagram slots seen in the paced queue.
    min_queue_free: usize,
    last_log: Instant,
    /// Process CPU anchor (ms, total since start) at the last interval edge.
    cpu_anchor_ms: Option<f64>,
    /// CPU consumed by this process during the last finished interval (ms).
    cpu_ms_window: Option<u64>,
    /// Telemetry CSV sink (header already written); `None` = log only.
    csv: Option<std::io::BufWriter<std::fs::File>>,
}

/// Telemetry cadence for the display pipeline.
const STATS_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// CSV header of the per-interval telemetry rows (§12.8). Keep it in sync
/// with [`FrameStats::csv_row`].
const STATS_CSV_HEADER: &str = "time,frames,sent,empty,skipped_bp,dropped_whole,avg_encode_ms,max_encode_ms,avg_kb,max_kb,min_queue_free,pacer_mbps,ladder_level,ladder_ema_pct,quality_fps,effective_fps,adaptive_load,rtt_ms,jitter_ms,loss_permille,proc_cpu_ms";

/// Process CPU time (user+kernel) in milliseconds, Windows `GetProcessTimes`
/// or Linux `/proc/self/stat`. `None` on platforms without an implementation.
#[cfg(all(windows, feature = "windows-capture"))]
fn process_cpu_ms() -> Option<f64> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: all out-params point to valid FILETIME locals.
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    }
    .ok()?;
    let ticks = |ft: FILETIME| (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    // FILETIME ticks are 100 ns.
    Some((ticks(kernel) + ticks(user)) as f64 / 10_000.0)
}

/// See the Windows variant.
#[cfg(target_os = "linux")]
fn process_cpu_ms() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let after_paren = stat.rsplit(')').next()?;
    let fields: Vec<&str> = after_paren.split_whitespace().collect();
    let ticks = fields.get(11)?.parse::<f64>().ok()?
        + fields.get(12)?.parse::<f64>().ok()?;
    Some(ticks * 10.0) // USER_HZ=100 → 10 ms per tick
}

/// See the Windows variant.
#[cfg(not(any(all(windows, feature = "windows-capture"), target_os = "linux")))]
fn process_cpu_ms() -> Option<f64> {
    None
}


impl FrameStats {
    fn new(queue_free: usize, csv_path: Option<&std::path::Path>) -> Self {
        let csv = csv_path.and_then(|path| {
            match std::fs::File::create(path) {
                Ok(file) => {
                    use std::io::Write as _;
                    let mut writer = std::io::BufWriter::new(file);
                    if let Err(e) = writeln!(writer, "{STATS_CSV_HEADER}") {
                        warn!(error = %e, "stats CSV header write failed; CSV disabled");
                        return None;
                    }
                    info!(path = %path.display(), "stats CSV active");
                    Some(writer)
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "cannot create the stats CSV; CSV disabled");
                    None
                }
            }
        });
        Self {
            frames: 0,
            empty: 0,
            encoded_bytes: 0,
            max_frame_bytes: 0,
            encode_micros: 0,
            max_encode_micros: 0,
            skipped_bp: 0,
            dropped_whole: 0,
            min_queue_free: queue_free,
            last_log: Instant::now(),
            cpu_anchor_ms: process_cpu_ms(),
            cpu_ms_window: None,
            csv,
        }
    }

    /// One CSV row for the finished interval (§12.8). Optional fields render
    /// empty (ladder on plain encoders, adaptive load without the adapter,
    /// link metrics before the first client report).
    #[allow(clippy::cast_precision_loss)] // telemetry rounding is fine
    fn csv_row(&self, encoder: &ServerEncoder, ctl: &ControlState) -> String {
        let sent = self.frames - self.empty - self.skipped_bp - self.dropped_whole;
        let n_encoded = self.frames - self.skipped_bp;
        let avg_encode_ms = self.encode_micros.checked_div(n_encoded).unwrap_or(0) / 1000;
        let avg_kb = self.encoded_bytes.checked_div(sent).unwrap_or(0) / 1000;
        let (level, ema) = encoder.ladder_snapshot();
        let level = level.map_or(String::new(), |l| format!("{l:?}").to_lowercase());
        let ema = ema.map_or(String::new(), |e| format!("{e:.1}"));
        let load = ctl
            .fps_adapter
            .as_ref()
            .and_then(server::fps::FpsAdapter::load)
            .map_or(String::new(), |l| format!("{l:.2}"));
        let (rtt_ms, jitter_ms, loss) = ctl.last_metrics.map_or(
            (String::new(), String::new(), String::new()),
            |(rtt, jitter, loss)| {
                (
                    format!("{:.2}", rtt as f64 / 1000.0),
                    format!("{:.2}", jitter as f64 / 1000.0),
                    loss.to_string(),
                )
            },
        );
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let cpu_ms = self
            .cpu_ms_window
            .map_or_else(String::new, |c| c.to_string());
        format!(
            "{time},{},{sent},{},{},{},{avg_encode_ms},{},{avg_kb},{},{},{},{level},{ema},{},{},{load},{rtt_ms},{jitter_ms},{loss},{cpu_ms}",
            self.frames,
            self.empty,
            self.skipped_bp,
            self.dropped_whole,
            self.max_encode_micros / 1000,
            self.max_frame_bytes / 1000,
            self.min_queue_free,
            ctl.pacer_rate.bps() / 1_000_000,
            ctl.quality_fps,
            ctl.effective_fps(),
        )
    }

    /// Emits the period line (and CSV row when active) and resets the
    /// accumulators.
    fn log_and_reset(&mut self, encoder: &ServerEncoder, ctl: &ControlState) {
        let sent = self.frames - self.empty - self.skipped_bp - self.dropped_whole;
        let n_encoded = self.frames - self.skipped_bp;
        #[allow(clippy::cast_possible_truncation)] // micros per frame fit u64
        let avg_encode_ms = self.encode_micros.checked_div(n_encoded).unwrap_or(0) / 1000;
        let avg_kb = self.encoded_bytes.checked_div(sent).unwrap_or(0) / 1000;
        let (level, ema) = encoder.ladder_snapshot();
        info!(
            frames = self.frames,
            sent,
            empty = self.empty,
            skipped_bp = self.skipped_bp,
            dropped_whole = self.dropped_whole,
            avg_kb,
            max_kb = self.max_frame_bytes / 1000,
            avg_encode_ms,
            max_encode_ms = self.max_encode_micros / 1000,
            min_queue_free = self.min_queue_free,
            pacer_mbps = ctl.pacer_rate.bps() / 1_000_000,
            ladder_level = ?level,
            ladder_ema_pct = ema.map(|e| (e * 10.0).round() / 10.0),
            "display pipeline"
        );
        if let Some(anchor) = self.cpu_anchor_ms {
            let now = process_cpu_ms();
            // Window deltas are small; rounding to whole ms is fine.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                self.cpu_ms_window = now.map(|n| (n - anchor).max(0.0).round() as u64);
            }
            if now.is_some() {
                self.cpu_anchor_ms = now;
            }
        }
        if self.csv.is_some() {
            use std::io::Write as _;
            let row = self.csv_row(encoder, ctl);
            if let Some(csv) = &mut self.csv
                && let Err(e) = writeln!(csv, "{row}")
            {
                warn!(error = %e, "stats CSV write failed; CSV disabled");
                self.csv = None;
            }
        }
        *self = Self {
            csv: self.csv.take(),
            ..Self::new(DISPLAY_QUEUE_DGRAMS, None)
        };
    }
}

/// Encodes one captured frame and sends its fragments; also re-announces
/// the display when the codec ladder switched regime (§8.5). Returns the
/// next frame id.
#[allow(clippy::too_many_arguments)] // session state travels as separate locals
async fn handle_frame<T: ReliableTransport>(
    frame: &OwnedFrame,
    legs: &mut Legs<T>,
    encoder: &mut ServerEncoder,
    frame_id: u32,
    width: u32,
    height: u32,
    ctl: &mut ControlState,
    stats: &mut FrameStats,
    last_keyframe: &mut Instant,
) -> Result<u32> {
    if let Some(free) = legs.frag_queue_free() {
        stats.min_queue_free = stats.min_queue_free.min(free);
    }
    // Backpressure (§12.6): the paced queue is congested — skip this frame
    // entirely (no encode CPU burned, no latency piled up) and force the
    // next sent frame to be a keyframe, since the peer may be missing the
    // deltas we are skipping. The frame id is reused: nothing of it left.
    stats.frames += 1; // every capture event, so `sent` never underflows
    if legs.frag_queue_limited() {
        encoder.request_keyframe();
        stats.skipped_bp += 1;
        return Ok(frame_id);
    }
    // Periodic safety-net keyframe: bounds the worst-case recovery time even
    // if the whole loss-detection/request path fails (§12.2).
    if last_keyframe.elapsed() >= KEYFRAME_SAFETY_INTERVAL {
        encoder.request_keyframe();
    }
    let encode_start = Instant::now();
    let packet = encoder.encode(&codec::VideoFrame {
        width: frame.width,
        height: frame.height,
        stride: frame.stride,
        data: &frame.data,
    })?;
    let encode_micros = u64::try_from(encode_start.elapsed().as_micros()).unwrap_or(u64::MAX);
    stats.encode_micros += encode_micros;
    stats.max_encode_micros = stats.max_encode_micros.max(encode_micros);
    // Adaptive fps (§12.7): the encoder's own cost steers the capture rate.
    ctl.observe_encode(Duration::from_micros(encode_micros));
    if encoder.take_codec_change().is_some() {
        // Codec ladder switched regime: re-announce before the fragments so
        // the peer rebuilds its decoder (§8.5).
        announce_display(legs, encoder, width, height).await?;
    }
    if packet.data.is_empty() {
        stats.empty += 1;
        return Ok(frame_id); // rect codecs: nothing changed on this frame
    }
    stats.encoded_bytes += packet.data.len() as u64;
    stats.max_frame_bytes = stats.max_frame_bytes.max(packet.data.len());
    // Every fragment carries the frame's capture timestamp (§9.1), and the
    // payload opens with the wire id of the codec that produced it (§8.5):
    // the DisplayInfo announce travels on another leg and can lose the race
    // against the first fragments (or be lost outright), so the per-frame
    // tag is what the client trusts to pick the decoder. Frames go out
    // all-or-nothing: a partial frame is undecodable waste.
    let mut tagged = Vec::with_capacity(packet.data.len() + 2);
    tagged.extend_from_slice(&encoder.codec_type().wire_id().to_be_bytes());
    tagged.extend_from_slice(&packet.data);
    let frags = Fragmenter::split(0, frame_id, frame.capture_ts_ms, &tagged);
    if !legs.send_frame(frags, packet.keyframe).await? {
        // The paced queue cannot hold this frame right now (a huge keyframe
        // over a busy link): skip it whole and force a keyframe on the next
        // one (the client's loss recovery would ask for it anyway).
        stats.dropped_whole += 1;
        warn!(frame_id, "paced queue congested; whole frame dropped, keyframe forced");
        encoder.request_keyframe();
        return Ok(frame_id);
    }
    if packet.keyframe {
        *last_keyframe = Instant::now();
    }
    if stats.last_log.elapsed() >= STATS_LOG_INTERVAL {
        stats.log_and_reset(encoder, ctl);
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

async fn serve<T: ReliableTransport>(
    raw: T,
    ticket: &[u8; protocol::TICKET_LENGTH],
    key: &[u8; consts::KEY_LENGTH],
    args: &Args,
) -> Result<()> {
    let stream = EncryptedStream::server_handshake(raw, key, None).await?;
    info!("handshake done");
    serve_stream(stream, ticket, key, args).await
}

/// Session body once the handshake is done (either completed locally in
/// `serve` or received via the service's validated handoff, §18.32).
// The session loop owns capture, display, audio and mic branches; splitting
// it further would only scatter the shared session state.
#[allow(clippy::too_many_lines)]
async fn serve_stream<T: ReliableTransport>(
    mut stream: EncryptedStream<T>,
    ticket: &[u8; protocol::TICKET_LENGTH],
    key: &[u8; consts::KEY_LENGTH],
    args: &Args,
) -> Result<()> {
    if stream
        .peer_hello()
        .is_some_and(|h| h.flags & protocol::FLAG_SMARTCARD != 0)
    {
        info!("client offers smartcard redirection (§17)");
    }

    // Privacy screen (M7): the client asks for it with FLAG_PRIVACY_SCREEN.
    // In console-mirror mode an overlay would cover the very display we
    // capture (DXGI delivers the composed desktop), so the remote user would
    // see the lock screen — the request is logged and ignored. Real privacy
    // needs a separate capture display (the archived IddCx work lives in the
    // archive/iddcx-display branch).
    #[cfg(all(windows, feature = "windows-capture"))]
    if stream
        .peer_hello()
        .is_some_and(|h| h.flags & protocol::FLAG_PRIVACY_SCREEN != 0)
    {
        warn!("privacy screen ignored: incompatible with console-mirror capture");
    }

    // Resolution policy (§18.36/§18.37): a fixed server resolution wins;
    // otherwise approach the client's HELLO preference without exceeding it.
    // Applied before capture starts so the first frames already carry the
    // final geometry; the guard restores the original mode on session end.
    let mut display_mode = server::display_mode::DisplayModeGuard::new();
    {
        let preferred = stream
            .peer_hello()
            .map_or((0, 0), |h| (u32::from(h.pref_width), u32::from(h.pref_height)));
        let policy = server::display_mode::ResolutionPolicy {
            fixed: args.fixed_resolution,
            preferred,
        };
        if display_mode.apply(&policy) {
            info!(?args.fixed_resolution, ?preferred, "session resolution applied");
        }
    }

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
    let mut frame_rx: Option<mpsc::Receiver<CaptureMsg>> = None;
    let mut frame_handle: Option<std::thread::JoinHandle<()>> = None;
    let mut capture_stop: Option<Arc<AtomicBool>> = None;
    start_capture(
        args,
        &fps_shared,
        clock_start,
        &mut frame_rx,
        &mut frame_handle,
        &mut capture_stop,
    );
    let ready = match recv_capture(&mut frame_rx).await {
        Some(CaptureMsg::Ready(r)) => r?,
        other => bail!("capture thread ended before reporting readiness ({other:?})"),
    };
    let (mut width, mut height) = (ready.width, ready.height);
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

    // Storage (M8, model B): spool directory for explicit file transfers.
    // Best-effort like the clipboard: when the directory cannot be created
    // the channel answers errors but the session goes on.
    let mut spool = match server::storage::Spool::open(
        args.spool
            .clone()
            .unwrap_or_else(server::storage::Spool::default_root),
    ) {
        Ok(s) => {
            info!("storage spool active");
            Some(s)
        }
        Err(e) => {
            warn!("storage spool unavailable: {e}");
            None
        }
    };
    // Download chunk pump: 4 x 64 KiB per millisecond tick ≈ 256 MiB/s of
    // headroom; the stream's own backpressure throttles the real rate.
    let mut storage_tick = tokio::time::interval(Duration::from_millis(1));

    // Smartcard (§17): requests originate at the virtual smart card reader
    // driver (UMDF) through the pipe origin; the router resolves replies by
    // request_id. When the pipes cannot be hosted (non-Windows, or another
    // session owns them) the channel stays inert, as with the pre-driver
    // NullSmartcardRelay.
    let (scard_tx, mut scard_rx) = mpsc::unbounded_channel::<protocol::SmartcardMessage>();
    let scard_router = Arc::new(server::smartcard::SmartcardRouter::new(scard_tx));
    #[cfg(all(windows, feature = "windows-capture"))]
    let _scard_origin = server::smartcard_pipe::PipeOrigin::spawn(
        server::smartcard::RouterRelay::new(Arc::clone(&scard_router)),
        tokio::runtime::Handle::current(),
    );

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
        fps_shared: Arc::clone(&fps_shared),
        quality_fps: args.fps.max(1),
        fps_adapter: if args.fps_adapt.is_empty() {
            None
        } else {
            let policy = server::fps::FpsPolicy::from_wire(&args.fps_adapt)?;
            info!(overrides = %args.fps_adapt, "adaptive fps enabled");
            Some(server::fps::FpsAdapter::new(policy, args.fps.max(1)))
        },
        last_metrics: None,
        resize_request: None,
        secure_input: None,
        // §18.42: the pipe acceptor waits for the service-spawned SYSTEM
        // injector; in manual/dev runs nobody connects and input stays local.
        injector_input: Some(spawn_injector_pipe(
            server::secure_pipe::own_session_id().unwrap_or(0),
        )),
    };

    let mut udp_buf = vec![0u8; 2048];
    // Sized for the largest stream record (~64 KiB): cursor shapes (§10)
    // can be a few KiB.
    let mut ctrl_buf = vec![0u8; 65_536];
    // Reusable buffer for audio datagram payloads (header + Opus frame).
    let mut audio_buf = Vec::with_capacity(1024);
    let mut frame_id = 0u32;
    let mut frame_stats = FrameStats::new(DISPLAY_QUEUE_DGRAMS, args.stats_csv.as_deref());
    // Set when a resize was applied and no frame of the new geometry has
    // arrived yet; drives the poison-mode revert (§18.37).
    let mut resize_pending: Option<Instant> = None;
    // Modes that poisoned the capture this session (never delivered a
    // frame after being applied); excluded from future resizes.
    let mut bad_modes: Vec<(u32, u32)> = Vec::new();
    // Seeded in the past so the very first frame goes out as a keyframe even
    // if the encoder's first frame was not one for some reason.
    let mut last_keyframe = Instant::now().checked_sub(KEYFRAME_SAFETY_INTERVAL).unwrap_or_else(Instant::now);
    // Liveness (§18.39): the handshake itself was client traffic.
    let mut last_recv = Instant::now();
    let mut liveness_tick = tokio::time::interval(Duration::from_secs(1));
    // Resize requests get their own fast tick (gated on a pending request):
    // waiting for the 1 s liveness tick added up to a second of dead time
    // to every mode change for no reason (§18.37).
    let mut resize_tick = tokio::time::interval(Duration::from_millis(100));
    // Secure desktop watch (§18.41): while UAC/Winlogon/lock owns the
    // console, DXGI capture is denied and SendInput cannot reach it, so
    // capture+input switch to the service-spawned helper over pipes. Only
    // real desktop capture needs this; the synthetic source never does.
    #[cfg(all(windows, feature = "windows-capture"))]
    let (_secure_watch, mut secure_rx) = if args.source == "dxgi" {
        let (watch, rx) = server::secure_watch::SecureWatch::spawn();
        (Some(watch), Some(rx))
    } else {
        (None, None)
    };
    #[cfg(not(all(windows, feature = "windows-capture")))]
    let mut secure_rx: Option<tokio::sync::mpsc::UnboundedReceiver<bool>> = None;
    let mut secure_state: Option<SecureState> = None;
    loop {
        tokio::select! {
            msg = recv_capture(&mut frame_rx) => {
                match msg {
                    Some(CaptureMsg::Ready(r)) => {
                        // Capture (re)start after a display-mode change
                        // (§18.37): adopt the new geometry, rebuild the
                        // encoder and re-announce the display.
                        match r {
                            Ok(ready) => {
                                info!(
                                    w = ready.width,
                                    h = ready.height,
                                    "capture restarted; rebuilding encoder"
                                );
                                width = ready.width;
                                height = ready.height;
                                encoder = build_encoder(args, width, height)?;
                                announce_display(&mut legs, &mut encoder, width, height).await?;
                            }
                            Err(e) => {
                                // The fresh capture could not initialize on
                                // the new mode: treat it as poisoned — roll
                                // the console back and respawn once. With no
                                // mode applied there is nothing to roll back
                                // to; the session cannot capture at all.
                                warn!("capture failed to start after the mode change: {e:#}");
                                if let Some(mode) = display_mode.applied() {
                                    bad_modes.push(mode);
                                    stop_capture(&mut frame_rx, &mut frame_handle, &mut capture_stop);
                                    display_mode.restore();
                                    start_capture(
                                        args,
                                        &fps_shared,
                                        clock_start,
                                        &mut frame_rx,
                                        &mut frame_handle,
                                        &mut capture_stop,
                                    );
                                    resize_pending = Some(Instant::now());
                                } else {
                                    bail!("capture thread failed to start: {e:#}");
                                }
                            }
                        }
                    }
                    Some(CaptureMsg::Frame(frame)) => {
                        // Any frame proves the capture is healthy again.
                        resize_pending = None;
                        // Geometry change (mid-session resize, §18.37): the
                        // capture recovered from access-lost with a new mode;
                        // rebuild the encoder and re-announce the display.
                        if frame.width != width || frame.height != height {
                            info!(
                                old = ?(width, height),
                                new = ?(frame.width, frame.height),
                                "display geometry changed; rebuilding encoder"
                            );
                            width = frame.width;
                            height = frame.height;
                            encoder = build_encoder(args, width, height)?;
                            announce_display(&mut legs, &mut encoder, width, height).await?;
                        }
                        frame_id = handle_frame(
                            &frame,
                            &mut legs,
                            &mut encoder,
                            frame_id,
                            width,
                            height,
                            &mut ctl,
                            &mut frame_stats,
                            &mut last_keyframe,
                        )
                        .await?;
                        if args.max_frames.is_some_and(|max| frame_id >= max) {
                            info!(frame_id, "max frames reached, closing session");
                            return Ok(());
                        }
                    }
                    Some(CaptureMsg::Cursor(update)) => {
                        send_cursor_update(&mut legs, &update).await?;
                    }
                    other => {
                        // The helper's frames channel closes when the helper
                        // self-exits on the desktop switching back to Default —
                        // that EOF usually reaches us BEFORE the watcher's
                        // `active=false` transition. It is not a fatal capture
                        // end: park the receiver and let the secure branch
                        // restart the console capture.
                        if other.is_none() && ctl.secure_input.is_some() {
                            info!("secure helper stream ended; awaiting the desktop transition");
                            frame_rx = None;
                        } else {
                            bail!("capture thread ended ({other:?})");
                        }
                    }
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
                last_recv = Instant::now();
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
                last_recv = Instant::now();
                // Probes also arrive on the stream leg (TCP fallback
                // keepalive, §18.39): answer them here too.
                if ch == channel::CONTROL
                    && let Ok(probe) = Probe::decode(&ctrl_buf[..n])
                {
                    let now = micros_since(clock_start);
                    let ack = ProbeAck { t1: probe.t1, t2: now, t3: now };
                    legs.send_control(&ack.encode()).await?;
                } else if ch == channel::AUDIO_IN {
                    forward_mic(&mut mic_tx, &ctrl_buf[..n]);
                } else if ch == channel::CLIPBOARD {
                    handle_clipboard_packet(&mut legs, &mut clip_session, clipboard.as_ref(), &ctrl_buf[..n]).await;
                } else if ch == channel::STORAGE {
                    handle_storage_packet(&mut legs, &mut spool, &ctrl_buf[..n]).await;
                } else if ch == channel::SMARTCARD {
                    scard_router.handle_packet(&ctrl_buf[..n]);
                } else {
                    handle_stream_packet(&mut ctl, &mut encoder, ch, &ctrl_buf[..n]);
                }
            }
            _ = storage_tick.tick(), if spool.as_ref().is_some_and(server::storage::Spool::has_pending) => {
                if let Some(s) = spool.as_mut() {
                    for msg in s.pump(4) {
                        if let Err(e) = legs.stream.send(channel::STORAGE, &msg.encode()).await {
                            warn!("storage chunk send failed: {e}");
                        }
                    }
                }
            }
            _ = liveness_tick.tick() => {
                if last_recv.elapsed() >= CLIENT_LIVENESS_TIMEOUT {
                    bail!("client silent for {CLIENT_LIVENESS_TIMEOUT:?}; session dead");
                }
                // Poison-mode revert (§18.37): a resize that never delivers
                // a frame (the display stack invalidates every duplication
                // instantly) rolls back to the original console mode instead
                // of leaving the session blind until the storm breaker. The
                // failed mode is blacklisted so a queued request does not
                // re-apply it in a loop. Capture is restarted around the
                // restore too: the mode change must never happen with a
                // live duplication (see the resize path below).
                if let Some(since) = resize_pending
                    && since.elapsed() >= RESIZE_FRAME_TIMEOUT
                {
                    warn!(
                        "no frames {RESIZE_FRAME_TIMEOUT:?} after the resize; reverting the console mode"
                    );
                    if let Some(mode) = display_mode.applied() {
                        bad_modes.push(mode);
                    }
                    stop_capture(&mut frame_rx, &mut frame_handle, &mut capture_stop);
                    display_mode.restore();
                    start_capture(
                        args,
                        &fps_shared,
                        clock_start,
                        &mut frame_rx,
                        &mut frame_handle,
                        &mut capture_stop,
                    );
                    resize_pending = None;
                }
            }
            _ = resize_tick.tick(), if ctl.resize_request.is_some() => {
                // Mid-session resize (§18.37): re-apply the resolution
                // policy with the client's new preference. The mode change
                // invalidates the DXGI duplication — and on the Basic
                // Render Driver stack a mode change with a LIVE duplication
                // poisons every future duplication of this process
                // (permanent access-lost storm). So the capture thread is
                // stopped (releasing the duplication) BEFORE the mode is
                // applied and a fresh one is spawned right after: same
                // condition as session start, which works. The new thread
                // reports Ready with the new geometry; the encoder rebuild
                // and re-announce happen in the capture branch.
                // Mode changes are serialized: while a previous change has
                // not delivered a frame, new requests wait for later ticks
                // (a window drag would otherwise stack mode changes on top
                // of a duplication still in transition).
                if let Some(request) = ctl.resize_request.take() {
                    // Mode changes are impossible while a secure desktop owns
                    // the console (§18.41): drop the request.
                    if ctl.secure_input.is_some() {
                        warn!(?request, "resize ignored while a secure desktop is active");
                    } else if resize_pending.is_some() {
                        ctl.resize_request = Some(request);
                    } else {
                        let policy = server::display_mode::ResolutionPolicy {
                            fixed: args.fixed_resolution,
                            preferred: request,
                        };
                        match display_mode.plan(&policy) {
                            // No change needed (no aspect match, or already
                            // at the target mode): leave the console alone.
                            None => {}
                            Some(target) if bad_modes.contains(&target) => {
                                warn!(?target, "resize targeted a blacklisted mode; ignored");
                            }
                            Some(_) => {
                                stop_capture(&mut frame_rx, &mut frame_handle, &mut capture_stop);
                                if display_mode.apply(&policy) {
                                    resize_pending = Some(Instant::now());
                                }
                                start_capture(
                                    args,
                                    &fps_shared,
                                    clock_start,
                                    &mut frame_rx,
                                    &mut frame_handle,
                                    &mut capture_stop,
                                );
                            }
                        }
                    }
                }
            }
            active = recv_secure(&mut secure_rx) => {
                // Secure-desktop transition (§18.41). Entering: stop the DXGI
                // capture (it is denied anyway) and switch frame_rx to the
                // helper pipes; the helper's first frame sends Ready, which
                // rebuilds the encoder and re-announces the display. Leaving:
                // back to a fresh console capture (same Ready path).
                if active && secure_state.is_none() {
                    info!("secure desktop active; switching capture/input to the helper");
                    legs.send_control(&ControlMessage::SecureDesktop { active: true }.encode()?).await?;
                    stop_capture(&mut frame_rx, &mut frame_handle, &mut capture_stop);
                    let session_id = server::secure_pipe::own_session_id().unwrap_or(0);
                    let (rx, state) = spawn_secure_pipes(session_id, clock_start);
                    frame_rx = Some(rx);
                    ctl.secure_input = Some(std::sync::Arc::clone(&state.input));
                    secure_state = Some(state);
                } else if !active && secure_state.is_some() {
                    info!("secure desktop released; resuming console capture");
                    ctl.secure_input = None;
                    secure_state = None;
                    stop_capture(&mut frame_rx, &mut frame_handle, &mut capture_stop);
                    start_capture(
                        args,
                        &fps_shared,
                        clock_start,
                        &mut frame_rx,
                        &mut frame_handle,
                        &mut capture_stop,
                    );
                    legs.send_control(&ControlMessage::SecureDesktop { active: false }.encode()?).await?;
                }
            }
            msg = scard_rx.recv() => {
                // Smartcard requests produced by the relay origin (§17);
                // pends forever while the channel is inert.
                if let Some(msg) = msg
                    && let Err(e) = legs.stream.send(channel::SMARTCARD, &msg.encode()).await
                {
                    warn!("smartcard request send failed: {e}");
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

/// Handles one packet from the storage channel (M8): decodes the message,
/// applies it to the spool and sends back the immediate responses (chunk
/// streams are produced by the session loop's pump tick).
async fn handle_storage_packet<T: ReliableTransport>(
    legs: &mut Legs<T>,
    spool: &mut Option<server::storage::Spool>,
    buf: &[u8],
) {
    let msg = match protocol::StorageMessage::decode(buf) {
        Ok(m) => m,
        Err(e) => {
            warn!("bad storage message: {e}");
            return;
        }
    };
    let responses = if let Some(s) = spool {
        s.handle(&msg)
    } else {
        let id = match &msg {
            protocol::StorageMessage::PutStart { transfer_id, .. }
            | protocol::StorageMessage::GetStart { transfer_id, .. } => *transfer_id,
            _ => 0,
        };
        vec![protocol::StorageMessage::Error {
            transfer_id: id,
            code: protocol::ERR_IO,
            message: "storage unavailable on the server".into(),
        }]
    };
    for resp in responses {
        if let Err(e) = legs.stream.send(channel::STORAGE, &resp.encode()).await {
            warn!("storage response send failed: {e}");
        }
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
    /// SYSTEM injector pipe (§18.42): when filled, normal input is forwarded
    /// to the SYSTEM injector process instead of `injector` so elevated
    /// windows receive it (UIPI focus trap, lab-confirmed).
    injector_input: Option<InjectorSlot>,
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
    /// Capture-fps knob shared with the capture thread.
    fps_shared: Arc<AtomicU32>,
    /// Latest quality-controller fps target (a ceiling for the adapter).
    quality_fps: u32,
    /// Adaptive-fps controller (§12.7); `None` when not configured.
    fps_adapter: Option<server::fps::FpsAdapter>,
    /// Last link-quality report (for the telemetry CSV, §12.8).
    last_metrics: Option<(u64, u64, u16)>,
    /// Pending mid-session desktop resize requested by the client
    /// (`DesktopResize`, §18.37); applied by the liveness tick.
    resize_request: Option<(u32, u32)>,
    /// Secure-desktop helper input pipe (§18.41): while `Some`, input is
    /// forwarded to the helper on the Winlogon desktop instead of the local
    /// injector (the inner `Option` is `None` until the helper connects).
    secure_input: Option<std::sync::Arc<std::sync::Mutex<Option<std::fs::File>>>>,
}

impl ControlState {
    /// The fps the capture thread should run at: the quality controller's
    /// target, lowered by the adaptive-fps controller when the encoder
    /// itself is the bottleneck (§12.7).
    fn effective_fps(&self) -> u32 {
        let adaptive = self.fps_adapter.as_ref().map_or(u32::MAX, server::fps::FpsAdapter::fps);
        self.quality_fps.min(adaptive).max(1)
    }

    /// Pushes the effective fps to the capture thread.
    fn apply_fps(&self) {
        self.fps_shared.store(self.effective_fps(), Ordering::Relaxed);
    }

    /// Feeds one frame's encode time into the adaptive-fps controller;
    /// applies and logs the new rate when it changes (§12.7).
    fn observe_encode(&mut self, encode: Duration) {
        let Some(adapter) = self.fps_adapter.as_mut() else {
            return;
        };
        if let Some(fps) = adapter.observe(encode, Instant::now()) {
            let load = adapter.load().unwrap_or(0.0);
            info!(
                fps,
                quality_fps = self.quality_fps,
                load = (load * 100.0).round() / 100.0,
                "adaptive fps changed"
            );
            self.apply_fps();
        }
    }
}

/// Handles one packet from the reliable stream: input events (§10) and
/// control messages (§12).
fn handle_stream_packet(
    ctl: &mut ControlState,
    encoder: &mut ServerEncoder,
    ch: u16,
    buf: &[u8],
) {
    if ch == channel::INPUT {
        // Secure mode (§18.41): forward the raw wire message to the helper
        // on the Winlogon desktop; it sanitizes and injects there.
        if let Some(pipe) = &ctl.secure_input {
            let mut slot = pipe.lock().expect("secure input pipe");
            if let Some(f) = slot.as_mut()
                && let Err(e) = server::secure_pipe::write_msg(f, server::secure_pipe::TAG_INPUT, buf)
            {
                warn!("secure input pipe write failed; dropping helper input: {e}");
                *slot = None;
            }
            return;
        }
        // SYSTEM injector mode (§18.42): forward the raw wire message to
        // the injector on the Default desktop (reaches elevated windows).
        if let Some(pipe) = &ctl.injector_input {
            // Diagnose Win+L delivery: the injector process keeps no log, so
            // the agent logs LWin on its behalf (§18.50).
            if buf.len() == 4 && buf[0] == 1 && buf[1] & 2 != 0 && buf[2..4] == [0, 0x5B] {
                info!(down = buf[1] & 1 != 0, "LWin forwarded to the SYSTEM injector");
            }
            let mut slot = pipe.lock().expect("injector input pipe");
            if slot.is_some() {
                if let Some(f) = slot.as_mut()
                    && let Err(e) =
                        server::secure_pipe::write_msg(f, server::secure_pipe::TAG_INPUT, buf)
                {
                    warn!("injector pipe write failed; falling back to local input: {e}");
                    *slot = None;
                }
                return;
            }
        }
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
            handle_metrics(ctl, encoder, rtt_micros, jitter_micros, loss_permille);
        }
        Ok(ControlMessage::RequestKeyframe { display_id }) => {
            if ctl.last_keyframe_req.is_none_or(|t| t.elapsed() >= Duration::from_millis(500)) {
                info!(display_id, "keyframe requested by client (frame loss)");
                encoder.request_keyframe();
                ctl.last_keyframe_req = Some(Instant::now());
            }
        }
        Ok(ControlMessage::DesktopResize { width, height }) => {
            info!(width, height, "desktop resize requested by client (§18.37)");
            ctl.resize_request = Some((width, height));
        }
        Ok(ControlMessage::LockRequest) => {
            info!("lock requested by client (§18.50)");
            lock_workstation();
        }
        Ok(_) => {} // other control messages: not handled here yet
        Err(e) => warn!("undecodable control message: {e}"),
    }
}

/// Locks the console session on a client `LockRequest` (§18.50): the OS
/// API is the only reliable trigger — synthesized Win+L key events do not
/// lock (lab-confirmed; the shell treats injected hotkeys as untrusted).
#[cfg(all(windows, feature = "windows-capture"))]
fn lock_workstation() {
    // SAFETY: no preconditions; locks the caller's own workstation.
    if let Err(e) = unsafe { windows::Win32::System::Shutdown::LockWorkStation() } {
        warn!("LockWorkStation failed: {e}");
    }
}

/// Non-Windows placeholder (Linux/macOS locking lands with their ports).
#[cfg(not(all(windows, feature = "windows-capture")))]
fn lock_workstation() {
    warn!("lock request ignored: unsupported on this platform");
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

    /// Current ladder regime and smoothed change-ratio for telemetry;
    /// `(None, None)` for plain encoders or builds without the ladder.
    fn ladder_snapshot(&self) -> (Option<codec::Level>, Option<f64>) {
        #[cfg(feature = "ladder")]
        {
            match self {
                Self::Ladder(e) => (Some(e.ladder().level()), e.ladder().ema()),
                Self::Simple(_) => (None, None),
            }
        }
        #[cfg(not(feature = "ladder"))]
        {
            let _ = self;
            (None, None)
        }
    }
}

/// Ladder tuning parsed from the `--ladder` spec: the sensitivity policy
/// plus the VP9 encoder knobs (not part of `LadderPolicy`, which is pure
/// switch logic).
#[cfg(feature = "ladder")]
struct LadderKnobs {
    policy: codec::LadderPolicy,
    vp9_cpu: Option<i32>,
    threads: Option<u32>,
}

/// Resolves the ladder tuning: balanced defaults plus `key=value` overrides
/// from `--ladder` / the service config frame (§8.5 sensitivity tuning).
/// Unknown keys and malformed values are rejected — a typo must not silently
/// tune nothing.
#[cfg(feature = "ladder")]
fn ladder_knobs(spec: &str) -> Result<LadderKnobs> {
    let mut knobs = LadderKnobs {
        policy: codec::LadderPolicy::balanced(),
        vp9_cpu: None,
        threads: None,
    };
    let policy = &mut knobs.policy;
    for pair in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (key, value) = pair
            .split_once('=')
            .with_context(|| format!("bad ladder override (expected key=value): {pair}"))?;
        match key.trim() {
            "up" => policy.up_threshold_pct = value.parse().context("bad up threshold")?,
            "down" => policy.down_threshold_pct = value.parse().context("bad down threshold")?,
            "hold_up_ms" => {
                policy.hold_up = Duration::from_millis(value.parse().context("bad hold_up_ms")?);
            }
            "hold_down_ms" => {
                policy.hold_down =
                    Duration::from_millis(value.parse().context("bad hold_down_ms")?);
            }
            "min_dwell_ms" => {
                policy.min_dwell =
                    Duration::from_millis(value.parse().context("bad min_dwell_ms")?);
            }
            "backoff" => policy.backoff = value.parse().context("bad backoff")?,
            "dwell_max_ms" => {
                policy.dwell_max =
                    Duration::from_millis(value.parse().context("bad dwell_max_ms")?);
            }
            "ema_alpha" => policy.ema_alpha = value.parse().context("bad ema_alpha")?,
            "vp9_cpu" => knobs.vp9_cpu = Some(value.parse().context("bad vp9_cpu")?),
            "threads" => knobs.threads = Some(value.parse().context("bad threads")?),
            other => bail!("unknown ladder override: {other}"),
        }
    }
    anyhow::ensure!(
        policy.down_threshold_pct < policy.up_threshold_pct,
        "ladder down threshold must be below the up threshold"
    );
    anyhow::ensure!(
        (0.0..=1.0).contains(&policy.ema_alpha) && policy.ema_alpha > 0.0,
        "ladder ema_alpha must be in (0, 1]"
    );
    Ok(knobs)
}

/// Builds the display encoder from `--preset`: a named preset, or the codec
/// ladder (`--preset ladder`, feature `ladder`) which starts in rects-JPEG
/// and switches to VP9 on sustained video-like change (§8.5).
fn build_encoder(args: &Args, width: u32, height: u32) -> Result<ServerEncoder> {
    #[cfg(feature = "ladder")]
    if args.preset == "ladder" {
        let kbps = args.bitrate.unwrap_or(codec::DEFAULT_BITRATE_KBPS);
        let knobs = ladder_knobs(&args.ladder)?;
        let encoder = match (knobs.vp9_cpu, knobs.threads) {
            (None, None) => codec::LadderEncoder::new(width, height, knobs.policy, kbps, 80)?,
            (cpu, threads) => codec::LadderEncoder::with_tuning(
                width,
                height,
                knobs.policy,
                kbps,
                80,
                cpu.unwrap_or(8),
                threads.unwrap_or(4),
            )?,
        };
        info!(width, height, kbps, overrides = %args.ladder, "codec ladder ready (rects-jpeg <-> vp9)");
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
        stop: &Arc<AtomicBool>,
    ) -> Result<Self> {
        let _ = selector; // used only by the Windows DXGI arm
        let _ = stop; // used only by the Windows DXGI arm
        match kind {
            "synthetic" => Ok(Self::Synthetic(SyntheticSource::new(width, height))),
            #[cfg(all(windows, feature = "windows-capture"))]
            "dxgi" => {
                let mut source = Box::new(server::dxgi::DxgiSource::new(selector)?);
                source.set_stop(Arc::clone(stop));
                Ok(Self::Dxgi { source, last: None })
            }
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
    // Secure-desktop probe (§18.41): hidden lab mode, synchronous, no
    // `--key` needed — runs before any config parsing.
    #[cfg(all(windows, feature = "windows-capture"))]
    if let Some(code) = server::secure_probe::maybe_run_from_argv() {
        return code;
    }
    // Secure-desktop helper (§18.41): spawned by the service as SYSTEM on
    // the Winlogon desktop while a secure desktop owns the console.
    #[cfg(all(windows, feature = "windows-capture"))]
    if let Some(code) = server::secure_helper::maybe_run_from_argv() {
        return code;
    }
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
    #[allow(unused_mut)] // `handoff` is taken only on Windows agent builds
    let mut args = parse_args()?;
    // Orchestrated mode (Windows agent): hold the session mutex so the
    // service can watch us and a second agent cannot start. Manual/dev runs
    // without a peer agent are unaffected.
    #[cfg(all(windows, feature = "windows-capture"))]
    let _session_mutex = server::session_mutex::acquire()?;
    let ticket: [u8; protocol::TICKET_LENGTH] = if let Some(t) = args.ticket {
        t
    } else {
        let mut t = [0u8; protocol::TICKET_LENGTH];
        getrandom::fill(&mut t).map_err(|e| anyhow::anyhow!("ticket generation: {e}"))?;
        t
    };
    println!("ticket: {}", protocol::hex::encode(&ticket));

    // Handoff mode (§18.32): the service already accepted and validated the
    // client connection; we rebuild the socket and serve exactly one
    // session, then exit.
    #[cfg(all(windows, feature = "windows-capture"))]
    if let Some(handoff) = args.handoff.take() {
        let stream_tokio = server::handoff::reconstruct_tcp(&handoff.socket_info)
            .context("rebuilding the handed-off socket")?;
        let keys = crypt::derive_session_keys(&ticket, &args.key)?;
        let hello_bytes: &[u8; protocol::Hello::ENCODED_LEN] = handoff
            .hello
            .as_slice()
            .try_into()
            .context("handoff hello has wrong length")?;
        let hello = protocol::Hello::decode(hello_bytes).context("decoding the handoff hello")?;
        let seqs = transport::StreamSeqs {
            send: handoff.send_seq,
            recv: handoff.recv_seq,
        };
        let stream = EncryptedStream::from_handoff(
            transport::TcpTransport::new(stream_tokio),
            &keys,
            hello,
            seqs,
        );
        info!("serving handed-off connection");
        serve_stream(stream, &ticket, &args.key, &args).await?;
        return Ok(());
    }
    #[cfg(not(all(windows, feature = "windows-capture")))]
    if args.handoff.is_some() {
        anyhow::bail!("socket handoff is only supported on Windows");
    }

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

#[cfg(all(test, feature = "ladder"))]
mod ladder_knobs_tests {
    use super::*;

    #[test]
    fn empty_spec_keeps_defaults() {
        let knobs = ladder_knobs("").unwrap();
        assert_eq!(knobs.vp9_cpu, None);
        assert_eq!(knobs.threads, None);
        let balanced = codec::LadderPolicy::balanced();
        assert_eq!(knobs.policy.up_threshold_pct, balanced.up_threshold_pct);
    }

    #[test]
    fn encoder_knobs_parse() {
        let knobs = ladder_knobs("vp9_cpu=6,threads=2").unwrap();
        assert_eq!(knobs.vp9_cpu, Some(6));
        assert_eq!(knobs.threads, Some(2));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(ladder_knobs("vp9cpu=6").is_err());
        assert!(ladder_knobs("threads=abc").is_err());
    }
}
