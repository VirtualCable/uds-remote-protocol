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
use tracing::{error, info, warn};
use transport::{EncryptedStream, TcpTransport};
use windows::Win32::Networking::WinSock::{WSAPROTOCOL_INFOW, WSADuplicateSocketW};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;

use crate::agent::Agent;
use crate::standalone::StandaloneRuntime;

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
pub fn spawn(stop: &'static AtomicBool, standalone: StandaloneRuntime, slot: Arc<AgentSlot>) {
    std::thread::Builder::new()
        .name("udsrp-listener".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    error!(error = %format!("{err:#}"), "cannot create the listener runtime");
                    return;
                }
            };
            runtime.block_on(accept_loop(stop, standalone, slot));
        })
        .expect("cannot spawn the listener thread");
}

async fn accept_loop(stop: &AtomicBool, standalone: StandaloneRuntime, slot: Arc<AgentSlot>) {
    let listener = match tokio::net::TcpListener::bind(&standalone.bind).await {
        Ok(listener) => listener,
        Err(err) => {
            error!(error = %format!("{err:#}"), bind = %standalone.bind, "cannot bind the service listener");
            return;
        }
    };
    info!(bind = %standalone.bind, "service listener up (validating; handoff to session agent)");
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
        let standalone = &standalone;
        let slot = &slot;
        // Sequential on purpose (one session per machine): a second client
        // waits until the current attempt resolves.
        if let Err(err) = handle_connection(socket, standalone, slot).await {
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
    standalone: &StandaloneRuntime,
    slot: &Arc<AgentSlot>,
) -> Result<()> {
    if slot.has_live_agent() {
        // SESSION_BUSY (§18.38): one interactive session per machine.
        bail!("a session agent is already serving a client");
    }
    let stream = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        EncryptedStream::server_handshake(TcpTransport::new(socket), &standalone.key, None),
    )
    .await
    .context("handshake timed out")??;
    info!("handshake validated (possession proof passed)");

    // From here on the client is authenticated: side effects are allowed.
    ensure_console_session(standalone).await?;

    let (inner, hello, seqs) = stream.into_handoff();
    let socket = inner.into_stream().into_std().context("tokio -> std socket")?;

    let Some((agent, pipe)) = Agent::spawn_process_in_console()? else {
        bail!("no console session after the logon wait");
    };
    let socket_info = duplicate_socket_into(&socket, agent.pid)?;
    let config = service_proto::AgentConfig {
        key: standalone.key,
        ticket: standalone.ticket,
        bind: standalone.bind.clone(),
        output_name: standalone.output_name.clone().unwrap_or_default(),
        preset: standalone.preset.clone(),
        ladder: standalone.ladder.clone(),
        fps_adapt: standalone.fps_adapt.clone(),
        stats_csv: standalone.stats_csv.clone(),
        handoff: Some(service_proto::HandoffInfo {
            send_seq: seqs.send,
            recv_seq: seqs.recv,
            hello: hello.encode().to_vec(),
            socket_info,
        }),
    };
    Agent::write_config(pipe, &config)?;
    info!(pid = agent.pid, "connection handed off to the session agent");
    slot.set(agent, socket);
    Ok(())
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
/// standalone credentials) when the machine sits at the logon screen.
async fn ensure_console_session(standalone: &StandaloneRuntime) -> Result<()> {
    if unsafe { WTSGetActiveConsoleSessionId() } != 0xFFFF_FFFF {
        return Ok(());
    }
    let Some(creds) = &standalone.creds else {
        bail!("no console session and no autologon credentials configured");
    };
    info!(user = %creds.username, "no console session; arming autologon and sending SAS");
    std::fs::write(crate::credprov::ARM_PATH, b"").context("creating the autologon arm flag")?;
    crate::sas::send_sas().context("synthetic SAS")?;

    let start = Instant::now();
    loop {
        if unsafe { WTSGetActiveConsoleSessionId() } != 0xFFFF_FFFF {
            info!("console session up after the autologon");
            return Ok(());
        }
        if start.elapsed() >= LOGON_TIMEOUT {
            bail!("autologon did not produce a console session within {LOGON_TIMEOUT:?}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
