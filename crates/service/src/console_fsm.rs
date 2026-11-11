// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Console state machine (§18.55): the SINGLE owner of every console side
//! effect in the service.
//!
//! Design (agreed 2026-11-04): one manager task drives an explicit state
//! machine fed by events over a flume channel:
//!
//! - [`Event::Snapshot`]: a 500 ms poller reports the console state
//!   ([`console::ConsoleState`]) and the secure-desktop flag. Snapshots are
//!   also the machine's tick: every wait is "transition X or timeout",
//!   never a free-running `sleep` loop.
//! - [`Event::ClientValidated`]: the listener, after a successful handshake
//!   and ticket validation, hands the connection over. The manager drives
//!   the whole pre-session sequence: console-conflict resolution
//!   (§18.53), autologon, agent spawn and handoff.
//!
//! Orthogonal signals are NOT states: `secure_desktop` (a UAC prompt can
//! appear inside a user session) and the live-agent flag.
//!
//! The machine also owns the surrounding supervision that used to live in
//! the watch loops: secure-helper lifecycle, auto-unlock (lock screen with
//! a live client → re-arm the credential provider + SAS), agent watchdog,
//! the legacy proactive-spawn mode (no validating listener) and the lab
//! secure-desktop probe.
//!
//! Future (not now, do not mix): replace the poller with passive
//! `WTSRegisterSessionNotification` events via a hidden window.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{error, info, warn};
use transport::{EncryptedStream, TcpTransport};
use udscredprov::logic::Credentials;

use protocol::ControlMessage;
use protocol::channel;
use protocol::{
    CONSOLE_STATE_ACTIVE, CONSOLE_STATE_LOCKED, SESSION_END_CONSOLE_BUSY,
    SESSION_END_DENIED_BY_USER, SESSION_STAGE_AUTOLOGON, SESSION_STAGE_EVICTING,
};

use crate::agent::{Agent, OwnedHandle};
use crate::console::{Account, ConflictConfig, ConsoleState};
use crate::listener::AgentSlot;

/// Poller cadence: console-state snapshots (and therefore the machine's
/// tick) every 500 ms.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long to wait for the autologon to produce a console session.
const LOGON_TIMEOUT: Duration = Duration::from_secs(60);

/// SAS re-send cadence while an autologon is in flight: the first SAS can
/// land while winlogon is still building the logon session (e.g. right
/// after an eviction) and is lost (§18.53 lesson of 2026-11-04).
const SAS_RETRY: Duration = Duration::from_secs(5);

/// Maximum synthetic SAS per lock episode (auto-unlock, §18.41/§18.52).
const UNLOCK_MAX_SAS: u32 = 2;

/// Minimum interval between synthetic SAS attempts in legacy mode while
/// waiting for an interactive session (the logon takes several seconds;
/// re-sending faster just strobes the secure screen).
const LEGACY_SAS_COOLDOWN: Duration = Duration::from_secs(15);

/// Maximum agent respawns per service run in legacy mode (crash-loop
/// guard).
const LEGACY_MAX_RESPAWNS: u32 = 5;

/// Events feeding the state machine.
pub enum Event {
    /// Poller snapshot: console state + secure-desktop flag.
    Snapshot(Snapshot),
    /// The listener validated a connection (handshake + ticket) and hands
    /// it over for the pre-session sequence. Boxed: it carries the stream.
    ClientValidated(Box<ClientValidated>),
}

/// One console snapshot from the poller.
pub struct Snapshot {
    /// Console state at poll time.
    pub console: ConsoleState,
    /// True while a secure desktop (UAC, Winlogon) owns the console input.
    pub secure_desktop: bool,
}

/// A connection that passed validation and awaits its session.
pub struct ClientValidated {
    /// Encrypted control stream (post-handshake).
    pub stream: EncryptedStream<TcpTransport>,
    /// Service runtime (standalone or broker mode).
    pub runtime: Arc<crate::standalone::Runtime>,
    /// Broker grant (pq mode only).
    pub grant: Option<crate::broker::SessionGrant>,
    /// Session autologon credentials, when configured.
    pub creds: Option<Credentials>,
    /// Console-conflict configuration for this session.
    pub conflict: ConflictConfig,
}

/// Phases of a pending (validated, not yet handed off) session.
enum Phase {
    /// Just arrived: evaluate the console conflict on the next drive.
    DecideConflict,
    /// Policy `ask`: waiting for the client's `ConsoleConflictReply`.
    AwaitReply,
    /// Eviction fired: waiting for the console to leave `owner_id`.
    Evicting {
        /// Session id being evicted.
        owner_id: u32,
    },
    /// Autologon armed: waiting for a console session with a user.
    Autologon,
}

/// A validated connection working through the pre-session phases.
struct Pending {
    stream: EncryptedStream<TcpTransport>,
    runtime: Arc<crate::standalone::Runtime>,
    grant: Option<crate::broker::SessionGrant>,
    creds: Option<Credentials>,
    conflict: ConflictConfig,
    phase: Phase,
    /// Current phase deadline (ask timeout, settle, logon).
    deadline: Instant,
    /// Last SAS sent while in [`Phase::Autologon`].
    last_sas: Instant,
}

/// Service mode: validating listener (standalone/pq) or legacy proactive
/// spawn (the agent itself listens; the service only keeps it alive).
enum Mode {
    /// `listener.rs` owns the TCP endpoint; sessions arrive as events.
    Listener,
    /// No listener: the manager keeps one agent alive in the console.
    Legacy {
        /// Fixed agent configuration for this service run.
        config: protocol::service_ipc::AgentConfig,
        /// The proactive agent, if alive.
        agent: Option<Agent>,
        /// Respawns so far (crash-loop cap).
        respawns: u32,
        /// True once the crash-loop cap was hit (log once, then stay idle).
        gave_up: bool,
        /// Last synthetic SAS (cooldown).
        last_sas: Option<Instant>,
        /// Last spawn attempt (backoff between failed attempts).
        last_attempt: Option<Instant>,
    },
}

/// Entry point replacing the old watch loops: pre-creates the session
/// mutex, starts the poller and (in listener modes) the validating
/// listener, then runs the manager until `stop`.
pub fn run(
    stop: &'static AtomicBool,
    runtime: Option<crate::standalone::Runtime>,
    creds_slot: Arc<Mutex<Option<Credentials>>>,
) {
    // Pre-create the global session mutex before any agent spawns: only a
    // privileged process can create Global\ objects (see agent.rs).
    let _session_mutex = crate::agent::ensure_session_mutex();
    let (tx, rx) = flume::bounded::<Event>(64);
    spawn_poller(stop, tx.clone());
    let slot = Arc::new(AgentSlot::default());
    let mode = match runtime {
        Some(runtime) => {
            let runtime = Arc::new(runtime);
            crate::listener::spawn(
                stop,
                Arc::clone(&runtime),
                Arc::clone(&slot),
                Arc::clone(&creds_slot),
                tx,
            );
            Mode::Listener
        }
        None => Mode::Legacy {
            config: legacy_config(),
            agent: None,
            respawns: 0,
            gave_up: false,
            last_sas: None,
            last_attempt: None,
        },
    };
    let tokio_rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            error!(error = %format!("{err:#}"), "cannot create the console manager runtime");
            return;
        }
    };
    tokio_rt.block_on(manager_loop(stop, rx, mode, slot, creds_slot));
}

/// Builds the fixed agent configuration of legacy mode (random key +
/// ticket per service run, handed over through the agent's stdin pipe).
fn legacy_config() -> protocol::service_ipc::AgentConfig {
    let mut key = [0u8; 32];
    let mut ticket = [0u8; 48];
    if let Err(err) = getrandom::fill(&mut key).and_then(|()| getrandom::fill(&mut ticket)) {
        error!(error = %format!("{err:#}"), "cannot generate session credentials");
    }
    protocol::service_ipc::AgentConfig {
        key,
        ticket,
        bind: "0.0.0.0:14433".to_owned(),
        output_name: String::new(),
        preset: "ladder".to_owned(),
        ladder: String::new(),
        fps_adapt: String::new(),
        stats_csv: String::new(),
        cam_sink: String::new(),
        mic_sink: String::new(),
        quality: String::new(),
        encoder: String::new(),
        fixed_resolution: (0, 0),
        allow_lock: false,
        wallpaper_off: false,
        // 0 = the agent's §18.78 defaults; legacy mode has no config file.
        spool_quota_mb: 0,
        clipboard_max_kb: 0,
        input_events_per_sec: 0,
        handoff: None,
    }
}

/// The poller thread: every [`POLL_INTERVAL`] reports the console state and
/// the secure-desktop flag. Best-effort (`try_send`): a full channel just
/// delays one tick.
fn spawn_poller(stop: &'static AtomicBool, tx: flume::Sender<Event>) {
    std::thread::Builder::new()
        .name("udsrp-console-poller".into())
        .spawn(move || {
            let secure_event = crate::agent::ensure_secure_event();
            while !stop.load(Ordering::Relaxed) {
                let snapshot = Snapshot {
                    console: crate::console::console_state(),
                    secure_desktop: secure_event
                        .as_ref()
                        .is_some_and(crate::agent::secure_event_set),
                };
                let _ = tx.try_send(Event::Snapshot(snapshot));
                std::thread::sleep(POLL_INTERVAL);
            }
        })
        .expect("cannot spawn the console poller thread");
}

/// The manager loop: events drive the machine; the client's reply is only
/// read while a conflict ask is in flight; a fallback tick covers timeout
/// checks even if the poller dies.
async fn manager_loop(
    stop: &'static AtomicBool,
    rx: flume::Receiver<Event>,
    mode: Mode,
    slot: Arc<AgentSlot>,
    creds_slot: Arc<Mutex<Option<Credentials>>>,
) {
    let mut manager = Manager {
        mode,
        slot,
        creds_slot,
        console: crate::console::console_state(),
        pending: None,
        helper: None,
        unlock_armed: false,
        unlock_sas_sent: 0,
    };
    info!(state = ?manager.console, "console state machine up");
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        if matches!(
            manager.pending.as_ref().map(|p| &p.phase),
            Some(Phase::AwaitReply)
        ) {
            // Awaiting the client's conflict answer: read its stream too.
            let pending = manager.pending.as_mut().expect("checked above");
            tokio::select! {
                event = rx.recv_async() => {
                    match event {
                        Ok(event) => manager.handle_event(event).await,
                        Err(_) => break,
                    }
                }
                msg = pending.stream.recv_into(&mut buf) => {
                    let n = msg.map(|(ch, n)| (ch, n));
                    manager.handle_conflict_reply(n, &buf).await;
                }
                _ = tokio::time::sleep(POLL_INTERVAL) => manager.check_timeouts().await,
            }
        } else {
            tokio::select! {
                event = rx.recv_async() => {
                    match event {
                        Ok(event) => manager.handle_event(event).await,
                        Err(_) => break,
                    }
                }
                _ = tokio::time::sleep(POLL_INTERVAL) => manager.check_timeouts().await,
            }
        }
    }
    manager.shutdown();
}

/// The console state machine.
struct Manager {
    mode: Mode,
    slot: Arc<AgentSlot>,
    creds_slot: Arc<Mutex<Option<Credentials>>>,
    /// Last known console state (updated by snapshots).
    console: ConsoleState,
    /// Connection working through the pre-session phases.
    pending: Option<Pending>,
    /// Running secure-desktop helper, if any.
    helper: Option<(u32, OwnedHandle)>,
    /// Auto-unlock: credprov re-armed for the current lock episode.
    unlock_armed: bool,
    /// Auto-unlock: SAS sent in the current lock episode.
    unlock_sas_sent: u32,
}

impl Manager {
    async fn handle_event(&mut self, event: Event) {
        match event {
            Event::Snapshot(snapshot) => self.on_snapshot(snapshot).await,
            Event::ClientValidated(client) => self.on_client_validated(*client).await,
        }
    }

    /// A validated connection arrives: reject when busy, otherwise start
    /// the pre-session phases.
    async fn on_client_validated(&mut self, client: ClientValidated) {
        if self.pending.is_some() {
            // The listener accepts sequentially, but a slow pre-session can
            // overlap a fresh connection: one negotiation at a time.
            warn!("a session negotiation is already in progress; dropping the new connection");
            return;
        }
        if self.slot.has_live_agent() {
            // SESSION_BUSY (§18.38): one interactive session per machine.
            warn!("a session agent is already serving a client; dropping the connection");
            return;
        }
        info!(
            mode = crate::listener::mode_name(&client.runtime),
            "handshake validated (possession proof passed)"
        );
        self.pending = Some(Pending {
            stream: client.stream,
            runtime: client.runtime,
            grant: client.grant,
            creds: client.creds,
            conflict: client.conflict,
            phase: Phase::DecideConflict,
            deadline: Instant::now(),
            last_sas: Instant::now(),
        });
        self.drive_pending().await;
    }

    /// Advances the pending connection through the phases that need no
    /// waiting (conflict decision, eviction start, autologon start,
    /// completion). Waiting phases are driven by snapshots and timeouts.
    async fn drive_pending(&mut self) {
        let Some(pending) = &self.pending else { return };
        if !matches!(pending.phase, Phase::DecideConflict) {
            return;
        }
        // Console-conflict policy (§18.53), server-mandatory: another
        // account owning the console is resolved BEFORE any side effect.
        // Without credentials there is no identity to compare and the
        // existing console is simply mirrored (no conflict).
        let conflict = match (&self.console, &pending.creds) {
            (
                ConsoleState::Session {
                    id,
                    account,
                    locked,
                },
                Some(creds),
            ) if !crate::console::same_account(account, creds) => {
                Some((*id, account.clone(), *locked))
            }
            _ => None,
        };
        let Some((owner_id, account, locked)) = conflict else {
            self.enter_autologon_or_ready().await;
            return;
        };
        info!(
            console_user = %account.display(),
            session_user = %pending.creds.as_ref().map(|c| format!("{}\\{}", c.domain, c.username)).unwrap_or_default(),
            locked,
            policy = ?pending.conflict.policy,
            "console conflict: another account owns the console"
        );
        match pending.conflict.policy {
            server_config::ConsoleConflictPolicy::TakeOver => {
                self.start_eviction(owner_id, &account).await;
            }
            server_config::ConsoleConflictPolicy::Deny => {
                let Some(mut pending) = self.pending.take() else {
                    return;
                };
                crate::console::send_session_end(&mut pending.stream, SESSION_END_CONSOLE_BUSY)
                    .await;
                info!("console busy (owned by another user); policy is deny");
            }
            server_config::ConsoleConflictPolicy::Ask => {
                let query = ControlMessage::ConsoleConflict {
                    user: pending.conflict.reveal_user.then(|| account.display()),
                    state: if locked {
                        CONSOLE_STATE_LOCKED
                    } else {
                        CONSOLE_STATE_ACTIVE
                    },
                };
                let timeout = pending.conflict.timeout;
                let Some(pending) = &mut self.pending else {
                    return;
                };
                match query.encode() {
                    Ok(encoded) => {
                        if let Err(err) = pending.stream.send(channel::CONTROL, &encoded).await {
                            warn!(error = %format!("{err:#}"), "cannot send the conflict query; dropping");
                            self.pending = None;
                            return;
                        }
                        pending.phase = Phase::AwaitReply;
                        pending.deadline = Instant::now() + timeout;
                    }
                    Err(err) => {
                        warn!(error = %format!("{err:#}"), "cannot encode the conflict query; dropping");
                        self.pending = None;
                    }
                }
            }
        }
    }

    /// The client's `ConsoleConflictReply` (or any other message / stream
    /// error) while in [`Phase::AwaitReply`].
    async fn handle_conflict_reply(
        &mut self,
        msg: std::result::Result<(u16, usize), transport::TransportError>,
        buf: &[u8],
    ) {
        let take_over = match msg {
            Ok((ch, n)) if ch == channel::CONTROL => {
                match ControlMessage::decode(&buf[..n]) {
                    Ok(ControlMessage::ConsoleConflictReply { take_over }) => take_over,
                    // Not part of the pre-session vocabulary; keep waiting.
                    _ => return,
                }
            }
            Ok(_) => return,
            Err(err) => {
                warn!(error = %format!("{err:#}"), "stream error during the conflict ask; dropping");
                self.pending = None;
                return;
            }
        };
        info!(take_over, "console conflict answered by the client");
        if take_over {
            // Re-derive the owner (it cannot change while we hold the only
            // negotiation) and evict.
            let owner = match &self.console {
                ConsoleState::Session { id, account, .. } => Some((*id, account.clone())),
                _ => None,
            };
            match owner {
                Some((owner_id, account)) => self.start_eviction(owner_id, &account).await,
                None => self.enter_autologon_or_ready().await,
            }
        } else {
            let Some(mut pending) = self.pending.take() else {
                return;
            };
            crate::console::send_session_end(&mut pending.stream, SESSION_END_DENIED_BY_USER).await;
            info!("console takeover refused by the connecting client");
        }
    }

    /// Fires the eviction and arms the settle wait.
    async fn start_eviction(&mut self, owner_id: u32, account: &Account) {
        let Some(pending) = &mut self.pending else {
            return;
        };
        // The eviction can take a few seconds; tell the client what the
        // wait is about so it does not read it as a hang.
        crate::console::send_progress(&mut pending.stream, SESSION_STAGE_EVICTING).await;
        match crate::console::evict_start(owner_id, account, pending.conflict.takeover) {
            Ok(()) => {
                pending.phase = Phase::Evicting { owner_id };
                pending.deadline = Instant::now() + crate::console::EVICT_SETTLE_TIMEOUT;
            }
            Err(err) => {
                warn!(error = %format!("{err:#}"), "eviction failed; dropping the connection");
                self.pending = None;
            }
        }
    }

    /// Enters the autologon phase (no console user) or completes the
    /// session (console up).
    async fn enter_autologon_or_ready(&mut self) {
        if matches!(self.console, ConsoleState::Session { .. }) {
            self.complete_pending().await;
            return;
        }
        let Some(pending) = &mut self.pending else {
            return;
        };
        let Some(creds) = &pending.creds else {
            warn!("no console session and no autologon credentials configured; dropping");
            self.pending = None;
            return;
        };
        info!(user = %creds.username, "no console session; arming autologon and sending SAS");
        // The sign-in takes a few seconds; tell the client.
        crate::console::send_progress(&mut pending.stream, SESSION_STAGE_AUTOLOGON).await;
        if let Err(err) = std::fs::write(crate::credprov::ARM_PATH, b"") {
            warn!(error = %format!("{err:#}"), "cannot create the autologon arm flag; dropping");
            self.pending = None;
            return;
        }
        if let Err(err) = crate::sas::send_sas() {
            warn!(error = %format!("{err:#}"), "synthetic SAS failed");
        }
        pending.phase = Phase::Autologon;
        pending.deadline = Instant::now() + LOGON_TIMEOUT;
        pending.last_sas = Instant::now();
    }

    /// Console up and conflict resolved: hand the connection to a fresh
    /// session agent.
    async fn complete_pending(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if let Err(err) = crate::listener::complete_session(
            pending.stream,
            &pending.runtime,
            pending.grant,
            &self.slot,
            &self.creds_slot,
        )
        .await
        {
            warn!(error = %format!("{err:#}"), "connection failed at handoff");
        }
    }

    /// Snapshot processing: state transitions, supervision, and the
    /// waiting phases of a pending connection.
    async fn on_snapshot(&mut self, snapshot: Snapshot) {
        let previous = std::mem::replace(&mut self.console, snapshot.console);
        if previous != self.console {
            info!(from = ?previous, to = ?self.console, "console transition");
        }
        self.supervise(snapshot.secure_desktop);
        self.drive_pending_wait().await;
        self.legacy_upkeep();
        // Lab trigger (§18.41): one secure-desktop probe per flag file.
        if std::path::Path::new(crate::agent::PROBE_FLAG).exists() {
            let _ = std::fs::remove_file(crate::agent::PROBE_FLAG);
            match crate::agent::spawn_probe_in_console() {
                Ok(Some(pid)) => info!(pid, "secure-desktop probe spawned"),
                Ok(None) => info!("secure-desktop probe requested but there is no console session"),
                Err(err) => {
                    warn!(error = %format!("{err:#}"), "cannot spawn the secure-desktop probe")
                }
            }
        }
    }

    /// The waiting phases of a pending connection, driven by snapshots.
    async fn drive_pending_wait(&mut self) {
        let Some(pending) = &self.pending else { return };
        match pending.phase {
            Phase::Evicting { owner_id } => {
                let still_there = matches!(
                    &self.console,
                    ConsoleState::Session { id, .. } if *id == owner_id
                );
                if !still_there {
                    self.enter_autologon_or_ready().await;
                }
            }
            Phase::Autologon => {
                if matches!(self.console, ConsoleState::Session { .. }) {
                    info!("console session up after the autologon");
                    self.complete_pending().await;
                    return;
                }
                if pending.last_sas.elapsed() >= SAS_RETRY {
                    if let Err(err) = crate::sas::send_sas() {
                        warn!(error = %format!("{err:#}"), "SAS retry failed");
                    }
                    let Some(pending) = &mut self.pending else {
                        return;
                    };
                    pending.last_sas = Instant::now();
                }
            }
            _ => {}
        }
    }

    /// Phase deadlines: ask timeout (fail-closed → deny), eviction settle,
    /// autologon timeout. Called from snapshots and the fallback tick.
    async fn check_timeouts(&mut self) {
        let Some(pending) = &self.pending else { return };
        if Instant::now() < pending.deadline {
            return;
        }
        match pending.phase {
            Phase::AwaitReply => {
                info!("console conflict ask timed out; denying");
                let Some(mut pending) = self.pending.take() else {
                    return;
                };
                crate::console::send_session_end(&mut pending.stream, SESSION_END_DENIED_BY_USER)
                    .await;
            }
            Phase::Evicting { owner_id } => {
                warn!(
                    session_id = owner_id,
                    "console still active after the eviction; dropping"
                );
                self.pending = None;
            }
            Phase::Autologon => {
                warn!("autologon did not produce a console session in time; dropping");
                self.pending = None;
            }
            Phase::DecideConflict => {}
        }
    }

    /// Per-tick supervision: agent slot cleanup, secure-helper lifecycle,
    /// auto-unlock on lock transitions with a live client.
    fn supervise(&mut self, secure_desktop: bool) {
        self.slot.clear_if_dead();
        self.slot.kill_if_unhealthy();
        crate::agent::supervise_secure_helper(secure_desktop, &mut self.helper);
        // Auto-unlock (§18.41/§18.52): on a LOCKED console (a UAC prompt is
        // not a lock), re-arm the credential provider so the session unlocks
        // by itself while we hold its creds, and nudge LogonUI with a
        // synthetic SAS — at most UNLOCK_MAX_SAS per lock episode, and only
        // while a client session is live (never at boot or idle).
        let locked = matches!(self.console, ConsoleState::Session { locked: true, .. });
        if !locked {
            self.unlock_armed = false;
            self.unlock_sas_sent = 0;
        } else if self.slot.has_live_agent() {
            if !self.unlock_armed {
                self.unlock_armed = true;
                crate::agent::maybe_arm_auto_unlock(&self.creds_slot);
            }
            if self.unlock_sas_sent < UNLOCK_MAX_SAS {
                match crate::sas::send_sas() {
                    Ok(()) => {
                        info!("console locked; synthetic SAS sent to reach the credential prompt")
                    }
                    Err(err) => warn!(error = %format!("{err:#}"), "cannot send the synthetic SAS"),
                }
                self.unlock_sas_sent += 1;
            }
        }
    }

    /// Legacy mode (no validating listener): keep one agent alive in the
    /// console session, with the crash-loop cap and the SAS nudge towards
    /// the logon screen. Runs on every snapshot tick.
    fn legacy_upkeep(&mut self) {
        let Mode::Legacy {
            config,
            agent,
            respawns,
            gave_up,
            last_sas,
            last_attempt,
        } = &mut self.mode
        else {
            return;
        };
        // Unattended auto-logon nudge (see sas.rs), gated by the one-shot
        // arm flag: only while a logon is armed do we nudge LogonUI with a
        // synthetic SAS; without CAD satisfied it never enumerates
        // providers. Without the flag the machine boots to a plain logon
        // screen and stays there.
        let nudge_towards_logon = |last_sas: &mut Option<Instant>| {
            let armed = std::path::Path::new(crate::credprov::CREDS_PATH).exists()
                && std::path::Path::new(crate::credprov::ARM_PATH).exists()
                && last_sas.is_none_or(|t| t.elapsed() >= LEGACY_SAS_COOLDOWN);
            if armed {
                match crate::sas::send_sas() {
                    Ok(()) => info!("armed with no interactive session; synthetic SAS sent"),
                    Err(err) => warn!(error = %format!("{err:#}"), "cannot send the synthetic SAS"),
                }
                *last_sas = Some(Instant::now());
            }
        };
        match agent {
            Some(a) if !a.is_alive() => {
                warn!("session agent exited; cleaning up and respawning");
                *agent = None;
            }
            Some(a) if !a.mutex_healthy() => {
                error!("session agent lost its mutex (wedged); killing it");
                a.kill();
                *agent = None;
            }
            Some(_) => {}
            None => {
                if *respawns >= LEGACY_MAX_RESPAWNS {
                    // Crash-loop guard: log once, then stay idle until the
                    // service restarts.
                    if !*gave_up {
                        *gave_up = true;
                        error!(
                            LEGACY_MAX_RESPAWNS,
                            "agent respawn limit reached; giving up until service restart"
                        );
                    }
                    return;
                }
                // Backoff between spawn attempts (the old loop slept 2-5 s).
                if last_attempt.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                    return;
                }
                *last_attempt = Some(Instant::now());
                match Agent::spawn_in_console_session(config) {
                    Ok(Some(spawned)) => {
                        *respawns += 1;
                        *agent = Some(spawned);
                    }
                    Ok(None) => nudge_towards_logon(last_sas), // no console session yet
                    Err(err) => {
                        error!(error = %format!("{err:#}"), "cannot spawn the session agent");
                        nudge_towards_logon(last_sas);
                    }
                }
            }
        }
    }

    /// Service stop: never leave helpers or agents behind.
    fn shutdown(&mut self) {
        if let Some((_, process)) = self.helper.take() {
            // SAFETY: the process handle is ours and open.
            unsafe {
                let _ = windows::Win32::System::Threading::TerminateProcess(process.raw(), 0);
            }
        }
        if let Mode::Legacy {
            agent: Some(agent), ..
        } = &self.mode
        {
            info!("service stopping: terminating the session agent");
            agent.kill();
        }
        self.slot.kill();
        info!("console state machine stopped");
    }
}
