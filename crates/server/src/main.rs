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
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use codec::CodecPreset;
use crypt::consts;
use protocol::fragment::Fragmenter;
use protocol::{self, Probe, ProbeAck, channel};
use server::synthetic::SyntheticSource;
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

/// Display leg chosen after UDP detection: the lossy UDP channel, or the
/// reliable encrypted stream as fallback (§4.2).
enum Display<T: ReliableTransport> {
    Udp(Box<EncryptedDatagram<UdpTransport>>),
    Tcp(Box<EncryptedStream<T>>),
}

impl<T: ReliableTransport> Display<T> {
    async fn send_fragment(&mut self, frag: &[u8]) -> transport::Result<()> {
        match self {
            Self::Udp(d) => d.send(channel::DISPLAY, frag).await,
            Self::Tcp(s) => s.send(channel::DISPLAY, frag).await,
        }
    }

    async fn send_control(&mut self, msg: &[u8]) -> transport::Result<()> {
        match self {
            Self::Udp(d) => d.send(channel::CONTROL, msg).await,
            Self::Tcp(s) => s.send(channel::CONTROL, msg).await,
        }
    }
}

fn micros_since(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
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
    let display = match tokio::time::timeout(Duration::from_secs(3), pending.wait_first()).await {
        Ok(Ok((udp, first))) => {
            let keys = crypt::derive_datagram_keys(ticket, key)?;
            let mut dgram = EncryptedDatagram::new(udp, &keys.s2c, &keys.c2s, &token);
            answer_probes(&mut dgram, &first, clock_start).await?;
            info!("display leg: UDP");
            Display::Udp(Box::new(dgram))
        }
        other => {
            if let Ok(Err(e)) = other {
                warn!("UDP accept failed: {e}");
            } else {
                info!("UDP detection timed out");
            }
            info!("display leg: TCP fallback");
            Display::Tcp(Box::new(stream))
        }
    };

    let mut display = display;
    let mut source = Source::new(args)?;
    let (width, height) = source.dimensions();
    let preset = CodecPreset::by_name(&args.preset)
        .with_context(|| format!("unknown preset: {}", args.preset))?;
    let preset = match args.bitrate {
        Some(kbps) => preset.with_bitrate(kbps),
        None => preset,
    };
    let mut encoder = preset.build_encoder(width, height)?;
    info!(preset = preset.name, codec = ?preset.codec, width, height, "display encoder ready");

    // Announce the display codec/geometry before the first frame (§8.3/§8.4).
    let info = protocol::ControlMessage::DisplayInfo {
        display_id: 0,
        width,
        height,
        codec: preset.codec.wire_id(),
    };
    display.send_control(&info.encode()?).await?;

    let frame_time = Duration::from_secs_f64(1.0 / f64::from(args.fps.max(1)));
    let mut interval = tokio::time::interval(frame_time);
    let mut frame_id = 0u32;
    loop {
        interval.tick().await;
        let Some(frame) = tokio::task::block_in_place(|| source.next_frame(frame_time))? else {
            continue; // no desktop changes this tick
        };
        let packet = encoder.encode(&frame)?;
        if packet.data.is_empty() {
            continue; // rect codecs: nothing changed on this frame
        }
        for frag in Fragmenter::split(0, frame_id, &packet.data) {
            display.send_fragment(&frag).await?;
        }
        frame_id = frame_id.wrapping_add(1);
        if args.max_frames.is_some_and(|max| frame_id >= max) {
            info!(frame_id, "max frames reached, closing session");
            return Ok(());
        }
    }
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
    fn new(args: &Args) -> Result<Self> {
        match args.source.as_str() {
            "synthetic" => Ok(Self::Synthetic(SyntheticSource::new(args.width, args.height))),
            #[cfg(all(windows, feature = "windows-capture"))]
            "dxgi" => Ok(Self::Dxgi {
                source: Box::new(server::dxgi::DxgiSource::new(args.output)?),
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
