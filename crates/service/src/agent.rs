//! Session-agent spawning and watchdog.
//!
//! The service (session 0, SYSTEM) cannot capture or inject input itself —
//! that is the agent's job inside the user session (see
//! doc/plan/servicio-y-sesion.md §13). This module spawns the agent into the
//! active console session with `CreateProcessAsUser` and watches it: the
//! process handle is the primary liveness signal, and the agent-held named
//! mutex (`Global\UDSRP-Session`) is the secondary one (an alive agent that
//! does not hold its mutex is wedged and gets killed).
//!
//! If the agent dies — crash, kill, wedged — the caller treats it as a dead
//! remote session and cleans up everything ephemeral (the IddCx monitor, and
//! implicitly the privacy overlay, whose windows die with the agent).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{error, info, warn};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_END, FILE_GENERIC_WRITE, FILE_SHARE_READ, OPEN_ALWAYS,
    SetFilePointerEx, WriteFile,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, OpenMutexW, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
    STARTUPINFOW, SYNCHRONIZATION_SYNCHRONIZE, TerminateProcess, WaitForSingleObject,
};
use windows::core::{BOOL, HSTRING, PCWSTR, PWSTR};

/// Name of the mutex the session agent holds while running (also acts as a
/// single-instance guard). `Global\` so session 0 and the user session see
/// the same object.
pub const SESSION_MUTEX: &str = r"Global\UDSRP-Session";

/// Grace period after spawning before the mutex must be observable.
const MUTEX_GRACE: Duration = Duration::from_secs(15);

/// Maximum respawns per service run before giving up (crash-loop guard).
const MAX_RESPAWNS: u32 = 5;

/// RAII wrapper for a Win32 handle.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and open.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// A running session agent: its process handle and when it was spawned.
pub struct Agent {
    process: OwnedHandle,
    spawned_at: Instant,
}

impl Agent {
    /// Spawns the agent (`server.exe`, expected next to our own executable)
    /// into the active console session with the given key + ticket. Returns
    /// `Ok(None)` when there is no interactive session to spawn into.
    ///
    /// The configuration (secrets included) is handed over through an
    /// anonymous pipe connected to the agent's stdin (`--agent-config-stdin`):
    /// the command line stays free of secrets, which any process in the
    /// session could otherwise read (M3). Credentials are also logged so the
    /// lab client can connect.
    pub fn spawn_in_console_session(key: &[u8; 32], ticket: &[u8; 48]) -> Result<Option<Self>> {
        let session_id = unsafe { WTSGetActiveConsoleSessionId() };
        if session_id == 0xFFFF_FFFF {
            return Ok(None);
        }

        // SAFETY: session_id is valid; the token handle is ours to close.
        let mut token_handle = HANDLE::default();
        unsafe { WTSQueryUserToken(session_id, &raw mut token_handle) }
            .context("WTSQueryUserToken (service must run as SYSTEM)")?;
        let token = OwnedHandle(token_handle);

        let exe = agent_exe_path()?;
        let key_hex = protocol::hex::encode(key);
        let ticket_hex = protocol::hex::encode(ticket);
        let mut cmdline: Vec<u16> = format!("\"{}\" --agent-config-stdin", exe.display())
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut desktop: Vec<u16> =
            "winsta0\\default".encode_utf16().chain(std::iter::once(0)).collect();
        // Config pipe: the read end becomes the agent's stdin (inheritable);
        // we keep the write end and send the frame once the process exists.
        let (pipe_read, pipe_write) = create_inheritable_pipe()?;
        // Redirect the agent's stdout/stderr to a log file: the process has
        // no console, and its diagnostics are otherwise lost.
        let log_handle = open_agent_log()?;
        let startup = STARTUPINFOW {
            cb: u32::try_from(size_of::<STARTUPINFOW>()).expect("struct fits in u32"),
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: pipe_read.0,
            hStdOutput: log_handle.0,
            hStdError: log_handle.0,
            ..Default::default()
        };
        let mut info = PROCESS_INFORMATION::default();
        // SAFETY: `token` is a primary user token for `session_id`;
        // `cmdline`/`desktop` are null-terminated, mutable and outlive the
        // call; `startup`/`info` are correctly sized structs.
        unsafe {
            CreateProcessAsUserW(
                Some(token.0),
                PCWSTR::null(),
                Some(PWSTR(cmdline.as_mut_ptr())),
                None,
                None,
                true,
                PROCESS_CREATION_FLAGS(0),
                None,
                PCWSTR::null(),
                &raw const startup,
                &raw mut info,
            )
        }
        .with_context(|| format!("CreateProcessAsUserW on session {session_id}"))?;
        // The thread handle is unused; close it immediately. The child owns a
        // copy of the pipe read end now; close ours so the agent sees EOF when
        // we finish writing (and so we do not hold its stdin open forever).
        let _thread = OwnedHandle(info.hThread);
        drop(pipe_read);
        let config = service_proto::AgentConfig {
            key: *key,
            ticket: *ticket,
            bind: "0.0.0.0:14433".to_owned(),
            output_name: "UDS".to_owned(),
            preset: "vp9-balanced".to_owned(),
        };
        write_all_and_close(&pipe_write, &service_proto::encode_agent_config(&config))?;
        info!(session_id, pid = info.dwProcessId, exe = %exe.display(), "session agent spawned");
        info!(key = key_hex, ticket = ticket_hex, "lab credentials (also piped to the agent)");
        Ok(Some(Self {
            process: OwnedHandle(info.hProcess),
            spawned_at: Instant::now(),
        }))
    }

    /// Whether the agent process is still alive.
    pub fn is_alive(&self) -> bool {
        // SAFETY: the process handle is ours and open.
        let wait = unsafe { WaitForSingleObject(self.process.0, 0) };
        wait != WAIT_OBJECT_0
    }

    /// Whether the agent still holds the session mutex (secondary liveness).
    /// Always `true` during the post-spawn grace period and on watchdog
    /// errors (never kill on our own failure to check).
    pub fn mutex_healthy(&self) -> bool {
        if self.spawned_at.elapsed() < MUTEX_GRACE {
            return true;
        }
        let name = HSTRING::from(SESSION_MUTEX);
        // SAFETY: `name` is null-terminated and outlives the call.
        let handle = unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, &name) };
        let Ok(raw) = handle else {
            // No mutex at all past the grace period: the agent never got as
            // far as creating it — wedged.
            warn!("session mutex does not exist past the grace period");
            return false;
        };
        let mutex = OwnedHandle(raw);
        // SAFETY: the mutex handle is ours and open.
        match unsafe { WaitForSingleObject(mutex.0, 0) } {
            WAIT_TIMEOUT => true, // owned by the agent
            WAIT_OBJECT_0 => {
                warn!("session mutex is free while the agent is alive");
                false
            }
            other => {
                warn!(?other, "unexpected mutex wait result");
                true
            }
        }
    }

    /// Forcefully terminates a wedged agent.
    pub fn kill(&self) {
        // SAFETY: the process handle is ours and open.
        unsafe {
            let _ = TerminateProcess(self.process.0, 1);
        }
    }
}

/// Creates an anonymous pipe whose read end is inheritable (for a child's
/// stdin); the write end stays non-inheritable so grandchildren cannot hold
/// the pipe open past our write.
fn create_inheritable_pipe() -> Result<(OwnedHandle, OwnedHandle)> {
    let attrs = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).expect("struct fits in u32"),
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: BOOL::from(true),
    };
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    // SAFETY: `attrs` is correctly sized; both out pointers are valid.
    unsafe { CreatePipe(&raw mut read, &raw mut write, Some(&raw const attrs), 0) }
        .context("CreatePipe for the agent config")?;
    // SAFETY: `write` is a valid handle we own; clearing the inherit flag is
    // always safe on our own handle.
    unsafe { SetHandleInformation(write, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) }
        .context("making the config pipe write end non-inheritable")?;
    Ok((OwnedHandle(read), OwnedHandle(write)))
}

/// Writes the whole config frame to the pipe and closes our end (the agent
/// reads until EOF). A dead/unreadable agent fails the write, which fails the
/// spawn and lets the watchdog respawn it.
fn write_all_and_close(pipe: &OwnedHandle, data: &[u8]) -> Result<()> {
    let mut written = 0usize;
    while written < data.len() {
        let mut n = 0u32;
        // SAFETY: `pipe` is a valid pipe write end we own; the slice and the
        // out pointer are valid for the duration of the call. Synchronous
        // pipe: no OVERLAPPED.
        unsafe {
            WriteFile(pipe.0, Some(&data[written..]), Some(&raw mut n), None)
                .context("writing the agent config frame")?;
        }
        anyhow::ensure!(n > 0, "config pipe write made no progress");
        written += n as usize;
    }
    Ok(())
}

/// Path of `server.exe` next to the running service executable.
fn agent_exe_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("resolving own executable path")?;
    Ok(exe.with_file_name("server.exe"))
}

/// Opens (creating and appending to) the agent log file and makes the handle
/// inheritable so the spawned agent can use it as stdout/stderr.
fn open_agent_log() -> Result<OwnedHandle> {
    let path = format!(r"{}\agent.log", crate::logging::STATE_DIR);
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a null-terminated path; no template handle.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ,
            None,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .context("opening the agent log file")?;
    // SAFETY: `handle` is valid; appending to the end of the file.
    unsafe {
        let _ = SetFilePointerEx(handle, 0, None, FILE_END);
        SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT)
            .context("making the agent log handle inheritable")?;
    }
    Ok(OwnedHandle(handle))
}

/// Service body: keep one agent running in the console session until the SCM
/// asks us to stop. Errors are logged and retried with backoff; the loop only
/// ends on stop (M5 revisits the exact recovery policy).
pub fn watch_loop(stop: &AtomicBool) {
    let mut agent: Option<Agent> = None;
    let mut respawns = 0u32;
    // Key and ticket are generated once per service run; the agent gets them
    // by command line (M3: pipe). Regenerating per respawn would invalidate
    // connected clients for no benefit.
    let mut key = [0u8; 32];
    let mut ticket = [0u8; 48];
    if let Err(err) = getrandom::fill(&mut key).and_then(|()| getrandom::fill(&mut ticket)) {
        error!(error = %format!("{err:#}"), "cannot generate session credentials");
        return;
    }

    while !stop.load(Ordering::Relaxed) {
        match &agent {
            None if respawns >= MAX_RESPAWNS => {
                error!(MAX_RESPAWNS, "agent respawn limit reached; giving up until service restart");
                // Keep the loop alive so the SCM can still stop us cleanly.
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(500));
                }
                break;
            }
            None => match Agent::spawn_in_console_session(&key, &ticket) {
                Ok(Some(spawned)) => {
                    respawns += 1;
                    agent = Some(spawned);
                }
                Ok(None) => std::thread::sleep(Duration::from_secs(2)), // no console session yet
                Err(err) => {
                    error!(error = %format!("{err:#}"), "cannot spawn the session agent");
                    std::thread::sleep(Duration::from_secs(5));
                }
            },
            Some(a) if !a.is_alive() => {
                warn!("session agent exited; cleaning up and respawning");
                agent = None;
            }
            Some(a) if !a.mutex_healthy() => {
                error!("session agent lost its mutex (wedged); killing it");
                a.kill();
                agent = None;
            }
            Some(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
    // Service stopping: do not leave an orphaned agent behind.
    if let Some(a) = &agent
        && a.is_alive()
    {
        info!("service stopping: terminating the session agent");
        a.kill();
    }
}
