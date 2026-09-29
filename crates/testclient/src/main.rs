//! Test client for the uds-rp protocol.
//!
//! Minimal development binary to exercise the framework end to end
//! (direct connection, no gateway). Renders the display channel with
//! minifb, or runs `--headless` to validate the pipeline without a GUI.
//!
//! Usage: `testclient --key <hex64> --ticket <hex96> [--addr host:port]
//!                     [--headless] [--max-frames N]`

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use codec::{RawDecoder, VideoDecoder};
use protocol::fragment::Reassembler;
use protocol::{self, channel};
use tracing::{info, warn};
use transport::{Connector, DirectConnector, EncryptedStream, ReliableTransport, Target, udp_detect};

struct Args {
    addr: SocketAddr,
    key: [u8; 32],
    ticket: [u8; protocol::TICKET_LENGTH],
    headless: bool,
    max_frames: Option<u64>,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        addr: "127.0.0.1:19790".parse().expect("valid default"),
        key: [0; 32],
        ticket: [0; protocol::TICKET_LENGTH],
        headless: false,
        max_frames: None,
    };
    let mut key_seen = false;
    let mut ticket_seen = false;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        if arg == "--headless" {
            args.headless = true;
            continue;
        }
        let value = it.next().context(format!("missing value for {arg}"))?;
        match arg.as_str() {
            "--addr" => args.addr = value.parse().context("bad --addr")?,
            "--key" => {
                args.key = protocol::hex::decode(&value)?;
                key_seen = true;
            }
            "--ticket" => {
                args.ticket = protocol::hex::decode(&value)?;
                ticket_seen = true;
            }
            "--max-frames" => args.max_frames = Some(value.parse().context("bad --max-frames")?),
            other => bail!("unknown argument: {other}"),
        }
    }
    if !key_seen || !ticket_seen {
        bail!("--key <hex64> and --ticket <hex96> are required");
    }
    Ok(args)
}

fn micros_since(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// Renders decoded frames, either to a minifb window or to the log
/// (headless).
struct Sink {
    window: Option<minifb::Window>,
    frames: u64,
    start: Instant,
    headless: bool,
    pixels: Vec<u32>,
}

impl Sink {
    fn new(headless: bool) -> Self {
        Self {
            window: None,
            frames: 0,
            start: Instant::now(),
            headless,
            pixels: Vec::new(),
        }
    }

    fn push(&mut self, frame: &codec::DecodedFrame) -> Result<bool> {
        self.frames += 1;
        if self.frames % 30 == 1 {
            let elapsed = self.start.elapsed().as_secs_f64();
            #[allow(clippy::cast_precision_loss)] // frame counts are tiny
            let fps = self.frames as f64 / elapsed.max(0.001);
            info!(frames = self.frames, fps, "display");
        }
        if self.headless {
            return Ok(true);
        }
        let (w, h) = (frame.width as usize, frame.height as usize);
        if self.window.is_none() {
            let window = minifb::Window::new(
                "uds-rp testclient",
                w,
                h,
                minifb::WindowOptions::default(),
            )
            .context("window creation")?;
            self.window = Some(window);
        }
        let window = self.window.as_mut().expect("just created");
        self.pixels.clear();
        self.pixels.extend(
            frame
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&[b, g, r, _]| u32::from_le_bytes([b, g, r, 0])),
        );
        window.update_with_buffer(&self.pixels, w, h).context("render")?;
        Ok(window.is_open() && !window.is_key_down(minifb::Key::Escape))
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match Box::pin(run()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("testclient: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = parse_args()?;

    let target = Target::new(args.addr, args.ticket, args.key);
    let (raw, _) = DirectConnector::new().connect(&target).await?;
    let mut stream: EncryptedStream<Box<dyn ReliableTransport>> =
        EncryptedStream::client_handshake(raw, &args.ticket, &args.key).await?;
    info!("handshake done");

    let clock_start = Instant::now();
    let clock = || micros_since(clock_start);
    let leg = udp_detect(
        &mut stream,
        args.addr.ip(),
        &args.ticket,
        &args.key,
        clock,
        3,
        Duration::from_millis(500),
    )
    .await?;
    if let Some(leg) = &leg {
        info!(rtt_micros = leg.initial_rtt_micros, "display leg: UDP");
    } else {
        warn!("UDP detection failed, display leg: TCP fallback");
    }

    let mut sink = Sink::new(args.headless);
    let mut reasm = Reassembler::new();
    let mut decoder = RawDecoder::new();
    let mut buf = vec![0u8; 2048];

    macro_rules! handle {
        ($ch:expr, $n:expr, $lost:expr) => {
            if $ch == channel::DISPLAY
                && let Some(complete) = reasm.push(&buf[..$n])?
            {
                let frame = decoder.decode(&codec::EncodedPacket {
                    data: complete.data,
                    keyframe: true,
                })?;
                if !sink.push(&frame)? {
                    info!("window closed");
                    return Ok(());
                }
                if args.max_frames.is_some_and(|max| sink.frames >= max) {
                    info!(frames = sink.frames, lost = $lost, "done");
                    return Ok(());
                }
            }
        };
    }

    match leg {
        Some(mut leg) => loop {
            let (ch, n) = leg.dgram.recv_into(&mut buf).await?;
            handle!(ch, n, leg.dgram.lost_estimate());
        },
        None => loop {
            let (ch, n) = stream.recv_into(&mut buf).await?;
            handle!(ch, n, 0);
        },
    }
}
