//! Development server: serves a synthetic moving test pattern over the
//! protocol (phases 0-2, before real screen capture lands).
//!
//! Usage: `server --key <hex64> [--ticket <hex96>] [--bind host:port]
//!                 [--width N] [--height N] [--fps N] [--max-frames N]`
//!
//! The session ticket is printed on startup when not given; pass it to the
//! test client with `--ticket`.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use codec::{RawEncoder, VideoEncoder};
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
    let mut source = SyntheticSource::new(args.width, args.height);
    let mut encoder = RawEncoder::new();
    let frame_time = Duration::from_secs_f64(1.0 / f64::from(args.fps.max(1)));
    let mut interval = tokio::time::interval(frame_time);
    let mut frame_id = 0u32;
    loop {
        interval.tick().await;
        let frame = source.next_frame();
        let packet = encoder.encode(&frame)?;
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
