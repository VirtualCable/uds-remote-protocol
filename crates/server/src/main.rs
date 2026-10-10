//! Development server: serves a synthetic moving test pattern or a real
//! desktop capture (DXGI, Windows) over the protocol.
//!
//! Usage: `server --key <hex64> [--ticket <hex96>] [--bind host:port]
//!                 [--source synthetic|dxgi] [--output N]
//!                 [--preset NAME] [--bitrate KBPS]
//!                 [--width N] [--height N] [--fps N] [--max-frames N]`
//!
//! `--preset` is a name from `codec::PRESETS` (e.g. `raw`, `vp9-balanced`,
//! `vp8-low-latency`); VP8/VP9 presets require the crate features
//! `vp9`/`vp8`. The chosen codec/geometry is announced on the control
//! channel (`DisplayInfo`) before the first frame.
//!
//! The session ticket is printed on startup when not given; pass it to the
//! test client with `--ticket`.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use codec::{CodecPreset, VideoEncoder as _};
use crypt::consts;
use protocol::fragment::Fragmenter;
use protocol::{self, ControlMessage, CursorMessage, Probe, ProbeAck, channel};
use server::synthetic::SyntheticSource;
use tokio::sync::mpsc;
use transport::{
    DirectListener, EncryptedDatagram, EncryptedStream, ReliableTransport, UdpTransport,
    udp_offer,
};
use tracing::{info, warn};

struct Args {
    bind: SocketAddr,
    key: [u8; consts::KEY_LENGTH],
    ticket: Option<[u8; protocol::TICKET_LENGTH]>,
    source: String,
    output: u32,
    preset: String,
    bitrate: Option<u32>,
    width: u32,
    height: u32,
    fps: u32,
    max_frames: Option<u32>,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        bind: "127.0.0.1:19790".parse().expect("valid default"),
        key: [0; consts::KEY_LENGTH],
        ticket: None,
        source: "synthetic".to_string(),
        output: 0,
        preset: "raw".to_string(),
        bitrate: None,
        width: 640,
        height: 480,
        fps: 30,
        max_frames: None,
    };
    let mut key_seen = false;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let value = it.next().context(format!("missing value for {arg}"))?;
        match arg.as_str() {
            "--bind" => args.bind = value.parse().context("bad --bind")?,
            "--key" => {
                args.key = protocol::hex::decode(&value)?;
                key_seen = true;
            }
            "--ticket" => args.ticket = Some(protocol::hex::decode(&value)?),
            "--source" => args.source = value,
            "--output" => args.output = value.parse().context("bad --output")?,
            "--preset" => args.preset = value,
            "--bitrate" => args.bitrate = Some(value.parse().context("bad --bitrate")?),
            "--width" => args.width = value.parse().context("bad --width")?,
            "--height" => args.height = value.parse().context("bad --height")?,
            "--fps" => args.fps = value.parse().context("bad --fps")?,
            "--max-frames" => args.max_frames = Some(value.parse().context("bad --max-frames")?),
            other => bail!("unknown argument: {other}"),
        }
    }
    if !key_seen {
        bail!("--key <hex64> is required");
    }
    Ok(args)
}

/// Session legs after UDP detection (§4.2): the reliable control stream is
/// always alive; display traffic goes over the lossy UDP leg when present,
/// or over the stream as fallback.
struct Legs<T: ReliableTransport> {
    udp: Option<Box<EncryptedDatagram<UdpTransport>>>,
    stream: Box<EncryptedStream<T>>,
}

impl<T: ReliableTransport> Legs<T> {
    async fn send_fragment(&mut self, frag: &[u8]) -> transport::Result<()> {
        match &mut self.udp {
            Some(d) => d.send(channel::DISPLAY, frag).await,
            None => self.stream.send(channel::DISPLAY, frag).await,
        }
    }

    async fn send_control(&mut self, msg: &[u8]) -> transport::Result<()> {
        match &mut self.udp {
            Some(d) => d.send(channel::CONTROL, msg).await,
            None => self.stream.send(channel::CONTROL, msg).await,
        }
    }

    /// Cursor updates always take the reliable stream: the client keeps the
    /// last shape as state, so losing one would leave a stale cursor (§11).
    async fn send_cursor(&mut self, msg: &[u8]) -> transport::Result<()> {
        self.stream.send(channel::CURSOR, msg).await
    }
}

/// Receives the next UDP datagram; pends forever when the session has no
/// UDP leg (TCP fallback), so it can sit in a `select!` branch.
async fn recv_udp(
    udp: &mut Option<Box<EncryptedDatagram<UdpTransport>>>,
    buf: &mut [u8],
) -> transport::Result<(channel::ChannelId, usize)> {
    match udp {
        Some(d) => d.recv_into(buf).await,
        None => std::future::pending().await,
    }
}

/// Capture thread → main loop messages.
#[derive(Debug)]
enum CaptureMsg {
    /// First message: the source is up (display geometry) or failed.
    Ready(Result<(u32, u32)>),
    /// One captured frame (owned, so the encoder can borrow from it).
    Frame(OwnedFrame),
    /// Cursor shape/visibility change observed while capturing (§11).
    Cursor(server::capture::CursorUpdate),
}

/// Owned frame data crossing the capture-thread boundary.
#[derive(Debug)]
struct OwnedFrame {
    width: u32,
    height: u32,
    stride: usize,
    data: Vec<u8>,
}

/// Runs the frame source on its own thread so the async loop never blocks
/// behind capture waits (probes and metrics must be handled on time, §12).
///
/// The thread paces itself from `fps_shared` (updated live by the quality
/// controller): DXGI already blocks up to `frame_time` waiting for desktop
/// damage, so only the synthetic source sleeps explicitly.
fn spawn_capture(args: &Args, fps_shared: Arc<AtomicU32>) -> mpsc::Receiver<CaptureMsg> {
    let kind = args.source.clone();
    let (width, height, output) = (args.width, args.height, args.output);
    let (tx, rx) = mpsc::channel(2);
    std::thread::spawn(move || {
        let mut source = match Source::new(&kind, output, width, height) {
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
            let fps = fps_shared.load(Ordering::Relaxed).max(1);
            let frame_time = Duration::from_secs_f64(1.0 / f64::from(fps));
            match source.next_frame(frame_time) {
                Ok(Some(f)) => {
                    let owned = OwnedFrame {
                        width: f.width,
                        height: f.height,
                        stride: f.stride,
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

/// Audio channel is not wired yet (§9); nominal Opus ceiling for quality
/// targets (§12.5).
const AUDIO_CEILING_KBPS: u32 = 64;

/// Feeds one link-quality report into the quality controller and, when the
/// level changes, applies the new targets: encoder bitrate, capture fps and
/// the codec-ladder regime force (§12.5).
#[allow(clippy::too_many_arguments)]
fn handle_metrics(
    quality: &mut Option<protocol::QualityController>,
    forced_video: &mut bool,
    encoder: &mut ServerEncoder,
    fps_shared: &AtomicU32,
    video_ceiling_kbps: u32,
    rtt_micros: u64,
    jitter_micros: u64,
    loss_permille: u16,
) {
    let controller = quality.get_or_insert_with(|| {
        info!(baseline_rtt_micros = rtt_micros, "quality baseline set");
        protocol::QualityController::new(protocol::QualityPolicy::standard(rtt_micros))
    });
    let sample = protocol::Sample {
        rtt_micros,
        jitter_micros,
        loss: f64::from(loss_permille) / 1000.0,
    };
    let Some(event) = controller.update(Instant::now(), sample) else {
        return;
    };
    let t = protocol::targets(
        event.level,
        video_ceiling_kbps,
        AUDIO_CEILING_KBPS,
        controller.policy(),
    );
    info!(
        level = event.level,
        degraded = event.degraded,
        video_kbps = t.video_kbps,
        fps = t.fps,
        "quality level changed"
    );
    if let Err(e) = encoder.set_bitrate(t.video_kbps) {
        warn!("set_bitrate failed: {e}");
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
    for frag in Fragmenter::split(0, frame_id, &packet.data) {
        legs.send_fragment(&frag).await?;
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

async fn serve<T: ReliableTransport>(
    raw: T,
    ticket: &[u8; protocol::TICKET_LENGTH],
    key: &[u8; consts::KEY_LENGTH],
    args: &Args,
) -> Result<()> {
    let mut stream = EncryptedStream::server_handshake(raw, key, None).await?;
    info!("handshake done");

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
            Some(Box::new(dgram))
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
    let mut legs = Legs { udp, stream: Box::new(stream) };

    let fps_shared = Arc::new(AtomicU32::new(args.fps.max(1)));
    let mut frame_rx = spawn_capture(args, Arc::clone(&fps_shared));
    let (width, height) = match frame_rx.recv().await {
        Some(CaptureMsg::Ready(r)) => r?,
        other => bail!("capture thread ended before reporting readiness ({other:?})"),
    };
    let mut encoder = build_encoder(args, width, height)?;

    // Announce the display codec/geometry before the first frame (§8.3/§8.4).
    announce_display(&mut legs, &mut encoder, width, height).await?;

    // Link-quality state (§12): created on the first MetricsReport, with the
    // first reported RTT as the baseline.
    let mut ctl = ControlState {
        quality: None,
        forced_video: false,
        last_keyframe_req: None,
        injector: new_injector(),
        video_ceiling_kbps: args.bitrate.unwrap_or(codec::DEFAULT_BITRATE_KBPS),
    };

    let mut udp_buf = vec![0u8; 2048];
    let mut ctrl_buf = vec![0u8; 2048];
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
            res = recv_udp(&mut legs.udp, &mut udp_buf) => {
                // Continuous probing (§12.1): answer latency probes for the
                // whole session, not only during UDP detection.
                let (ch, n) = res?;
                if ch == channel::CONTROL
                    && let Ok(probe) = Probe::decode(&udp_buf[..n])
                {
                    let now = micros_since(clock_start);
                    let ack = ProbeAck { t1: probe.t1, t2: now, t3: now };
                    legs.send_control(&ack.encode()).await?;
                }
            }
            res = legs.stream.recv_into(&mut ctrl_buf) => {
                let (ch, n) = res?;
                handle_stream_packet(&mut ctl, &mut encoder, &fps_shared, ch, &ctrl_buf[..n]);
            }
        }
    }
}

/// Encodes and sends one cursor update (visibility and/or shape) on the
/// reliable stream (§11).
async fn send_cursor_update<T: ReliableTransport>(
    legs: &mut Legs<T>,
    update: &server::capture::CursorUpdate,
) -> transport::Result<()> {
    if let Some(visible) = update.visible {
        legs.send_cursor(&CursorMessage::Visibility { visible }.encode())
            .await?;
    }
    if let Some(shape) = &update.shape {
        legs.send_cursor(
            &CursorMessage::Shape {
                width: shape.width,
                height: shape.height,
                hot_x: shape.hot_x,
                hot_y: shape.hot_y,
                pixels: shape.rgba.clone(),
            }
            .encode(),
        )
        .await?;
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
                encoder,
                fps_shared,
                ctl.video_ceiling_kbps,
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
    fn new(kind: &str, output: u32, width: u32, height: u32) -> Result<Self> {
        let _ = output; // used only by the Windows DXGI arm
        match kind {
            "synthetic" => Ok(Self::Synthetic(SyntheticSource::new(width, height))),
            #[cfg(all(windows, feature = "windows-capture"))]
            "dxgi" => Ok(Self::Dxgi {
                source: Box::new(server::dxgi::DxgiSource::new(output)?),
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
    tracing_subscriber::fmt::init();
    let args = parse_args()?;
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
