// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Validating TCP listener + socket handoff (§18.32).
//!
//! In standalone mode the service (session 0, present since boot) owns the
//! public TCP endpoint. For each connection it completes the encrypted
//! handshake — which since protocol v4 includes the possession proof, so a
//! peer that does not hold the PSK never gets past this point — and only
//! then triggers side effects: the autologon (SAS + credentials push) when
//! no console session exists yet, and the session-agent spawn with the
//! connected socket handed off via `WSADuplicateSocketW`.
//!
//! The UDP leg is not proxied: the agent binds UDP directly and the client
//! finds it through the `UdpOffer` exchange after the handoff.

// The `windows` crate FFI methods take raw pointers; implicit borrows are
// the idiomatic call style for that API (same policy as agent.rs).
#![allow(clippy::borrow_as_ptr)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use broker::ConnectionProvider as _;
use tracing::{error, info, warn};
use transport::{EncryptedStream, TcpTransport};
use udscredprov::logic::Credentials;
use windows::Win32::Networking::WinSock::{WSAPROTOCOL_INFOW, WSADuplicateSocketW};

use crate::agent::Agent;

/// Shared slot for the live session agent (and its handed-off socket, kept
/// alive until the agent has surely materialized its own copy).
#[derive(Default)]
pub struct AgentSlot {
    inner: Mutex<Option<(Agent, Option<std::net::TcpStream>)>>,
}

impl AgentSlot {
    /// Stores a freshly spawned agent.
    pub fn set(&self, agent: Agent, socket: std::net::TcpStream) {
        *self.inner.lock().expect("agent slot poisoned") = Some((agent, Some(socket)));
    }

    /// Replaces the slot contents, returning the previous agent (already
    /// dead or about to be replaced after a wedge).
    pub fn take(&self) -> Option<(Agent, Option<std::net::TcpStream>)> {
        self.inner.lock().expect("agent slot poisoned").take()
    }

    /// Kills and clears the current agent, if any (service shutdown).
    pub fn kill(&self) {
        if let Some((agent, _)) = self.take() {
            info!(pid = agent.pid, "service stopping: terminating the session agent");
            agent.kill();
        }
    }

    /// Whether a live agent currently occupies the session.
    pub fn has_live_agent(&self) -> bool {
        match self.inner.lock().expect("agent slot poisoned").as_ref() {
            Some((agent, _)) => agent.is_alive(),
            None => false,
        }
    }

    /// Drops the slot when the agent died on its own (session ended).
    pub fn clear_if_dead(&self) {
        let mut guard = self.inner.lock().expect("agent slot poisoned");
        if let Some((agent, _)) = guard.as_ref()
            && !agent.is_alive()
        {
            info!(pid = agent.pid, "session agent exited; clearing the slot");
            *guard = None;
        }
    }

    /// Kills the agent when it lost its session mutex (wedged).
    pub fn kill_if_unhealthy(&self) {
        let unhealthy = {
            let guard = self.inner.lock().expect("agent slot poisoned");
            matches!(guard.as_ref(), Some((agent, _)) if !agent.mutex_healthy())
        };
        if unhealthy {
            if let Some((agent, _)) = self.take() {
                error!(pid = agent.pid, "session agent lost its mutex (wedged); killing it");
                agent.kill();
            }
        }
    }
}

/// Starts the validating listener on a dedicated thread with its own
/// current-thread tokio runtime (the service itself is sync).
pub fn spawn(
    stop: &'static AtomicBool,
    runtime: crate::standalone::Runtime,
    slot: Arc<AgentSlot>,
    creds_slot: Arc<Mutex<Option<Credentials>>>,
) {
    std::thread::Builder::new()
        .name("udsrp-listener".into())
        .spawn(move || {
            let runtime_tokio = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    error!(error = %format!("{err:#}"), "cannot create the listener runtime");
                    return;
                }
            };
            runtime_tokio.block_on(accept_loop(stop, runtime, slot, creds_slot));
        })
        .expect("cannot spawn the listener thread");
}

async fn accept_loop(
    stop: &AtomicBool,
    runtime: crate::standalone::Runtime,
    slot: Arc<AgentSlot>,
    creds_slot: Arc<Mutex<Option<Credentials>>>,
) {
    let bind = match &runtime {
        crate::standalone::Runtime::Standalone(standalone) => &standalone.bind,
        crate::standalone::Runtime::Pq(pq) => &pq.bind,
    };
    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(listener) => listener,
        Err(err) => {
            error!(error = %format!("{err:#}"), %bind, "cannot bind the service listener");
            return;
        }
    };
    info!(%bind, "service listener up (validating; handoff to session agent)");
    while !stop.load(Ordering::Relaxed) {
        let accepted = tokio::select! {
            _ = wait_for_stop(stop) => break,
            accepted = listener.accept() => accepted,
        };
        let (socket, peer) = match accepted {
            Ok(pair) => pair,
            Err(err) => {
                warn!(error = %format!("{err:#}"), "accept failed");
                continue;
            }
        };
        info!(%peer, "connection accepted by the service");
        let runtime = &runtime;
        let slot = &slot;
        let creds_slot = &creds_slot;
        // Sequential on purpose (one session per machine): a second client
        // waits until the current attempt resolves.
        if let Err(err) = handle_connection(socket, peer, runtime, slot, creds_slot).await {
            warn!(%peer, error = %format!("{err:#}"), "connection rejected before handoff");
        }
    }
    info!("service listener stopped");
}

async fn wait_for_stop(stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Handshake timeout: a stalled or probing peer must not hold the session.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to wait for the autologon to produce a console session.
const LOGON_TIMEOUT: Duration = Duration::from_secs(60);

async fn handle_connection(
    socket: tokio::net::TcpStream,
    peer: std::net::SocketAddr,
    runtime: &crate::standalone::Runtime,
    slot: &Arc<AgentSlot>,
    creds_slot: &Arc<Mutex<Option<Credentials>>>,
) -> Result<()> {
    if slot.has_live_agent() {
        // SESSION_BUSY (§18.38): one interactive session per machine.
        bail!("a session agent is already serving a client");
    }
    // Broker mode resolves the PSK per ticket: the grant (key, credentials,
    // session parameters) lands in `grant_slot` during the handshake.
    let grant_slot: Arc<Mutex<Option<broker::SessionGrant>>> = Arc::new(Mutex::new(None));
    let peer_ip = peer.ip().to_string();
    let mut stream = match runtime {
        crate::standalone::Runtime::Standalone(standalone) => {
            tokio::time::timeout(
                HANDSHAKE_TIMEOUT,
                EncryptedStream::server_handshake(TcpTransport::new(socket), &standalone.key, None),
            )
            .await
            .context("handshake timed out")??
        }
        crate::standalone::Runtime::Pq(pq) => {
            let provider = Arc::clone(&pq.provider);
            let grants = Arc::clone(&grant_slot);
            tokio::time::timeout(
                HANDSHAKE_TIMEOUT,
                EncryptedStream::server_handshake_with(TcpTransport::new(socket), None, move |ticket| {
                    let provider = Arc::clone(&provider);
                    let grants = Arc::clone(&grants);
                    let ip = peer_ip.clone();
                    async move {
                        let grant = provider.start(&ticket, &ip).await.map_err(|err| {
                            transport::TransportError::UnexpectedMessage(format!("broker: {err:#}"))
                        })?;
                        let key = grant.key;
                        *grants.lock().expect("grant slot poisoned") = Some(grant);
                        Ok(key)
                    }
                }),
            )
            .await
            .context("handshake timed out")??
        }
    };
    info!(mode = mode_name(runtime), "handshake validated (possession proof passed)");
    let grant = grant_slot.lock().expect("grant slot poisoned").take();

    // Per-session credentials (broker mode) replace the credprov slot for
    // the autologon below; standalone mode keeps its configured credentials.
    let creds = match runtime {
        crate::standalone::Runtime::Standalone(standalone) => standalone.creds.clone(),
        crate::standalone::Runtime::Pq(_) => grant
            .as_ref()
            .and_then(|grant| grant.credentials.clone())
            .map(|creds| Credentials::new(creds.user.trim(), &creds.domain, &creds.password)),
    };
    if matches!(runtime, crate::standalone::Runtime::Pq(_)) {
        *creds_slot.lock().expect("creds slot poisoned") = creds.clone();
    }

    // Console-conflict policy (§18.53), server-mandatory: another user
    // owning the console is resolved BEFORE any session side effect.
    let conflict = conflict_config(runtime, grant.as_ref());
    crate::console::resolve_conflict(&mut stream, creds.as_ref(), &conflict).await?;

    // From here on the client is authenticated: side effects are allowed.
    ensure_console_session(creds.as_ref()).await?;

    // §18.53 pre-session gate: tell the client the session is ready; its
    // next control message will be the agent's `UdpOffer` after the
    // handoff. (A lab agent without the service never sends this — the
    // client treats an early `UdpOffer` as an implicit ready.)
    let ready = protocol::ControlMessage::SessionReady;
    stream.send(protocol::channel::CONTROL, &ready.encode()?).await?;

    let (inner, hello, seqs) = stream.into_handoff();
    let socket = inner.into_stream().into_std().context("tokio -> std socket")?;

    let Some((agent, pipe)) = Agent::spawn_process_in_console()? else {
        bail!("no console session after the logon wait");
    };
    let socket_info = duplicate_socket_into(&socket, agent.pid)?;
    let video = grant.as_ref().and_then(|grant| grant.video.clone());
    let config = match runtime {
        crate::standalone::Runtime::Standalone(standalone) => service_proto::AgentConfig {
            key: standalone.key,
            ticket: standalone.ticket,
            bind: standalone.bind.clone(),
            output_name: standalone.output_name.clone().unwrap_or_default(),
            preset: standalone.preset.clone(),
            ladder: standalone.ladder.clone(),
            fps_adapt: standalone.fps_adapt.clone(),
            stats_csv: standalone.stats_csv.clone(),
            fixed_resolution: standalone.fixed_resolution.unwrap_or((0, 0)),
            allow_lock: standalone.allow_lock,
            wallpaper_off: standalone.wallpaper_off,
            handoff: Some(service_proto::HandoffInfo {
                send_seq: seqs.send,
                recv_seq: seqs.recv,
                hello: hello.encode().to_vec(),
                socket_info,
            }),
        },
        crate::standalone::Runtime::Pq(pq) => {
            let key = grant.as_ref().expect("pq mode grants a session").key;
            service_proto::AgentConfig {
                key,
                ticket: hello.ticket,
                bind: pq.bind.clone(),
                output_name: String::new(),
                preset: video
                    .as_ref()
                    .and_then(|video| video.preset.clone())
                    .unwrap_or_else(|| "ladder".to_string()),
                ladder: video
                    .as_ref()
                    .and_then(|video| video.ladder.clone())
                    .unwrap_or_default(),
                fps_adapt: video
                    .and_then(|video| video.adaptive_fps)
                    .unwrap_or_default(),
                stats_csv: String::new(),
                // §18.37: a fixed display policy in the ticket wins over the
                // client's preference; otherwise the agent approaches it.
                fixed_resolution: grant
                    .as_ref()
                    .and_then(|grant| grant.display.as_ref())
                    .filter(|display| display.fixed)
                    .map_or((0, 0), |display| (display.width, display.height)),
                // Server-side mandatory policy: the ticket payload decides
                // whether the client may lock the remote console.
                allow_lock: grant.as_ref().is_some_and(|grant| grant.channels.lock),
                wallpaper_off: grant.as_ref().is_some_and(|grant| grant.wallpaper_off),
                handoff: Some(service_proto::HandoffInfo {
                    send_seq: seqs.send,
                    recv_seq: seqs.recv,
                    hello: hello.encode().to_vec(),
                    socket_info,
                }),
            }
        }
    };
    Agent::write_config(pipe, &config)?;
    info!(pid = agent.pid, "connection handed off to the session agent");
    slot.set(agent, socket);

    // Broker mode: notify the broker when the session ends (best effort;
    // traffic counters are not plumbed back from the agent yet).
    if let crate::standalone::Runtime::Pq(pq) = runtime
        && let Some(grant) = grant.filter(|grant| grant.notify.is_some())
    {
        let provider = Arc::clone(&pq.provider);
        let slot = Arc::clone(slot);
        let creds_slot = Arc::clone(creds_slot);
        tokio::spawn(async move {
            let started = Instant::now();
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if !slot.has_live_agent() {
                    break;
                }
            }
            let stats = broker::SessionStats {
                elapsed: started.elapsed(),
                bytes_sent: 0,
                bytes_received: 0,
            };
            provider.stop(&grant, &stats).await;
            // The per-session credentials are one-shot: drop them.
            *creds_slot.lock().expect("creds slot poisoned") = None;
        });
    }
    Ok(())
}

fn mode_name(runtime: &crate::standalone::Runtime) -> &'static str {
    match runtime {
        crate::standalone::Runtime::Standalone(_) => "standalone",
        crate::standalone::Runtime::Pq(_) => "pq",
    }
}

/// Resolves the console-conflict configuration (§18.53) from the
/// connection mode: standalone file settings, or the broker ticket payload.
fn conflict_config(
    runtime: &crate::standalone::Runtime,
    grant: Option<&broker::SessionGrant>,
) -> crate::console::ConflictConfig {
    match runtime {
        crate::standalone::Runtime::Standalone(standalone) => crate::console::ConflictConfig {
            policy: standalone.console_conflict,
            reveal_user: standalone.console_conflict_reveal_user,
            timeout: Duration::from_secs(standalone.console_conflict_timeout_secs),
            takeover: standalone.console_takeover,
        },
        crate::standalone::Runtime::Pq(_) => {
            let grant = grant.expect("pq mode grants a session");
            crate::console::ConflictConfig {
                policy: grant.console_conflict,
                reveal_user: grant.console_conflict_reveal_user,
                timeout: Duration::from_secs(grant.console_conflict_timeout_secs),
                takeover: grant.console_takeover,
            }
        }
    }
}

/// Duplicates `socket` into the process `pid` and returns the serialized
/// `WSAPROTOCOL_INFO` blob the target process passes to `WSASocketW`.
fn duplicate_socket_into(socket: &std::net::TcpStream, pid: u32) -> Result<Vec<u8>> {
    use std::os::windows::io::AsRawSocket;
    let mut info = WSAPROTOCOL_INFOW::default();
    // SAFETY: `socket` is a live connected socket; `info` is a valid
    // out-pointer sized as the struct.
    let rc = unsafe {
        WSADuplicateSocketW(
            windows::Win32::Networking::WinSock::SOCKET(socket.as_raw_socket() as usize),
            pid,
            &mut info,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("WSADuplicateSocketW");
    }
    let bytes = unsafe {
        std::slice::from_raw_parts(
            &info as *const WSAPROTOCOL_INFOW as *const u8,
            size_of::<WSAPROTOCOL_INFOW>(),
        )
    };
    Ok(bytes.to_vec())
}

/// Ensures an interactive console session exists, triggering the autologon
/// (arm flag + synthetic SAS; the credential provider pipe then pushes the
/// session credentials) when the machine sits at the logon screen.
///
/// "Console up" means a console session WITH a logged-on user: after a
/// logoff (or a console disconnect) the console keeps a live winlogon
/// session id with no user, and spawning the agent there dies in
/// `WTSQueryUserToken` (0x800703F0) — only a fresh boot reports
/// `0xFFFF_FFFF` (§18.53 lesson of 2026-11-04).
async fn ensure_console_session(creds: Option<&Credentials>) -> Result<()> {
    if crate::console::console_has_user() {
        return Ok(());
    }
    let Some(creds) = creds else {
        bail!("no console session and no autologon credentials configured");
    };
    info!(user = %creds.username, "no console session; arming autologon and sending SAS");
    std::fs::write(crate::credprov::ARM_PATH, b"").context("creating the autologon arm flag")?;
    crate::sas::send_sas().context("synthetic SAS")?;

    // The first SAS can land while winlogon is still building the logon
    // session (e.g. right after a console-conflict eviction, §18.53) and is
    // then lost: the credential provider never loads and the logon never
    // happens. Re-send it on a slow cadence until the console comes up —
    // a SAS on an already-logged console is harmless (it just shows the
    // secure-attention options, which the autologon dismisses).
    const SAS_RETRY: Duration = Duration::from_secs(5);
    let start = Instant::now();
    let mut last_sas = Instant::now();
    loop {
        if crate::console::console_has_user() {
            info!("console session up after the autologon");
            return Ok(());
        }
        if start.elapsed() >= LOGON_TIMEOUT {
            bail!("autologon did not produce a console session within {LOGON_TIMEOUT:?}");
        }
        if last_sas.elapsed() >= SAS_RETRY {
            if let Err(err) = crate::sas::send_sas() {
                warn!(error = %format!("{err:#}"), "SAS retry failed");
            }
            last_sas = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
