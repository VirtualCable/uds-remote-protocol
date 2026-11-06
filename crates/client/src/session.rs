// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Client session: connection, decoding and the event/command pumps (§15.3).
//!
//! A [`Session`] owns the whole protocol side of a connection — handshake,
//! UDP detection, reassembly, decoding, A/V presentation sync, audio and
//! mic pipelines, probing/metrics and input forwarding — and talks to its
//! consumer exclusively through the injected [`flume::Sender<ClientEvent>`]
//! and the returned [`CommandSender`]. Decoded pixels live in the shared
//! [`Displays`] framebuffers; the consumer runs [`Session::run`] on its own
//! task/thread and renders damage rects as they arrive.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use codec::{EncodedPacket, VideoCodecType, VideoDecoder};
use protocol::fragment::Reassembler;
use protocol::{self, ControlMessage, CursorMessage, channel};
use tracing::{debug, info, warn};
use transport::{
    Connector, DirectConnector, EncryptedStream, ReliableTransport, Target, UdpLeg, UdpTransport,
    udp_detect, udp_probe,
};

use crate::audio::{AudioClient, AudioSpec};
use crate::avsync::{self, SyncStats};
use crate::command::{COMMAND_CHANNEL_CAPACITY, ClientCommand, CommandReceiver, CommandSender};
use crate::events::ClientEvent;
use crate::framebuffer::Displays;
use crate::mic::{MicClient, MicMode};
use crate::webcam::CamClient;

/// Keyframe requests on unrecoverable frame loss are rate-limited to one
/// per this interval (riesgos §2.1).
const KEYFRAME_REQUEST_MIN_INTERVAL: Duration = Duration::from_millis(500);

/// Dead-server detection (§18.39): with no inbound traffic at all for this
/// long (probes are answered once per second on every leg), the session is
/// declared dead. The server applies the symmetric rule (5 s) to client
/// traffic.
const SERVER_LIVENESS_TIMEOUT: Duration = Duration::from_secs(7);

/// How long a partial frame may sit untouched before it is counted as lost.
/// Fragments of one frame leave the server back-to-back, so 250 ms covers
/// even heavy reordering; the sweep runs on the presentation tick.
const REASM_STALE_AGE: Duration = Duration::from_millis(250);

/// Pre-session gate budget (§18.53): covers the worst-case server-side
/// conflict ask (~30 s) plus console eviction and autologon (~60 s) before
/// the `SessionReady`/`UdpOffer` arrives. The normal path answers in one
/// round-trip.
const PRE_SESSION_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the pre-session gate waits for the consumer's
/// `AnswerConflict` after emitting `ClientEvent::ConsoleConflict` — always
/// shorter than the server's ask timeout (default 30 s) so the reply can
/// still reach it. Without an answer the gate denies (fail-closed).
const CONFLICT_ANSWER_TIMEOUT: Duration = Duration::from_secs(25);

/// Presentation-queue pump cadence (§9.1): held frames are presented when
/// the audio clock reaches their timestamp.
const PRESENT_TICK: Duration = Duration::from_millis(8);

/// A/V sync statistics are logged with this cadence.
const SYNC_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// Static configuration of one client session.
// A plain bag of independent switches the consumer fills; grouping them
// into sub-structs would only add nesting at every call site.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Server address (direct, or the launcher's local tunnel endpoint).
    pub addr: SocketAddr,
    /// Session ticket delivered out of band (§6.3.1).
    pub ticket: [u8; protocol::TICKET_LENGTH],
    /// Shared secret delivered out of band (§6.3.1).
    pub key: [u8; 32],
    /// Audio playback (§9); when false, audio packets are dropped.
    pub audio: bool,
    /// Lab aid: dump every decoded audio frame to this file as raw s16le
    /// interleaved PCM (rate/channels from the announced spec).
    pub audio_dump: Option<std::path::PathBuf>,
    /// Mic capture mode (§9.2); off by default.
    pub mic: MicMode,
    /// Ask the server to hide the physical console behind a privacy screen
    /// for the duration of the session (M7; `FLAG_PRIVACY_SCREEN`).
    pub privacy_screen: bool,
    /// Offer smartcard redirection (§17; `FLAG_SMARTCARD`): the session
    /// runs the request dispatcher against the local PC/SC backend (or
    /// answers `SCARD_E_NO_SERVICE` when there is none).
    pub smartcard: bool,
    /// Offer webcam redirection (§18.57; `FLAG_WEBCAM`): when the server
    /// asks for it, stream the local camera VP9-encoded on `channel::WEBCAM`.
    pub webcam: bool,
    /// Accept redirected print jobs (§18.57 phase 0; `FLAG_PRINTER`):
    /// received PDFs are materialized in this directory and reported as
    /// [`ClientEvent::PrintJobDone`]. `None` disables the channel.
    pub printer: Option<std::path::PathBuf>,
    /// Preferred remote display resolution (§18.37), sent in the HELLO.
    /// `(0, 0)` means no preference: the server keeps its current mode or
    /// applies its own fixed `resolution` policy.
    pub pref_resolution: (u16, u16),
}

/// One client session (§15.3). Build with [`Session::new`], then drive it
/// with [`Session::run`] on a dedicated task.
pub struct Session {
    config: ClientConfig,
    events: flume::Sender<ClientEvent>,
    command_rx: CommandReceiver,
    displays: Displays,
}

impl Session {
    /// Creates a session. The consumer injects the (ideally bounded, see
    /// [`crate::events::EVENT_CHANNEL_CAPACITY`]) event channel and receives
    /// the command sender plus the shared framebuffer registry.
    #[must_use]
    pub fn new(
        config: ClientConfig,
        events: flume::Sender<ClientEvent>,
    ) -> (Self, CommandSender, Displays) {
        let (command_tx, command_rx) = flume::bounded(COMMAND_CHANNEL_CAPACITY);
        let displays = Displays::default();
        (
            Self {
                config,
                events,
                command_rx,
                displays: displays.clone(),
            },
            command_tx,
            displays,
        )
    }

    /// Runs the session until it ends (close command, peer disconnect or
    /// fatal error). Always emits [`ClientEvent::Disconnected`] as the last
    /// event (unless the consumer is already gone).
    pub async fn run(mut self) -> Result<()> {
        let result = Box::pin(self.run_inner()).await;
        let _ = self.events.try_send(ClientEvent::Disconnected);
        result
    }

    async fn run_inner(&mut self) -> Result<()> {
        let target = Target::new(self.config.addr, self.config.ticket, self.config.key);
        let (raw, _) = DirectConnector::new().connect(&target).await?;
        let mut flags = 0u8;
        if self.config.privacy_screen {
            flags |= protocol::FLAG_PRIVACY_SCREEN;
        }
        if self.config.smartcard {
            flags |= protocol::FLAG_SMARTCARD;
        }
        // Webcam (§18.57): the source is probed here so the flag reflects a
        // real capture capability, and passed down to the Core.
        let cam_source = if self.config.webcam {
            ::webcam::default_source()
        } else {
            None
        };
        if cam_source.is_some() {
            flags |= protocol::FLAG_WEBCAM;
        }
        // Printer (§18.57): like the webcam, the flag must reflect a real
        // capability, so the save directory is prepared before the HELLO.
        let printer = self.config.printer.as_ref().and_then(|dir| {
            match crate::printer::PrinterJobs::new(dir.clone()) {
                Ok(jobs) => Some(jobs),
                Err(e) => {
                    warn!(dir = %dir.display(), "printer save dir unavailable: {e}");
                    None
                }
            }
        });
        if printer.is_some() {
            flags |= protocol::FLAG_PRINTER;
        }
        let mut stream: EncryptedStream<Box<dyn ReliableTransport>> =
            EncryptedStream::client_handshake(
                raw,
                &self.config.ticket,
                &self.config.key,
                flags,
                self.config.pref_resolution,
            )
            .await?;
        info!(
            privacy_screen = self.config.privacy_screen,
            smartcard = self.config.smartcard,
            "handshake done"
        );

        let clock_start = Instant::now();
        let clock = || micros_since(clock_start);
        // §18.53 pre-session gate: the orchestrator service resolves the
        // console-conflict policy before handing off to the agent. We wait
        // for its `SessionReady` (or an early `UdpOffer` from a lab agent,
        // which doubles as the ready signal), answering conflict asks.
        let early_offer = self.pre_session(&mut stream).await?;
        let leg = match early_offer {
            Some((port, token)) => {
                udp_probe(
                    self.config.addr.ip(),
                    &self.config.ticket,
                    &self.config.key,
                    port,
                    token,
                    clock,
                    3,
                    Duration::from_millis(500),
                )
                .await?
            }
            None => {
                udp_detect(
                    &mut stream,
                    self.config.addr.ip(),
                    &self.config.ticket,
                    &self.config.key,
                    clock,
                    3,
                    Duration::from_millis(500),
                )
                .await?
            }
        };
        if let Some(leg) = &leg {
            info!(rtt_micros = leg.initial_rtt_micros, "display leg: UDP");
        } else {
            warn!("UDP detection failed, display leg: TCP fallback");
        }
        if !emit(&self.events, ClientEvent::Connected { udp: leg.is_some() }) {
            return Ok(());
        }

        let mut core = Core::new(
            self.events.clone(),
            self.displays.clone(),
            &self.config,
            clock_start,
            cam_source,
            printer,
        );
        core.stats.set_udp(leg.is_some());
        // Webcam capabilities (§18.57): the server answers with WebcamConfig
        // (+ WebcamStart) when its policy allows the stream.
        if let Some(caps) = core.cam.caps_message() {
            stream.send(channel::CONTROL, &caps.encode()?).await?;
        }
        match leg {
            Some(leg) => self.udp_loop(leg, stream, clock_start, &mut core).await,
            None => self.tcp_loop(stream, clock_start, &mut core).await,
        }
    }

    /// §18.53 pre-session gate, run right after the handshake: waits for
    /// the service's `SessionReady` (or an early `UdpOffer` from a lab
    /// agent, returned for the caller to probe) and answers
    /// console-conflict asks via the consumer's `AnswerConflict` command.
    /// A `SessionEnd` refusal surfaces as a visible error and ends the
    /// session before any pipeline starts.
    async fn pre_session(
        &mut self,
        stream: &mut EncryptedStream<Box<dyn ReliableTransport>>,
    ) -> Result<Option<(u16, [u8; 16])>> {
        let deadline = Instant::now() + PRE_SESSION_TIMEOUT;
        let mut buf = [0u8; 2048];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            anyhow::ensure!(!remaining.is_zero(), "pre-session gate timed out");
            let (ch, n) = tokio::time::timeout(remaining, stream.recv_into(&mut buf))
                .await
                .context("pre-session gate timed out")??;
            if ch != channel::CONTROL {
                continue;
            }
            match ControlMessage::decode(&buf[..n]) {
                Ok(ControlMessage::SessionReady) => return Ok(None),
                Ok(ControlMessage::UdpOffer { port, token }) => {
                    // Lab agent (no service): the offer doubles as ready.
                    return Ok(Some((port, token)));
                }
                Ok(ControlMessage::SessionEnd { reason }) => {
                    let text = match reason {
                        protocol::SESSION_END_CONSOLE_BUSY => {
                            "the remote console is busy (owned by another user)".to_string()
                        }
                        protocol::SESSION_END_DENIED_BY_USER => {
                            "console takeover refused".to_string()
                        }
                        other => format!("session refused by the server (reason {other})"),
                    };
                    let _ = emit(&self.events, ClientEvent::Error(text.clone()));
                    anyhow::bail!("{text}");
                }
                Ok(ControlMessage::ConsoleConflict { user, state }) => {
                    let take_over = self.answer_conflict(user, state).await;
                    let reply = ControlMessage::ConsoleConflictReply { take_over };
                    stream.send(channel::CONTROL, &reply.encode()?).await?;
                    if !take_over {
                        info!("console takeover denied; waiting for the server to close");
                    }
                }
                Ok(ControlMessage::SessionProgress { stage }) => {
                    let text = match stage {
                        protocol::SESSION_STAGE_EVICTING => {
                            "cleaning up the previous console session…".to_string()
                        }
                        protocol::SESSION_STAGE_AUTOLOGON => {
                            "warming up the session (signing in)…".to_string()
                        }
                        other => format!("server is preparing the session ({other})…"),
                    };
                    let _ = emit(&self.events, ClientEvent::Status(text));
                }
                // Anything else is not part of the pre-session vocabulary.
                _ => {}
            }
        }
    }

    /// Emits the conflict query and waits for the consumer's
    /// `AnswerConflict` command. Fail-closed: timeout, a disconnected
    /// command channel or an undeliverable query event all deny the
    /// takeover.
    async fn answer_conflict(
        &mut self,
        user: Option<String>,
        state: u8,
    ) -> bool {
        info!(revealed = user.is_some(), state, "console conflict; asking the consumer");
        if !emit(&self.events, ClientEvent::ConsoleConflict { user, state }) {
            return false;
        }
        let deadline = Instant::now() + CONFLICT_ANSWER_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                warn!("no AnswerConflict within {CONFLICT_ANSWER_TIMEOUT:?}; denying the takeover");
                return false;
            }
            match tokio::time::timeout(remaining, self.command_rx.recv_async()).await {
                Ok(Ok(ClientCommand::AnswerConflict { take_over })) => return take_over,
                Ok(Ok(other)) => {
                    debug!(?other, "command ignored while waiting for AnswerConflict");
                }
                Ok(Err(_)) => {
                    warn!("command channel closed during the conflict ask; denying");
                    return false;
                }
                Err(_) => {
                    warn!("no AnswerConflict within {CONFLICT_ANSWER_TIMEOUT:?}; denying the takeover");
                    return false;
                }
            }
        }
    }

    /// Reliable-stream-only session (§4.1 fallback): control, display,
    /// audio and input all share the encrypted stream. Probes keep running
    /// here too (1/s, §12.1): they are the session keepalive — the server
    /// kills the session after ~5 s without client traffic (§18.39).
    async fn tcp_loop(
        &mut self,
        mut stream: EncryptedStream<Box<dyn ReliableTransport>>,
        clock_start: Instant,
        core: &mut Core,
    ) -> Result<()> {
        // Sized for the largest stream record (~64 KiB): cursor shapes (§10)
        // can be a few KiB; datagrams on this buffer stay MTU-sized anyway.
        let mut buf = vec![0u8; 65_536];
        let clock = || micros_since(clock_start);
        let mut probe_tick = tokio::time::interval(Duration::from_secs(1));
        let mut liveness_tick = tokio::time::interval(Duration::from_secs(1));
        let mut last_recv = Instant::now();
        let mut present_tick = tokio::time::interval(PRESENT_TICK);
        let mut stats_tick = tokio::time::interval(Duration::from_secs(1));
        let mut prev_fps_stats: Option<(u64, Instant)> = None;
        loop {
            tokio::select! {
                res = stream.recv_into(&mut buf) => {
                    let (ch, n) = res?;
                    last_recv = Instant::now();
                    if !core.handle_packet(ch, &buf[..n])? {
                        return Ok(());
                    }
                    Self::flush_out(&mut stream, core).await?;
                }
                _ = probe_tick.tick() => {
                    // Keepalive + RTT sampling on the fallback leg (§18.39).
                    stream
                        .send(channel::CONTROL, &protocol::Probe { t1: clock() }.encode())
                        .await?;
                }
                _ = liveness_tick.tick() => {
                    if last_recv.elapsed() >= SERVER_LIVENESS_TIMEOUT {
                        anyhow::bail!("server silent for {SERVER_LIVENESS_TIMEOUT:?}; session dead");
                    }
                }
                msg = core.mic.recv() => {
                    if let Some(payload) = msg {
                        // TCP fallback session: audio-in goes over the
                        // reliable stream (like the server's audio-out
                        // fallback, §9).
                        stream.send(channel::AUDIO_IN, &payload).await?;
                    } else {
                        warn!("mic thread ended; audio-in disabled for this session");
                        core.mic.disable();
                    }
                }
                msg = core.cam.recv() => {
                    if let Some(payload) = msg {
                        // §18.57: webcam frames are droppable, but with no
                        // UDP leg they share the reliable stream (best
                        // effort, like audio-out on the fallback).
                        stream.send(channel::WEBCAM, &payload).await?;
                    } else {
                        warn!("webcam thread ended; webcam disabled for this session");
                        core.cam.stop();
                    }
                }
                cmd = self.command_rx.recv_async() => {
                    if !Self::handle_command(cmd, &mut stream, core).await? {
                        return Ok(());
                    }
                }
                _ = present_tick.tick() => {
                    for ev in core.storage.pump() {
                        if !emit(&core.events, ev) {
                            return Ok(());
                        }
                    }
                    Self::flush_out(&mut stream, core).await?;
                }
                _ = stats_tick.tick() => {
                    let snap = core.stats.snapshot();
                    Self::log_present_fps(&mut prev_fps_stats, &snap);
                    if !emit(&core.events, ClientEvent::Stats(snap)) {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// UDP display leg session (§4.2): receives display traffic while
    /// running continuous probing (§12.1) — one probe per second; answered
    /// ones feed the RTT/jitter EWMAs, and once per second a MetricsReport
    /// goes to the server over the reliable control stream (§12.2).
    /// Unrecoverable frame loss triggers a rate-limited `RequestKeyframe`.
    ///
    /// The reliable stream is read too: cursor messages always travel on it
    /// (§10 — a lost shape would leave a stale cursor), along with any
    /// reliable S→C control traffic.
    ///
    /// Select loop over display + stream + probing + mic + commands;
    /// splitting it further would only scatter the session state.
    #[allow(clippy::too_many_lines)]
    async fn udp_loop(
        &mut self,
        leg: UdpLeg<UdpTransport>,
        mut stream: EncryptedStream<Box<dyn ReliableTransport>>,
        clock_start: Instant,
        core: &mut Core,
    ) -> Result<()> {
        let clock = || micros_since(clock_start);
        let mut leg = leg;
        let mut buf = vec![0u8; 65_536];
        let mut sbuf = vec![0u8; 65_536];

        // 5 probes/s: the report period (1 s) gets several samples, so the
        // period-min filter has something to work with (§12.2).
        let mut probe_tick = tokio::time::interval(Duration::from_millis(200));
        let mut report_tick = tokio::time::interval(Duration::from_secs(1));
        let mut rtt_ewma: Option<f64> = None;
        let mut jitter_ewma = 0.0_f64;
        // Minimum RTT seen in the current report period (§12.2): acks share
        // the client's recv loop with display decode, so under load a sample
        // measures our own queueing delay, not the network's. The period min
        // discards that self-inflicted pollution (the max inflates, the min
        // stays the true path RTT); the server's baseline windows it again.
        let mut rtt_period_min: Option<f64> = None;
        // The first ack is delayed by the server's session bring-up (capture
        // thread, encoder init), so it is discarded as warm-up and never
        // feeds the EWMA/baseline.
        let mut warmup_acks = 1u8;
        let mut prev_lost = 0u64;
        let mut prev_received = 0u64;
        let mut present_tick = tokio::time::interval(PRESENT_TICK);
        let mut stats_tick = tokio::time::interval(Duration::from_secs(1));
        let mut prev_fps_stats: Option<(u64, Instant)> = None;
        let mut liveness_tick = tokio::time::interval(Duration::from_secs(1));
        let mut last_recv = Instant::now();
        loop {
            tokio::select! {
                res = leg.dgram.recv_into(&mut buf) => {
                    let (ch, n) = res?;
                    last_recv = Instant::now();
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
                        rtt_period_min = Some(rtt_period_min.map_or(rtt, |m: f64| m.min(rtt)));
                        continue;
                    }
                    if !core.handle_packet(ch, &buf[..n])? {
                        return Ok(());
                    }
                    Self::flush_out(&mut stream, core).await?;
                    Self::maybe_request_keyframe(&mut stream, core).await?;
                }
                res = stream.recv_into(&mut sbuf) => {
                    // Reliable S→C traffic while the UDP leg is up: cursor
                    // messages (§10) and any control announcements.
                    let (ch, n) = res?;
                    last_recv = Instant::now();
                    if !core.handle_packet(ch, &sbuf[..n])? {
                        return Ok(());
                    }
                    Self::flush_out(&mut stream, core).await?;
                }
                _ = probe_tick.tick() => {
                    leg.dgram
                        .send(channel::CONTROL, &protocol::Probe { t1: clock() }.encode())
                        .await?;
                }
                _ = liveness_tick.tick() => {
                    if last_recv.elapsed() >= SERVER_LIVENESS_TIMEOUT {
                        anyhow::bail!("server silent for {SERVER_LIVENESS_TIMEOUT:?}; session dead");
                    }
                }
                _ = report_tick.tick() => {
                    // Jitter over min-filtered samples: the raw per-ack
                    // spread is our own scheduling noise, not the link's.
                    if let Some(min) = rtt_period_min {
                        let prev = rtt_ewma.unwrap_or(min);
                        jitter_ewma += 0.25 * ((min - prev).abs() - jitter_ewma);
                        rtt_ewma = Some(prev + 0.25 * (min - prev));
                    }
                    let Some(rtt) = rtt_period_min.take().or(rtt_ewma) else { continue };
                    let lost = leg.dgram.lost_estimate();
                    let received = leg.dgram.received();
                    let dlost = lost - prev_lost;
                    let drecv = received - prev_received;
                    prev_lost = lost;
                    prev_received = received;
                    // Swallowed-invalid counters (blind spot of the seq-gap
                    // loss estimate): a rising one explains "no loss on the
                    // wire but frames never complete".
                    let (bad_token, aead_fail, replay, too_old) = leg.dgram.dropped_invalid();
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
                    debug!(
                        rtt_micros = rtt,
                        jitter_micros = jitter_ewma,
                        loss_permille = loss_rate * 1000.0,
                        dlost,
                        drecv,
                        bad_token,
                        aead_fail,
                        replay,
                        too_old,
                        reasm_dropped = core.reasm.dropped_frames(),
                        "metrics report"
                    );
                    #[allow(clippy::cast_possible_truncation)] // micros→ms fits f32
                    core.stats.set_net(
                        (rtt / 1000.0) as f32,
                        (jitter_ewma / 1000.0) as f32,
                        (loss_rate * 1000.0) as f32,
                        received,
                        lost,
                    );
                    stream.send(channel::CONTROL, &report.encode()?).await?;
                }
                msg = core.mic.recv() => {
                    if let Some(payload) = msg {
                        // Audio-in goes over the UDP leg (lossy is fine: the
                        // server's jitter buffer conceals gaps), like probes
                        // do.
                        leg.dgram.send(channel::AUDIO_IN, &payload).await?;
                    } else {
                        warn!("mic thread ended; audio-in disabled for this session");
                        core.mic.disable();
                    }
                }
                msg = core.cam.recv() => {
                    if let Some(payload) = msg {
                        // §18.57: webcam frames travel on the lossy leg.
                        leg.dgram.send(channel::WEBCAM, &payload).await?;
                    } else {
                        warn!("webcam thread ended; webcam disabled for this session");
                        core.cam.stop();
                    }
                }
                cmd = self.command_rx.recv_async() => {
                    if !Self::handle_command(cmd, &mut stream, core).await? {
                        return Ok(());
                    }
                }
                _ = present_tick.tick() => {
                    // A loss right before a static period produces no newer
                    // frames to trigger the window eviction; sweep by age so
                    // loss detection never depends on future traffic (§12.2).
                    core.reasm.evict_stale(REASM_STALE_AGE);
                    Self::maybe_request_keyframe(&mut stream, core).await?;
                    for ev in core.storage.pump() {
                        if !emit(&core.events, ev) {
                            return Ok(());
                        }
                    }
                    Self::flush_out(&mut stream, core).await?;
                }
                _ = stats_tick.tick() => {
                    let snap = core.stats.snapshot();
                    Self::log_present_fps(&mut prev_fps_stats, &snap);
                    if !emit(&core.events, ClientEvent::Stats(snap)) {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Logs the presentation rate every ~5 s of stats ticks. The F3 overlay
    /// shows the same numbers live; the log carries them for offline
    /// analysis (fps deltas of the cumulative `presented` counter).
    fn log_present_fps(
        prev: &mut Option<(u64, Instant)>,
        snap: &crate::stats::StatsSnapshot,
    ) {
        let now = Instant::now();
        let Some((prev_presented, prev_at)) = *prev else {
            *prev = Some((snap.presented, now));
            return;
        };
        let dt = now.duration_since(prev_at).as_secs_f64();
        if dt >= 4.0 {
            #[expect(clippy::cast_precision_loss, reason = "frame deltas fit an f64")]
            let fps = (snap.presented - prev_presented) as f64 / dt;
            *prev = Some((snap.presented, now));
            tracing::info!(
                fps = format_args!("{fps:.1}"),
                rtt_ms = snap.rtt_ms,
                loss_permille = snap.loss_permille,
                udp = snap.udp,
                queue = snap.present_queue_len,
                held = snap.held,
                dropped_overflow = snap.dropped_overflow,
                dropped_stale = snap.dropped_stale,
                kf_req = snap.keyframe_requests,
                "client presentation rate"
            );
        }
    }

    /// Asks the server for a keyframe when the stream is broken (decode
    /// errors) or a frame was evicted incomplete; the rate limit re-asks
    /// every 500 ms while the stream stays broken. Called after each
    /// datagram and on the present tick (the stale sweep can evict without
    /// new traffic).
    async fn maybe_request_keyframe(
        stream: &mut EncryptedStream<Box<dyn ReliableTransport>>,
        core: &mut Core,
    ) -> Result<()> {
        let dropped = core.reasm.dropped_frames();
        let awaiting = core.recovery_awaiting.load(Ordering::Relaxed);
        if (dropped > core.prev_dropped || awaiting)
            && core
                .last_key_req
                .is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_MIN_INTERVAL)
        {
            info!(dropped, awaiting, "stream broken; requesting keyframe");
            let req = ControlMessage::RequestKeyframe { display_id: 0 };
            stream.send(channel::CONTROL, &req.encode()?).await?;
            core.stats.record_keyframe_request();
            core.last_key_req = Some(Instant::now());
        }
        core.prev_dropped = dropped;
        Ok(())
    }

    /// Flushes queued clipboard, storage and smartcard answers on the
    /// reliable stream (M8, §17).
    async fn flush_out(
        stream: &mut EncryptedStream<Box<dyn ReliableTransport>>,
        core: &mut Core,
    ) -> Result<()> {
        for msg in core.clip_out.drain(..) {
            stream.send(channel::CLIPBOARD, &msg.encode()).await?;
        }
        for msg in core.storage.out.drain(..) {
            stream.send(channel::STORAGE, &msg.encode()).await?;
        }
        for msg in core.scard_out.drain(..) {
            stream.send(channel::SMARTCARD, &msg.encode()).await?;
        }
        if let Some(printer) = &mut core.printer {
            for msg in printer.drain_out() {
                stream.send(channel::PRINTER, &msg.encode()).await?;
            }
        }
        // Smartcard replies produced by the dispatcher threads since the
        // last flush (bounded by the presentation tick, ~8 ms of latency).
        while let Ok(msg) = core.scard_rx.try_recv() {
            stream.send(channel::SMARTCARD, &msg.encode()).await?;
        }
        Ok(())
    }

    /// Applies one consumer command; returns `false` when the session must
    /// end (close requested or command channel shut down).
    async fn handle_command(
        cmd: std::result::Result<ClientCommand, flume::RecvError>,
        stream: &mut EncryptedStream<Box<dyn ReliableTransport>>,
        core: &mut Core,
    ) -> Result<bool> {
        match cmd {
            Ok(ClientCommand::Input(msg)) => {
                let (buf, n) = msg.encode();
                stream.send(channel::INPUT, &buf[..n]).await?;
            }
            Ok(ClientCommand::SetMic(on)) => core.mic.set_allowed(on),
            Ok(ClientCommand::ClipboardChanged(text)) => {
                // Local clipboard changed (M8): announce it; the server will
                // pull the text if it wants it.
                let offer = core.clip.local_text_changed(text.as_bytes());
                stream.send(channel::CLIPBOARD, &offer.encode()).await?;
            }
            Ok(ClientCommand::Resize { width, height }) => {
                // §18.37: ask the server to re-apply its resolution policy
                // with the new preference; the answer is a fresh
                // `DisplayInfo` with the real geometry.
                let msg = ControlMessage::DesktopResize { width, height };
                stream.send(channel::CONTROL, &msg.encode()?).await?;
            }
            Ok(ClientCommand::RequestKeyframe { display_id }) => {
                if core
                    .last_key_req
                    .is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_MIN_INTERVAL)
                {
                    let req = ControlMessage::RequestKeyframe { display_id };
                    stream.send(channel::CONTROL, &req.encode()?).await?;
                    core.last_key_req = Some(Instant::now());
                }
            }
            Ok(ClientCommand::StorageList) => core.storage.list(),
            Ok(ClientCommand::StorageUpload { path }) => {
                for ev in core.storage.start_upload(&path) {
                    if !emit(&core.events, ev) {
                        return Ok(false);
                    }
                }
            }
            Ok(ClientCommand::StorageDownload { name, dest }) => {
                for ev in core.storage.start_download(&name, &dest) {
                    if !emit(&core.events, ev) {
                        return Ok(false);
                    }
                }
            }
            Ok(ClientCommand::StorageAbort { transfer_id }) => core.storage.abort(transfer_id),
            Ok(ClientCommand::SmartcardEnable(on)) => core.set_smartcard(on),
            Ok(ClientCommand::LockRemote) => {
                info!("remote lock requested (Ctrl+Alt+Shift+L)");
                stream.send(channel::CONTROL, &ControlMessage::LockRequest.encode()?).await?;
            }
            Ok(ClientCommand::AnswerConflict { .. }) => {
                // Only meaningful during the pre-session gate (§18.53).
                debug!("AnswerConflict arrived mid-session; ignored");
            }
            Ok(ClientCommand::Close) => {
                info!("close requested by the consumer");
                return Ok(false);
            }
            Err(_) => {
                info!("command channel closed; ending session");
                return Ok(false);
            }
        }
        // Storage commands queue wire messages instead of sending them
        // (they also emit events); flush anything they produced.
        Self::flush_out(stream, core).await?;
        Ok(true)
    }
}

/// Sends one event without blocking (§15.3): a full channel drops the
/// notification (pixels are never lost, they stay in the framebuffer); a
/// disconnected channel means the consumer is gone and ends the session.
fn emit(tx: &flume::Sender<ClientEvent>, event: ClientEvent) -> bool {
    match tx.try_send(event) {
        Ok(()) => true,
        Err(flume::TrySendError::Full(event)) => {
            debug!(?event, "event channel full; notification dropped");
            true
        }
        Err(flume::TrySendError::Disconnected(_)) => false,
    }
}

fn micros_since(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// A decoded frame waiting for the audio clock to catch up (§9.1).
struct HeldFrame {
    display_id: u8,
    /// Present when `audio_now` reaches this capture timestamp (ms).
    hold_until: u32,
    frame: codec::DecodedFrame,
}

/// Presentation queue for held frames (§9.1): at most `MAX_HELD_FRAMES`;
/// on overflow the oldest is dropped — stale video is worse than a skipped
/// frame.
struct PresentQueue {
    frames: VecDeque<HeldFrame>,
}

/// Maximum frames waiting for presentation (§9.1: "a couple of frames").
const MAX_HELD_FRAMES: usize = 3;

impl PresentQueue {
    fn new() -> Self {
        Self {
            frames: VecDeque::new(),
        }
    }

    /// Holds a frame until the audio clock reaches `hold_until`.
    fn hold(&mut self, display_id: u8, hold_until: u32, frame: codec::DecodedFrame, stats: &mut SyncStats, live: &crate::stats::SessionStats) {
        stats.record_hold();
        if self.frames.len() >= MAX_HELD_FRAMES {
            self.frames.pop_front();
            stats.record_overflow_drop();
            live.record_overflow_drop();
        }
        self.frames.push_back(HeldFrame {
            display_id,
            hold_until,
            frame,
        });
    }
}

/// One work item for the decode thread (§15.3).
enum DecodeJob {
    /// Display announced (`DisplayInfo`): codec for the display.
    Announce { display_id: u8, codec: VideoCodecType },
    /// One reassembled display frame.
    Frame(protocol::fragment::ReassembledFrame),
}

/// Decode jobs in flight. Bounded: a consumer too slow to decode must drop
/// frames (breaking the predictive chain → keyframe) instead of building
/// presentation latency.
const DECODE_QUEUE_CAPACITY: usize = 4;

/// Decode + presentation worker (§15.3): owns the decoders, the recovery
/// state and the A/V presentation queue so video decode and framebuffer
/// writes never block the network task (audio datagrams and probe acks
/// share that task's recv loop; a 20 ms VP9 decode behind a keyframe burst
/// was enough to push them past the audio jitter window).
struct DecodeWorker {
    rx: flume::Receiver<DecodeJob>,
    /// Control jobs (Announce). Unbounded and drained before frames: a
    /// codec switch must never be lost behind a full frame queue.
    ctl_rx: flume::Receiver<DecodeJob>,
    events: flume::Sender<ClientEvent>,
    displays: Displays,
    /// One decoder per display (§8.3); created from DisplayInfo
    /// announcements, or Raw on first frame for servers that do not
    /// announce (§8.4 compat).
    decoders: HashMap<u8, Box<dyn VideoDecoder>>,
    /// Shared with the network task: it drives the keyframe requests.
    recovery_awaiting: Arc<AtomicBool>,
    /// Audio playback cursor (capture-clock ms), for the A/V decisions.
    audio_clock: Arc<AtomicI64>,
    present_queue: PresentQueue,
    avsync_stats: SyncStats,
    avsync_log: Instant,
    stats: Arc<crate::stats::SessionStats>,
}

impl DecodeWorker {
    fn spawn(
        events: flume::Sender<ClientEvent>,
        displays: Displays,
        audio_clock: Arc<AtomicI64>,
        stats: Arc<crate::stats::SessionStats>,
    ) -> (flume::Sender<DecodeJob>, flume::Sender<DecodeJob>, Arc<AtomicBool>) {
        let (tx, rx) = flume::bounded(DECODE_QUEUE_CAPACITY);
        let (ctl_tx, ctl_rx) = flume::unbounded();
        let recovery_awaiting = Arc::new(AtomicBool::new(false));
        let mut worker = Self {
            rx,
            ctl_rx,
            events,
            displays,
            decoders: HashMap::new(),
            recovery_awaiting: Arc::clone(&recovery_awaiting),
            audio_clock,
            present_queue: PresentQueue::new(),
            avsync_stats: SyncStats::default(),
            avsync_log: Instant::now(),
            stats,
        };
        std::thread::Builder::new()
            .name("decode-worker".into())
            .spawn(move || worker.run())
            .expect("spawning the decode worker");
        (tx, ctl_tx, recovery_awaiting)
    }

    fn audio_now_ms(&self) -> Option<u32> {
        let v = self.audio_clock.load(Ordering::Relaxed);
        u32::try_from(v).ok()
    }

    /// Main loop: drains decode jobs, then pumps the presentation queue at
    /// the present cadence (§9.1). Ends when the session drops the sender.
    fn run(&mut self) {
        loop {
            // Control jobs first: an Announce lost behind a frame burst
            // would leave every frame undecodable until the next one.
            while let Ok(job) = self.ctl_rx.try_recv() {
                self.apply(job);
            }
            match self.rx.recv_timeout(PRESENT_TICK) {
                Ok(job) => self.apply(job),
                Err(flume::RecvTimeoutError::Timeout) => {}
                Err(flume::RecvTimeoutError::Disconnected) => return,
            }
            // Catch up on queued frames before pumping: decode is the
            // bottleneck this thread exists to absorb.
            while let Ok(job) = self.rx.try_recv() {
                self.apply(job);
            }
            if !self.avsync_pump() {
                return; // consumer gone
            }
        }
    }

    fn apply(&mut self, job: DecodeJob) {
        match job {
            DecodeJob::Announce { display_id, codec } => match codec::build_decoder(codec) {
                Ok(dec) => {
                    self.decoders.insert(display_id, dec);
                }
                Err(e) => {
                    warn!(codec = codec.wire_id(), "cannot build decoder: {e}");
                    let _ = emit(
                        &self.events,
                        ClientEvent::Error(format!("cannot build video decoder: {e}")),
                    );
                }
            },
            DecodeJob::Frame(complete) => self.decode_frame(complete),
        }
    }

    fn decode_frame(&mut self, mut complete: protocol::fragment::ReassembledFrame) {
        // Every frame opens with the wire id (u16 BE) of the codec that
        // produced it (§8.5): the DisplayInfo announce travels on another leg
        // and can lose the race against the first fragments (or the datagram
        // can be lost outright), so the per-frame tag — not the last
        // announce — is authoritative for the decoder choice. Feeding a frame
        // to the wrong decoder is not just a decode error: a VP9 bitstream
        // read as a rects header yields garbage geometry and a multi-GB
        // allocation.
        if complete.data.len() < 2 {
            return;
        }
        let codec_id = u16::from_be_bytes([complete.data[0], complete.data[1]]);
        let payload = complete.data.split_off(2);
        let Some(codec_type) = VideoCodecType::from_wire_id(codec_id) else {
            warn!(codec_id, "frame with unknown codec tag; dropped");
            return;
        };
        let needs_build = self
            .decoders
            .get(&complete.display_id)
            .is_none_or(|d| d.codec_type() != codec_type);
        if needs_build {
            match codec::build_decoder(codec_type) {
                Ok(dec) => {
                    info!(display_id = complete.display_id, codec_id, "decoder (re)built from frame tag");
                    self.decoders.insert(complete.display_id, dec);
                }
                Err(e) => {
                    warn!(codec_id, "cannot build decoder: {e}");
                    return;
                }
            }
        }
        let decoder = self
            .decoders
            .get_mut(&complete.display_id)
            .expect("inserted above");
        match decoder.decode(&EncodedPacket {
            data: payload,
            keyframe: true,
        }) {
            Ok(frame) => {
                self.recovery_awaiting.store(false, Ordering::Relaxed);
                // Every display is presented into its own framebuffer
                // (§8.3, §15.3); the A/V sync decision (§9.1) decides:
                // present, hold for the audio clock, or drop.
                self.present_or_hold(complete.display_id, frame, complete.capture_ts_ms);
            }
            Err(e) => {
                // A lost fragment breaks the predictive chain: tolerate
                // the error and let the network task ask for a keyframe
                // (§12.2).
                if !self.recovery_awaiting.swap(true, Ordering::Relaxed) {
                    warn!("decode failed ({e}); awaiting keyframe");
                    let _ = emit(
                        &self.events,
                        ClientEvent::Error(format!("video decode failed: {e}")),
                    );
                }
            }
        }
    }

    /// Writes one decoded frame into the shared framebuffer and notifies
    /// the consumer (§15.3). Returns `false` when the consumer is gone.
    fn present(&mut self, display_id: u8, frame: &codec::DecodedFrame) -> bool {
        self.stats.record_present();
        let Some(display) = self.displays.get_or_insert(display_id) else {
            return true; // registry poisoned: keep the session, lose the frame
        };
        let rect = {
            let mut guard = display.write().unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.write_frame(frame);
            guard.full_rect()
        };
        emit(
            &self.events,
            ClientEvent::FrameDirty {
                display_id,
                rects: vec![rect],
            },
        )
    }

    /// Applies the A/V sync decision (§9.1) to one freshly decoded frame.
    fn present_or_hold(&mut self, display_id: u8, frame: codec::DecodedFrame, video_ts_ms: u32) {
        let audio_now = self.audio_now_ms();
        match avsync::decide(video_ts_ms, audio_now.unwrap_or(0), audio_now.is_some()) {
            avsync::PresentDecision::PresentNow => {
                if let Some(now) = audio_now {
                    self.avsync_stats
                        .record_present(i64::from(video_ts_ms) - i64::from(now));
                }
                // A gone consumer is noticed by the pump on the next tick.
                let _ = self.present(display_id, &frame);
            }
            avsync::PresentDecision::HoldUntil(ts) => {
                self.stats.record_hold();
                self.present_queue
                    .hold(display_id, ts, frame, &mut self.avsync_stats, &self.stats);
            }
            avsync::PresentDecision::Drop => {
                self.avsync_stats.record_stale_drop();
                self.stats.record_stale_drop();
            }
        }
    }

    /// Presentation-queue pump (§9.1): presents due held frames (an absent
    /// audio clock flushes everything so video never stalls) and logs the
    /// sync stats periodically. Returns `false` when the consumer is gone.
    fn avsync_pump(&mut self) -> bool {
        let audio_now = self.audio_now_ms();
        self.stats.set_avsync(
            u32::try_from(self.present_queue.frames.len()).unwrap_or(u32::MAX),
            audio_now.is_some(),
            i32::try_from(self.avsync_stats.skew_avg_ms()).unwrap_or(i32::MAX),
            i32::try_from(self.avsync_stats.skew_max_abs_ms()).unwrap_or(i32::MAX),
        );
        while let Some(front) = self.present_queue.frames.front() {
            let due = match audio_now {
                Some(now) => front.hold_until <= now,
                None => true,
            };
            if !due {
                break;
            }
            let held = self.present_queue.frames.pop_front().expect("front checked");
            if let Some(now) = audio_now {
                self.avsync_stats
                    .record_present(i64::from(held.hold_until) - i64::from(now));
            }
            if !self.present(held.display_id, &held.frame) {
                return false;
            }
        }
        if self.avsync_log.elapsed() >= SYNC_LOG_INTERVAL {
            info!(
                mode = if audio_now.is_some() { "audio" } else { "no-audio" },
                presented = self.avsync_stats.presented,
                held = self.avsync_stats.held,
                dropped_stale = self.avsync_stats.dropped_stale,
                dropped_overflow = self.avsync_stats.dropped_overflow,
                skew_avg_ms = self.avsync_stats.skew_avg_ms(),
                skew_max_abs_ms = self.avsync_stats.skew_max_abs_ms(),
                "avsync"
            );
            self.avsync_stats.reset();
            self.avsync_log = Instant::now();
        }
        true
    }
}

/// Mutable state shared by the two session loops.
struct Core {
    events: flume::Sender<ClientEvent>,
    displays: Displays,
    reasm: Reassembler,
    /// Decode + presentation work goes to the decode thread (§15.3).
    decode_tx: flume::Sender<DecodeJob>,
    /// Unbounded control channel to the decode thread (Announce only);
    /// control jobs are never droppable.
    decode_ctl_tx: flume::Sender<DecodeJob>,
    /// Decode-error recovery flag, owned by the decode thread (§12.2).
    recovery_awaiting: Arc<AtomicBool>,
    audio: AudioClient,
    mic: MicClient,
    /// Webcam streaming endpoint (§18.57).
    cam: CamClient,
    /// Live stats accumulator shared with the audio thread; snapshots go to
    /// the consumer once per second (§15.3 debug aid).
    stats: std::sync::Arc<crate::stats::SessionStats>,
    /// Rate-limit state for keyframe requests (riesgos §2.1).
    last_key_req: Option<Instant>,
    prev_dropped: u64,
    /// Clipboard wire state (M8).
    clip: protocol::ClipboardSession,
    /// Clipboard messages queued for sending (handle_packet is synchronous;
    /// the receive loops drain this after each packet).
    clip_out: Vec<protocol::ClipboardMessage>,
    /// Storage transfers state (M8, model B); its `out` queue is drained
    /// by the receive loops like `clip_out`.
    storage: crate::storage::StorageClient,
    /// Smartcard request dispatcher (§17); `None` while disabled.
    smartcard: Option<crate::smartcard::SmartcardService>,
    /// Reply channel of the smartcard dispatcher (replies are drained by
    /// `flush_out` onto the reliable stream).
    scard_tx: flume::Sender<protocol::SmartcardMessage>,
    scard_rx: flume::Receiver<protocol::SmartcardMessage>,
    /// Inline smartcard replies produced while the dispatcher is disabled
    /// (`SCARD_E_NO_SERVICE` — a request is never left unanswered).
    scard_out: Vec<protocol::SmartcardMessage>,
    /// Printer job receiver (§18.57 phase 0); `None` while disabled (the
    /// HELLO then carries no `FLAG_PRINTER` and the server stays quiet).
    printer: Option<crate::printer::PrinterJobs>,
}

impl Core {
    fn new(
        events: flume::Sender<ClientEvent>,
        displays: Displays,
        config: &ClientConfig,
        clock_start: Instant,
        cam_source: Option<Box<dyn ::webcam::WebcamSource>>,
        printer: Option<crate::printer::PrinterJobs>,
    ) -> Self {
        let (scard_tx, scard_rx) = flume::unbounded();
        let smartcard = config
            .smartcard
            .then(|| crate::smartcard::SmartcardService::start(crate::smartcard::default_backend(), scard_tx.clone()));
        let stats = std::sync::Arc::new(crate::stats::SessionStats::default());
        let audio = AudioClient::new(config.audio, config.audio_dump.clone(), std::sync::Arc::clone(&stats));
        let (decode_tx, decode_ctl_tx, recovery_awaiting) = DecodeWorker::spawn(
            events.clone(),
            displays.clone(),
            audio.shared_clock(),
            std::sync::Arc::clone(&stats),
        );
        Self {
            events,
            displays,
            reasm: Reassembler::new(),
            decode_tx,
            decode_ctl_tx,
            recovery_awaiting,
            audio,
            mic: MicClient::new(config.mic, clock_start),
            cam: CamClient::new(cam_source),
            stats,
            last_key_req: None,
            prev_dropped: 0,
            clip: protocol::ClipboardSession::new(),
            clip_out: Vec::new(),
            storage: crate::storage::StorageClient::new(),
            smartcard,
            scard_tx,
            scard_rx,
            scard_out: Vec::new(),
            printer,
        }
    }

    /// Applies `ClientCommand::SmartcardEnable`: starts or stops the
    /// request dispatcher (§17). Starting with no PC/SC backend still
    /// answers every request, with `SCARD_E_NO_SERVICE`.
    fn set_smartcard(&mut self, on: bool) {
        match (on, self.smartcard.is_some()) {
            (true, false) => {
                info!("smartcard redirection enabled");
                self.smartcard = Some(crate::smartcard::SmartcardService::start(
                    crate::smartcard::default_backend(),
                    self.scard_tx.clone(),
                ));
            }
            (false, true) => {
                info!("smartcard redirection disabled");
                self.smartcard = None;
            }
            _ => {}
        }
    }

    /// Handles one received packet; returns `false` when the session should
    /// end (consumer gone).
    // One dispatch arm per channel; the packet kinds are the structure.
    #[allow(clippy::too_many_lines)]
    fn handle_packet(&mut self, ch: u16, packet: &[u8]) -> Result<bool> {
        if ch == channel::CURSOR {
            match CursorMessage::decode(packet) {
                Ok(CursorMessage::Visibility { visible }) => {
                    info!(visible, "cursor visibility");
                    if !emit(&self.events, ClientEvent::CursorVisibility { visible }) {
                        return Ok(false);
                    }
                }
                Ok(CursorMessage::Shape {
                    width,
                    height,
                    hot_x,
                    hot_y,
                    pixels,
                }) => {
                    info!(width, height, "cursor shape");
                    if !emit(
                        &self.events,
                        ClientEvent::CursorShape {
                            width,
                            height,
                            hot_x,
                            hot_y,
                            rgba: pixels,
                        },
                    ) {
                        return Ok(false);
                    }
                }
                Err(e) => warn!("bad cursor message: {e}"),
            }
            return Ok(true);
        }
        if ch == channel::CLIPBOARD {
            // Clipboard (M8): offers are pulled automatically; the answers
            // queue in clip_out and the receive loops flush them on the
            // reliable stream after this call.
            use protocol::ClipboardMessage as M;
            match M::decode(packet) {
                Ok(M::Offer { seq, formats }) => {
                    if let Some(req) = self.clip.peer_offer(seq, formats) {
                        self.clip_out.push(req);
                    }
                }
                Ok(M::Request { seq, format }) => {
                    let resp = self.clip.peer_request(seq, format);
                    self.clip_out.push(resp);
                }
                Ok(M::Data {
                    seq,
                    format,
                    payload,
                }) => {
                    if let Some(bytes) = self.clip.peer_data(seq, format, &payload) {
                        if let Ok(text) = String::from_utf8(bytes) {
                            info!(len = text.len(), "clipboard data from remote");
                            if !emit(&self.events, ClientEvent::ClipboardData { text }) {
                                return Ok(false);
                            }
                        } else {
                            warn!("clipboard data is not valid UTF-8");
                        }
                    }
                }
                Ok(M::DataFailed { seq }) => self.clip.peer_data_failed(seq),
                Err(e) => warn!("bad clipboard message: {e}"),
            }
            return Ok(true);
        }
        if ch == channel::STORAGE {
            // Storage (M8, model B): applied to the transfer state; wire
            // answers queue in storage.out and are flushed by the loops.
            match protocol::StorageMessage::decode(packet) {
                Ok(msg) => {
                    for ev in self.storage.handle(&msg) {
                        if !emit(&self.events, ev) {
                            return Ok(false);
                        }
                    }
                }
                Err(e) => warn!("bad storage message: {e}"),
            }
            return Ok(true);
        }
        if ch == channel::PRINTER {
            // Printer (§18.57 phase 0): jobs are written to disk; replies
            // queue in the receiver and are flushed by the loops.
            match protocol::PrinterMessage::decode(packet) {
                Ok(msg) => {
                    if let Some(printer) = &mut self.printer {
                        for ev in printer.handle(&msg) {
                            if !emit(&self.events, ev) {
                                return Ok(false);
                            }
                        }
                    } else {
                        warn!("printer job received with the channel disabled; dropped");
                    }
                }
                Err(e) => warn!("bad printer message: {e}"),
            }
            return Ok(true);
        }
        if ch == channel::SMARTCARD {
            // Smartcard (§17): requests go to the dispatcher service
            // (replies flow back through scard_rx and are flushed by the
            // loops). With no service the request is still answered —
            // SCARD_E_NO_SERVICE — because an unanswered request hangs
            // the server side.
            match protocol::SmartcardMessage::decode(packet) {
                Ok(msg) if msg.is_request() => {
                    if let Some(service) = &self.smartcard {
                        service.enqueue(msg);
                    } else {
                        self.scard_out.push(protocol::SmartcardMessage::Error {
                            request_id: msg.request_id(),
                            code: smartcard::SCARD_E_NO_SERVICE,
                        });
                    }
                }
                Ok(msg) => warn!(?msg, "unexpected smartcard reply from the server"),
                Err(e) => warn!("bad smartcard message: {e}"),
            }
            return Ok(true);
        }
        if ch == channel::AUDIO_OUT {
            // Audio (§9): the audio thread owns jitter buffer, decoder and
            // playback; the receive loop only forwards raw payloads.
            self.audio.feed(packet);
            return Ok(true);
        }
        if ch == channel::CONTROL {
            self.handle_control(packet);
            return Ok(true);
        }
        if ch == channel::DISPLAY
            && let Some(complete) = self.reasm.push(packet)?
        {
            // Decode + presentation run on the decode thread (§15.3); this
            // task only reassembles, so audio and probes never wait behind
            // a video decode. A full queue means the consumer cannot keep
            // up: drop the frame and mark the stream broken so the loop
            // asks for a keyframe (same handling as a loss, §12.2). A
            // disconnected queue means the decode worker died (its panic
            // is logged by the panic hook): the session cannot present
            // video ever again, so end it with a visible error instead of
            // leaving a black window.
            match self.decode_tx.try_send(DecodeJob::Frame(complete)) {
                Ok(()) => {}
                Err(flume::TrySendError::Full(_)) => {
                    self.recovery_awaiting.store(true, Ordering::Relaxed);
                    self.stats.record_stale_drop();
                }
                Err(flume::TrySendError::Disconnected(_)) => {
                    anyhow::bail!("decode worker died; ending session");
                }
            }
        }
        Ok(true)
    }

    /// Handles one control-channel message (announcements, §8.4/§9/§9.2).
    #[allow(clippy::too_many_lines)] // a flat dispatch over the control vocabulary
    fn handle_control(&mut self, packet: &[u8]) {
        match ControlMessage::decode(packet) {
            Ok(ControlMessage::DisplayInfo {
                display_id,
                width,
                height,
                codec,
            }) => {
                let codec_type = VideoCodecType::from_wire_id(codec);
                if let Some(codec_type) = codec_type {
                    info!(display_id, width, height, codec, "display announced");
                    // The decoder is built on the decode thread (§15.3);
                    // the framebuffer exists from the announcement on so
                    // the consumer can fetch it immediately.
                    self.displays.get_or_insert(display_id);
                    if self
                        .decode_ctl_tx
                        .send(DecodeJob::Announce { display_id, codec: codec_type })
                        .is_err()
                    {
                        warn!("decode worker gone; DisplayInfo lost");
                    }
                    let _ = emit(
                        &self.events,
                        ClientEvent::DisplayInfo {
                            display_id,
                            width,
                            height,
                            codec: codec_type,
                        },
                    );
                } else {
                    warn!(codec, "unknown codec id; frames will be dropped");
                    let _ = emit(
                        &self.events,
                        ClientEvent::Error(format!("unknown video codec id {codec}")),
                    );
                }
            }
            Ok(ControlMessage::AudioConfig {
                codec,
                sample_rate,
                channels,
                bitrate_kbps,
                frame_ms,
                fec,
            }) => {
                self.audio.configure(AudioSpec {
                    codec,
                    sample_rate,
                    channels,
                    bitrate_kbps,
                    frame_ms,
                    fec,
                });
            }
            Ok(ControlMessage::AudioInConfig {
                codec,
                sample_rate,
                channels,
                bitrate_kbps,
                frame_ms,
                fec,
            }) => {
                // Audio-in support (§9.2): the server has a real mic sink;
                // capture starts now (no packets are sent before this).
                self.mic.enable(&AudioSpec {
                    codec,
                    sample_rate,
                    channels,
                    bitrate_kbps,
                    frame_ms,
                    fec,
                });
            }
            Ok(ControlMessage::WebcamConfig {
                codec,
                width,
                height,
                fps,
                bitrate_kbps,
            }) => {
                // §18.57: the server picked the webcam wire format.
                info!(codec, width, height, fps, bitrate_kbps, "webcam configured by server");
                self.cam.configure(crate::webcam::CamSpec {
                    codec,
                    width,
                    height,
                    fps,
                    bitrate_kbps,
                });
            }
            Ok(ControlMessage::WebcamStart) => {
                info!("webcam stream requested by server");
                self.cam.start();
            }
            Ok(ControlMessage::WebcamStop) => {
                info!("webcam stream stopped by server");
                self.cam.stop();
            }
            Ok(ControlMessage::WebcamRequestKeyframe) => {
                // §18.57: the server lost webcam fragments; resync the VP9
                // stream (the encoder thread rate-limits via the flag).
                self.cam.request_keyframe();
            }
            Ok(ControlMessage::SecureDesktop { active }) => {
                // §18.41: the console switched to/from a secure desktop
                // (UAC, lock, Winlogon); surface it so the GUI can explain
                // the lower frame rate and sanitized input.
                info!(active, "secure desktop transition");
                let _ = emit(&self.events, ClientEvent::SecureDesktop { active });
            }
            _ => {}
        }
    }
}
