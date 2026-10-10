//! Test client for the uds-rp protocol.
//!
//! Minimal development binary to exercise the framework end to end
//! (direct connection, no gateway). Renders the display channel with
//! minifb, or runs `--headless` to validate the pipeline without a GUI.
//!
//! Usage: `testclient --key <hex64> --ticket <hex96> [--addr host:port]
//!                     [--headless] [--max-frames N]`

use std::collections::HashMap;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use codec::{VideoCodecType, VideoDecoder};
use protocol::fragment::Reassembler;
use protocol::{self, ControlMessage, channel};
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
    // One decoder per display (§8.3); created from DisplayInfo announcements,
    // or Raw on first frame for servers that do not announce (§8.4 compat).
    let mut decoders: HashMap<u8, Box<dyn VideoDecoder>> = HashMap::new();
    let mut buf = vec![0u8; 2048];

    match leg {
        Some(mut leg) => {
            // Continuous probing (§12.1): one probe per second over the UDP
            // leg; answered ones feed the RTT/jitter EWMAs, and once per
            // second a MetricsReport goes to the server over the reliable
            // control stream (§12.2).
            let mut probe_tick = tokio::time::interval(Duration::from_secs(1));
            let mut rtt_ewma: Option<f64> = None;
            let mut jitter_ewma = 0.0_f64;
            // The first ack is delayed by the server's session bring-up
            // (capture thread, encoder init), so it is discarded as warm-up
            // and never feeds the EWMA/baseline.
            let mut warmup_acks = 1u8;
            let mut prev_lost = 0u64;
            let mut prev_received = 0u64;
            loop {
                tokio::select! {
                    res = leg.dgram.recv_into(&mut buf) => {
                        let (ch, n) = res?;
                        if ch == channel::CONTROL
                            && let Ok(ack) = protocol::ProbeAck::decode(&buf[..n])
                        {
                            let rtt = protocol::compute_rtt(ack.t1, ack.t2, ack.t3, clock());
                            #[allow(clippy::cast_precision_loss)] // micros fit in f64
                            let rtt = rtt as f64;
                            if warmup_acks > 0 {
                                warmup_acks -= 1;
                                continue;
                            }
                            let prev = rtt_ewma.unwrap_or(rtt);
                            jitter_ewma += 0.25 * ((rtt - prev).abs() - jitter_ewma);
                            rtt_ewma = Some(prev + 0.25 * (rtt - prev));
                            continue;
                        }
                        let lost = leg.dgram.lost_estimate();
                        if !handle_packet(ch, &buf[..n], lost, &args, &mut reasm, &mut decoders, &mut sink)? {
                            return Ok(());
                        }
                    }
                    _ = probe_tick.tick() => {
                        leg.dgram
                            .send(channel::CONTROL, &protocol::Probe { t1: clock() }.encode())
                            .await?;
                        let Some(rtt) = rtt_ewma else { continue };
                        let lost = leg.dgram.lost_estimate();
                        let received = leg.dgram.received();
                        let dlost = lost - prev_lost;
                        let drecv = received - prev_received;
                        prev_lost = lost;
                        prev_received = received;
                        let denom = dlost + drecv;
                        #[allow(clippy::cast_precision_loss)] // counters fit in f64
                        let loss_rate = if denom == 0 { 0.0 } else { dlost as f64 / denom as f64 };
                        // Rates and micros are non-negative and small; the
                        // casts cannot lose sign or meaningful precision.
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let report = ControlMessage::MetricsReport {
                            rtt_micros: rtt as u64,
                            jitter_micros: jitter_ewma as u64,
                            loss_permille: (loss_rate * 1000.0) as u16,
                        };
                        stream.send(channel::CONTROL, &report.encode()?).await?;
                    }
                }
            }
        }
        None => loop {
            let (ch, n) = stream.recv_into(&mut buf).await?;
            if !handle_packet(ch, &buf[..n], 0, &args, &mut reasm, &mut decoders, &mut sink)? {
                return Ok(());
            }
        },
    }
}

/// Handles one received packet; returns `false` when the session should end.
fn handle_packet(
    ch: u16,
    packet: &[u8],
    lost: u64,
    args: &Args,
    reasm: &mut Reassembler,
    decoders: &mut HashMap<u8, Box<dyn VideoDecoder>>,
    sink: &mut Sink,
) -> Result<bool> {
    if ch == channel::CONTROL
        && let Ok(ControlMessage::DisplayInfo {
            display_id,
            width,
            height,
            codec,
        }) = ControlMessage::decode(packet)
    {
        match VideoCodecType::from_wire_id(codec)
            .map(codec::build_decoder)
            .transpose()
        {
            Ok(Some(dec)) => {
                info!(display_id, width, height, codec, "display announced");
                decoders.insert(display_id, dec);
            }
            Ok(None) => warn!(codec, "unknown codec id; frames will be dropped"),
            Err(e) => warn!(codec, "cannot build decoder: {e}"),
        }
    }
    if ch == channel::DISPLAY
        && let Some(complete) = reasm.push(packet)?
    {
        let decoder = match decoders.entry(complete.display_id) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                if complete.display_id == 0 {
                    warn!("no DisplayInfo received; assuming raw codec");
                    e.insert(codec::build_decoder(VideoCodecType::Raw)?)
                } else {
                    warn!(
                        display_id = complete.display_id,
                        "frame for unknown display; dropped"
                    );
                    return Ok(true);
                }
            }
        };
        let frame = decoder.decode(&codec::EncodedPacket {
            data: complete.data,
            keyframe: true,
        })?;
        // Only display 0 is rendered (multi-monitor arrives later, §8.3);
        // other displays are still decoded to exercise the pipeline.
        if complete.display_id == 0 && !sink.push(&frame)? {
            info!("window closed");
            return Ok(false);
        }
        if args.max_frames.is_some_and(|max| sink.frames >= max) {
            info!(frames = sink.frames, lost, "done");
            return Ok(false);
        }
    }
    Ok(true)
}
