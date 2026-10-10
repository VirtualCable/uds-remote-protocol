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
use protocol::{self, ControlMessage, CursorMessage, InputMessage, PointerButton, channel};
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

/// Last cursor state received from the server (§10): the client paints the
/// cursor locally at the pointer position it injects.
struct ClientCursor {
    visible: bool,
    shape: Option<CursorShape>,
}

/// One cursor shape in RGBA (as decoded from the wire).
struct CursorShape {
    width: u16,
    height: u16,
    hot_x: u16,
    hot_y: u16,
    rgba: Vec<u8>,
}

impl ClientCursor {
    fn new() -> Self {
        Self { visible: true, shape: None }
    }

    /// Applies one cursor message from the wire.
    fn apply(&mut self, msg: CursorMessage) {
        match msg {
            CursorMessage::Visibility { visible } => {
                self.visible = visible;
                info!(visible, "cursor visibility");
            }
            CursorMessage::Shape { width, height, hot_x, hot_y, pixels } => {
                info!(width, height, "cursor shape");
                self.shape = Some(CursorShape {
                    width,
                    height,
                    hot_x,
                    hot_y,
                    rgba: pixels,
                });
            }
        }
    }
}

/// Alpha-blends the cursor shape over `pixels` (`0x00RRGGBB`, minifb format)
/// with its top-left at `(x, y)` in frame coordinates (hotspot already
/// subtracted by the caller). Pure: unit-tested without a window.
// Bitmap blits mix signed cursor coords with unsigned frame indices; cursor
// dims are ≤ MAX_CURSOR_DIM and frames ≤ 2^31 px, so the casts cannot lose
// information. Short names follow the x/y pixel-loop convention.
#[allow(
    clippy::many_single_char_names,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss // dy/dx are bounds-checked before indexing
)]
fn overlay_cursor(
    pixels: &mut [u32],
    frame_w: usize,
    frame_h: usize,
    shape: &CursorShape,
    x: i32,
    y: i32,
) {
    let (sw, sh) = (usize::from(shape.width), usize::from(shape.height));
    for sy in 0..sh {
        let dy = y + sy as i32;
        if dy < 0 || dy >= frame_h as i32 {
            continue;
        }
        for sx in 0..sw {
            let dx = x + sx as i32;
            if dx < 0 || dx >= frame_w as i32 {
                continue;
            }
            let p = (sy * sw + sx) * 4;
            let (r, g, b, a) = (
                u32::from(shape.rgba[p]),
                u32::from(shape.rgba[p + 1]),
                u32::from(shape.rgba[p + 2]),
                u32::from(shape.rgba[p + 3]),
            );
            if a == 0 {
                continue;
            }
            let dst = &mut pixels[dy as usize * frame_w + dx as usize];
            let (dr, dg, db) = ((*dst >> 16) & 0xFF, (*dst >> 8) & 0xFF, *dst & 0xFF);
            let blend = |s: u32, d: u32| (s * a + d * (255 - a)) / 255;
            *dst = (blend(r, dr) << 16) | (blend(g, dg) << 8) | blend(b, db);
        }
    }
}

/// Renders decoded frames, either to a minifb window or to the log
/// (headless).
struct Sink {
    window: Option<minifb::Window>,
    frames: u64,
    start: Instant,
    headless: bool,
    pixels: Vec<u32>,
    /// Size of the last decoded frame (for window→display coord mapping).
    frame_size: (u32, u32),
}

impl Sink {
    fn new(headless: bool) -> Self {
        Self {
            window: None,
            frames: 0,
            start: Instant::now(),
            headless,
            pixels: Vec::new(),
            frame_size: (0, 0),
        }
    }

    fn push(&mut self, frame: &codec::DecodedFrame, cursor: &ClientCursor) -> Result<bool> {
        self.frames += 1;
        self.frame_size = (frame.width, frame.height);
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
        // Local cursor rendering (§10): painted at the local pointer
        // position, which is what we inject into the remote session.
        if cursor.visible
            && let Some(shape) = &cursor.shape
            && let Some((mx, my)) = window.get_mouse_pos(minifb::MouseMode::Clamp)
        {
            let (ww, wh) = window.get_size();
            if ww > 0 && wh > 0 {
                #[allow(clippy::cast_precision_loss)] // pixel dims fit in f32
                let (wf, hf, wwf, whf) = (w as f32, h as f32, ww as f32, wh as f32);
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let (fx, fy) = (
                    (mx * wf / wwf) as i32 - i32::from(shape.hot_x),
                    (my * hf / whf) as i32 - i32::from(shape.hot_y),
                );
                overlay_cursor(&mut self.pixels, w, h, shape, fx, fy);
            }
        }
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

    if let Some(leg) = leg {
        return Box::pin(udp_session(leg, stream, clock_start, &args)).await;
    }
    // TCP fallback: everything (control + display) on the reliable stream.
    let mut sink = Sink::new(args.headless);
    let mut reasm = Reassembler::new();
    // One decoder per display (§8.3); created from DisplayInfo
    // announcements, or Raw on first frame for servers that do not
    // announce (§8.4 compat).
    let mut decoders: HashMap<u8, Box<dyn VideoDecoder>> = HashMap::new();
    let mut buf = vec![0u8; 2048];
    let mut recovery = DecodeRecovery::default();
    let mut cursor = ClientCursor::new();
    let mut input = InputPoll::new();
    let mut input_tick = tokio::time::interval(Duration::from_millis(8));
    loop {
        tokio::select! {
            res = stream.recv_into(&mut buf) => {
                let (ch, n) = res?;
                if !handle_packet(ch, &buf[..n], 0, &args, &mut reasm, &mut decoders, &mut sink, &mut recovery, &mut cursor)? {
                    return Ok(());
                }
            }
            _ = input_tick.tick() => {
                for msg in input.poll(&sink) {
                    let (buf, n) = msg.encode();
                    stream.send(channel::INPUT, &buf[..n]).await?;
                }
            }
        }
    }
}

/// UDP display leg session (§4.2): receives display traffic while running
/// continuous probing (§12.1) — one probe per second; answered ones feed
/// the RTT/jitter EWMAs, and once per second a MetricsReport goes to the
/// server over the reliable control stream (§12.2). Unrecoverable frame
/// loss triggers a rate-limited `RequestKeyframe`.
async fn udp_session(
    leg: transport::UdpLeg<transport::UdpTransport>,
    mut stream: EncryptedStream<Box<dyn ReliableTransport>>,
    clock_start: Instant,
    args: &Args,
) -> Result<()> {
    let clock = || micros_since(clock_start);
    let mut leg = leg;
    let mut sink = Sink::new(args.headless);
    let mut reasm = Reassembler::new();
    let mut decoders: HashMap<u8, Box<dyn VideoDecoder>> = HashMap::new();
    let mut buf = vec![0u8; 2048];

    let mut probe_tick = tokio::time::interval(Duration::from_secs(1));
    let mut rtt_ewma: Option<f64> = None;
    let mut jitter_ewma = 0.0_f64;
    // The first ack is delayed by the server's session bring-up (capture
    // thread, encoder init), so it is discarded as warm-up and never feeds
    // the EWMA/baseline.
    let mut warmup_acks = 1u8;
    let mut prev_lost = 0u64;
    let mut prev_received = 0u64;
    // Keyframe requests on unrecoverable frame loss (§12.2): rate-limited
    // to 1 per 500 ms (riesgos §2.1).
    let mut prev_dropped = 0u64;
    let mut last_key_req: Option<Instant> = None;
    let mut recovery = DecodeRecovery::default();
    let mut cursor = ClientCursor::new();
    let mut input = InputPoll::new();
    let mut input_tick = tokio::time::interval(Duration::from_millis(8));
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
                if !handle_packet(ch, &buf[..n], lost, args, &mut reasm, &mut decoders, &mut sink, &mut recovery, &mut cursor)? {
                    return Ok(());
                }
                // Ask for a keyframe when the stream is broken (decode
                // errors) or a frame was evicted incomplete; the rate limit
                // re-asks every 500 ms while the stream stays broken.
                let dropped = reasm.dropped_frames();
                if (dropped > prev_dropped || recovery.awaiting)
                    && last_key_req.is_none_or(|t| t.elapsed() >= Duration::from_millis(500))
                {
                    info!(dropped, awaiting = recovery.awaiting, "stream broken; requesting keyframe");
                    let req = ControlMessage::RequestKeyframe { display_id: 0 };
                    stream.send(channel::CONTROL, &req.encode()?).await?;
                    last_key_req = Some(Instant::now());
                }
                prev_dropped = dropped;
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
                // Rates and micros are non-negative and small; the casts
                // cannot lose sign or meaningful precision.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let report = ControlMessage::MetricsReport {
                    rtt_micros: rtt as u64,
                    jitter_micros: jitter_ewma as u64,
                    loss_permille: (loss_rate * 1000.0) as u16,
                };
                stream.send(channel::CONTROL, &report.encode()?).await?;
            }
            _ = input_tick.tick() => {
                // Input channel (§10): poll the window, send the diffs on
                // the reliable stream.
                for msg in input.poll(&sink) {
                    let (buf, n) = msg.encode();
                    stream.send(channel::INPUT, &buf[..n]).await?;
                }
            }
        }
    }
}

/// Decode-error recovery state (§12.2): when a predictive decoder errors
/// (a lost delta frame breaks the reference chain), the client asks for a
/// keyframe and tolerates decode errors until the stream decodes again
/// instead of dying on the first broken frame.
#[derive(Default)]
struct DecodeRecovery {
    /// True while the stream is known-broken and a keyframe is awaited.
    awaiting: bool,
}

/// Input event capture over the minifb window (§10): polls at ~125 Hz and
/// emits protocol messages for the diffs (moves, buttons, keys).
struct InputPoll {
    last_pos: Option<(i32, i32)>,
    buttons: [bool; 3],
    keys: Vec<minifb::Key>,
}

impl InputPoll {
    fn new() -> Self {
        Self { last_pos: None, buttons: [false; 3], keys: Vec::new() }
    }

    /// Collects the input events since the last poll.
    fn poll(&mut self, sink: &Sink) -> Vec<InputMessage> {
        let mut out = Vec::new();
        let Some(window) = &sink.window else { return out };
        let (fw, fh) = sink.frame_size;
        if fw == 0 || fh == 0 {
            return out;
        }
        // Pointer position, mapped from window to display-local pixels.
        if let Some((mx, my)) = window.get_mouse_pos(minifb::MouseMode::Clamp) {
            let (ww, wh) = window.get_size();
            // Screen sizes are tiny; f32 precision is more than enough here.
            #[allow(clippy::cast_precision_loss)]
            let (fwf, wwf, fhf, whf) = (fw as f32, ww.max(1) as f32, fh as f32, wh.max(1) as f32);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let pos = ((mx * fwf / wwf) as i32, (my * fhf / whf) as i32);
            if self.last_pos != Some(pos) {
                self.last_pos = Some(pos);
                out.push(InputMessage::PointerMove { display_id: 0, x: pos.0, y: pos.1 });
            }
        }
        // Buttons (minifb exposes left/right/middle only).
        for (i, btn) in [
            minifb::MouseButton::Left,
            minifb::MouseButton::Right,
            minifb::MouseButton::Middle,
        ]
        .iter()
        .enumerate()
        {
            let down = window.get_mouse_down(*btn);
            if down != self.buttons[i] {
                self.buttons[i] = down;
                let button = match i {
                    0 => PointerButton::Left,
                    1 => PointerButton::Right,
                    _ => PointerButton::Middle,
                };
                out.push(InputMessage::PointerButton { button, down });
            }
        }
        // Keys: press/release diffs of the currently-pressed set.
        let pressed = window.get_keys();
        for key in &pressed {
            if !self.keys.contains(key)
                && let Some((scancode, extended)) = set1_scancode(*key)
            {
                out.push(InputMessage::Key { down: true, scancode, extended });
            }
        }
        for key in &self.keys {
            if !pressed.contains(key)
                && let Some((scancode, extended)) = set1_scancode(*key)
            {
                out.push(InputMessage::Key { down: false, scancode, extended });
            }
        }
        self.keys = pressed;
        out
    }
}

/// Maps a minifb key to a Set 1 scancode + extended (`0xE0`) flag (§10).
/// Unknown/unmapped keys return `None` (ignored by the PoC client).
// A flat key table is the readable form; the line count is the table.
#[allow(clippy::too_many_lines)]
fn set1_scancode(key: minifb::Key) -> Option<(u16, bool)> {
    use minifb::Key;
    let (code, ext) = match key {
        Key::Escape => (0x01, false),
        Key::Key1 => (0x02, false),
        Key::Key2 => (0x03, false),
        Key::Key3 => (0x04, false),
        Key::Key4 => (0x05, false),
        Key::Key5 => (0x06, false),
        Key::Key6 => (0x07, false),
        Key::Key7 => (0x08, false),
        Key::Key8 => (0x09, false),
        Key::Key9 => (0x0A, false),
        Key::Key0 => (0x0B, false),
        Key::Minus => (0x0C, false),
        Key::Equal => (0x0D, false),
        Key::Backspace => (0x0E, false),
        Key::Tab => (0x0F, false),
        Key::Q => (0x10, false),
        Key::W => (0x11, false),
        Key::E => (0x12, false),
        Key::R => (0x13, false),
        Key::T => (0x14, false),
        Key::Y => (0x15, false),
        Key::U => (0x16, false),
        Key::I => (0x17, false),
        Key::O => (0x18, false),
        Key::P => (0x19, false),
        Key::LeftBracket => (0x1A, false),
        Key::RightBracket => (0x1B, false),
        Key::Enter => (0x1C, false),
        Key::LeftCtrl => (0x1D, false),
        Key::A => (0x1E, false),
        Key::S => (0x1F, false),
        Key::D => (0x20, false),
        Key::F => (0x21, false),
        Key::G => (0x22, false),
        Key::H => (0x23, false),
        Key::J => (0x24, false),
        Key::K => (0x25, false),
        Key::L => (0x26, false),
        Key::Semicolon => (0x27, false),
        Key::Apostrophe => (0x28, false),
        Key::Backquote => (0x29, false),
        Key::LeftShift => (0x2A, false),
        Key::Backslash => (0x2B, false),
        Key::Z => (0x2C, false),
        Key::X => (0x2D, false),
        Key::C => (0x2E, false),
        Key::V => (0x2F, false),
        Key::B => (0x30, false),
        Key::N => (0x31, false),
        Key::M => (0x32, false),
        Key::Comma => (0x33, false),
        Key::Period => (0x34, false),
        Key::Slash => (0x35, false),
        Key::RightShift => (0x36, false),
        Key::NumPadAsterisk => (0x37, false),
        Key::LeftAlt => (0x38, false),
        Key::Space => (0x39, false),
        Key::CapsLock => (0x3A, false),
        Key::F1 => (0x3B, false),
        Key::F2 => (0x3C, false),
        Key::F3 => (0x3D, false),
        Key::F4 => (0x3E, false),
        Key::F5 => (0x3F, false),
        Key::F6 => (0x40, false),
        Key::F7 => (0x41, false),
        Key::F8 => (0x42, false),
        Key::F9 => (0x43, false),
        Key::F10 => (0x44, false),
        Key::NumLock => (0x45, false),
        Key::ScrollLock => (0x46, false),
        Key::NumPad7 => (0x47, false),
        Key::NumPad8 => (0x48, false),
        Key::NumPad9 => (0x49, false),
        Key::NumPadMinus => (0x4A, false),
        Key::NumPad4 => (0x4B, false),
        Key::NumPad5 => (0x4C, false),
        Key::NumPad6 => (0x4D, false),
        Key::NumPadPlus => (0x4E, false),
        Key::NumPad1 => (0x4F, false),
        Key::NumPad2 => (0x50, false),
        Key::NumPad3 => (0x51, false),
        Key::NumPad0 => (0x52, false),
        Key::NumPadDot => (0x53, false),
        Key::F11 => (0x57, false),
        Key::F12 => (0x58, false),
        // Extended keys (0xE0 prefix).
        Key::NumPadEnter => (0x1C, true),
        Key::RightCtrl => (0x1D, true),
        Key::NumPadSlash => (0x35, true),
        Key::RightAlt => (0x38, true),
        Key::Home => (0x47, true),
        Key::Up => (0x48, true),
        Key::PageUp => (0x49, true),
        Key::Left => (0x4B, true),
        Key::Right => (0x4D, true),
        Key::End => (0x4F, true),
        Key::Down => (0x50, true),
        Key::PageDown => (0x51, true),
        Key::Insert => (0x52, true),
        Key::Delete => (0x53, true),
        Key::LeftSuper => (0x5B, true),
        Key::RightSuper => (0x5C, true),
        _ => return None,
    };
    Some((code, ext))
}

/// Handles one received packet; returns `false` when the session should end.
#[allow(clippy::too_many_arguments)]
fn handle_packet(
    ch: u16,
    packet: &[u8],
    lost: u64,
    args: &Args,
    reasm: &mut Reassembler,
    decoders: &mut HashMap<u8, Box<dyn VideoDecoder>>,
    sink: &mut Sink,
    recovery: &mut DecodeRecovery,
    cursor: &mut ClientCursor,
) -> Result<bool> {
    if ch == channel::CURSOR {
        match CursorMessage::decode(packet) {
            Ok(msg) => cursor.apply(msg),
            Err(e) => warn!("bad cursor message: {e}"),
        }
        return Ok(true);
    }
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
        match decoder.decode(&codec::EncodedPacket {
            data: complete.data,
            keyframe: true,
        }) {
            Ok(frame) => {
                recovery.awaiting = false;
                // Only display 0 is rendered (multi-monitor arrives later,
                // §8.3); other displays are still decoded to exercise the
                // pipeline.
                if complete.display_id == 0 && !sink.push(&frame, cursor)? {
                    info!("window closed");
                    return Ok(false);
                }
            }
            Err(e) => {
                // A lost fragment breaks the predictive chain: tolerate the
                // error and let the caller ask for a keyframe (§12.2).
                if !recovery.awaiting {
                    warn!("decode failed ({e}); awaiting keyframe");
                }
                recovery.awaiting = true;
            }
        }
        if args.max_frames.is_some_and(|max| sink.frames >= max) {
            info!(frames = sink.frames, lost, "done");
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape_1px(r: u8, g: u8, b: u8, a: u8) -> CursorShape {
        CursorShape { width: 1, height: 1, hot_x: 0, hot_y: 0, rgba: vec![r, g, b, a] }
    }

    #[test]
    fn overlay_blends_over_background() {
        let mut px = vec![0x0000_0000u32]; // black 1x1
        overlay_cursor(&mut px, 1, 1, &shape_1px(255, 0, 0, 128), 0, 0);
        // ~50% red over black
        assert_eq!(px[0], (128 << 16) | 0);
    }

    #[test]
    fn overlay_opaque_replaces() {
        let mut px = vec![0x0012_3456u32];
        overlay_cursor(&mut px, 1, 1, &shape_1px(255, 255, 255, 255), 0, 0);
        assert_eq!(px[0], 0x00FF_FFFF);
    }

    #[test]
    fn overlay_transparent_keeps() {
        let mut px = vec![0x0012_3456u32];
        overlay_cursor(&mut px, 1, 1, &shape_1px(255, 255, 255, 0), 0, 0);
        assert_eq!(px[0], 0x0012_3456);
    }

    #[test]
    fn overlay_clips_outside_frame() {
        let mut px = vec![0u32; 4]; // 2x2
        overlay_cursor(&mut px, 2, 2, &shape_1px(255, 255, 255, 255), -1, 5);
        assert_eq!(px, vec![0u32; 4]);
    }
}
