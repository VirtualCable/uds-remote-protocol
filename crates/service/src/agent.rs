// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

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
//! remote session and cleans up everything ephemeral.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{error, info, warn};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE, HANDLE_FLAGS,
    HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::{
    ACL, ACL_REVISION, AddAccessAllowedAce, AdjustTokenPrivileges, CreateWellKnownSid,
    DuplicateTokenEx, InitializeAcl, InitializeSecurityDescriptor, LookupPrivilegeValueW,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SE_PRIVILEGE_ENABLED,
    SE_TCB_NAME, SecurityImpersonation, SetSecurityDescriptorDacl, SetTokenInformation,
    TOKEN_ACCESS_MASK, TOKEN_ADJUST_PRIVILEGES, TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY,
    TOKEN_DUPLICATE, TOKEN_PRIVILEGES, TOKEN_QUERY, TokenPrimary, TokenSessionId, WinWorldSid,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_END, FILE_GENERIC_WRITE, FILE_SHARE_READ, OPEN_ALWAYS,
    SetFilePointerEx, WriteFile,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSGetActiveConsoleSessionId, WTSQueryUserToken,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
    TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateEventW, CreateMutexW, CreateProcessAsUserW, EVENT_MODIFY_STATE,
    GetCurrentProcess, MUTEX_MODIFY_STATE, OpenMutexW, OpenProcess, OpenProcessToken,
    PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION, ReleaseMutex, STARTF_USESTDHANDLES,
    STARTUPINFOW, SYNCHRONIZATION_SYNCHRONIZE, TerminateProcess, WaitForSingleObject,
};
use windows::core::{BOOL, HSTRING, PCWSTR, PWSTR};

/// Name of the mutex the session agent holds while running (also acts as a
/// single-instance guard). `Global\` so session 0 and the user session see
/// the same object.
pub const SESSION_MUTEX: &str = r"Global\UDSRP-Session";

/// Named event (manual-reset, global namespace) the session agent sets while
/// a secure desktop owns the console (§18.41). The watch loop spawns the
/// secure helper while set and kills it when cleared.
pub(crate) const SECURE_EVENT: &str = r"Global\UDSRP-SecureDesktop";

/// Grace period after spawning before the mutex must be observable.
const MUTEX_GRACE: Duration = Duration::from_secs(15);

/// Maximum respawns per service run before giving up (crash-loop guard).
const MAX_RESPAWNS: u32 = 5;

/// Minimum interval between synthetic SAS attempts while waiting for the
/// auto-logon to produce an interactive session (the logon itself takes
/// several seconds; re-sending faster would just strobe the secure screen).
const SAS_COOLDOWN: Duration = Duration::from_secs(15);

/// RAII wrapper for a Win32 handle.
pub(crate) struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and open.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// A running session agent: its process handle, the kill-on-close job it
/// lives in, and when it was spawned. The job handle is the symmetric-kill
/// mechanism (M5): when the service dies — even a hard kill — the kernel
/// closes the job's last handle and every process in it is terminated, so an
/// orphaned agent never lingers holding the session mutex or the network
/// port.
pub struct Agent {
    process: OwnedHandle,
    /// `None` when job assignment failed (logged; liveness still works).
    _job: Option<OwnedHandle>,
    /// Process id (socket handoff target, §18.32).
    pub pid: u32,
    /// Console session the agent was spawned into.
    pub session_id: u32,
    spawned_at: Instant,
}

// SAFETY: an Agent only carries raw handle values (process/job HANDLEs) and
// plain data. Win32 process and job handles are usable from any thread, so
// moving an Agent between threads (the watch loop owns it, the listener
// produces it) is sound.
unsafe impl Send for Agent {}

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
    pub(crate) fn spawn_in_console_session(config: &service_proto::AgentConfig) -> Result<Option<Self>> {
        let Some((agent, pipe_write)) = Self::spawn_process_in_console()? else {
            return Ok(None);
        };
        write_all_and_close(&pipe_write, &service_proto::encode_agent_config(config))?;
        info!(session_id = agent.session_id, pid = agent.pid, "session agent spawned");
        info!(key = protocol::hex::encode(&config.key), ticket = protocol::hex::encode(&config.ticket), "lab credentials (also piped to the agent)");
        Ok(Some(agent))
    }

    /// First half of the spawn for the handoff flow (§18.32): creates the
    /// agent process in the console session and returns it together with the
    /// write end of its config pipe, WITHOUT writing the config. The caller
    /// can then use `agent.pid` (e.g. `WSADuplicateSocketW` into it) and must
    /// finish with [`Agent::write_config`].
    pub(crate) fn spawn_process_in_console() -> Result<Option<(Self, OwnedHandle)>> {
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
                // No console window on the user's desktop: the agent's
                // stdout/stderr already go to the log file.
                CREATE_NO_WINDOW,
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
        let job = match assign_kill_on_close_job(info.hProcess) {
            Ok(job) => Some(job),
            Err(err) => {
                // Non-fatal: the watchdog still tracks the process handle and
                // the mutex; only the orphan-on-hard-kill guarantee is lost.
                warn!(error = %format!("{err:#}"), "cannot put the agent in a kill-on-close job");
                None
            }
        };
        // §18.42: spawn the SYSTEM input injector on the Default desktop and
        // add it to the SAME job, so it dies with the agent (kill-on-close).
        // Best effort: without it, input falls back to the agent's local
        // medium-IL SendInput (elevated windows unreachable, UIPI).
        if let Some(job_handle) = &job {
            match spawn_system_in_console(
                &format!("--input-injector {session_id}"),
                "winsta0\\default",
            ) {
                Ok(Some((pid, process))) => {
                    if let Err(err) = assign_process_to_job(job_handle.0, process.0) {
                        warn!(error = %format!("{err:#}"), "cannot add the input injector to the agent job");
                    }
                    info!(pid, session_id, "SYSTEM input injector spawned");
                }
                Ok(None) => warn!("no console session for the input injector"),
                Err(err) => {
                    warn!(error = %format!("{err:#}"), "cannot spawn the input injector")
                }
            }
        }
        Ok(Some((
            Self {
                process: OwnedHandle(info.hProcess),
                _job: job,
                pid: info.dwProcessId,
                session_id,
                spawned_at: Instant::now(),
            },
            pipe_write,
        )))
    }

    /// Second half of the handoff spawn: sends the config frame (with the
    /// handoff block already filled in) down the agent's stdin pipe.
    pub(crate) fn write_config(pipe_write: OwnedHandle, config: &service_proto::AgentConfig) -> Result<()> {
        write_all_and_close(&pipe_write, &service_proto::encode_agent_config(config))
    }

    /// Whether the agent process is still alive.
    pub(crate) fn is_alive(&self) -> bool {
        // SAFETY: the process handle is ours and open.
        let wait = unsafe { WaitForSingleObject(self.process.0, 0) };
        wait != WAIT_OBJECT_0
    }

    /// Whether the agent still holds the session mutex (secondary liveness).
    /// Always `true` during the post-spawn grace period and on watchdog
    /// errors (never kill on our own failure to check).
    pub(crate) fn mutex_healthy(&self) -> bool {
        if self.spawned_at.elapsed() < MUTEX_GRACE {
            return true;
        }
        let name = HSTRING::from(SESSION_MUTEX);
    // SAFETY: `name` is null-terminated and outlives the call. MODIFY_STATE
    // is requested too because the wait below can ACQUIRE the mutex (when
    // free or abandoned) and we then have to release it.
    let handle = unsafe {
        OpenMutexW(MUTEX_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE, false, &name)
    };
        let Ok(raw) = handle else {
            // No mutex at all past the grace period: the agent never got as
            // far as creating it — wedged.
            warn!("session mutex does not exist past the grace period");
            return false;
        };
        let mutex = OwnedHandle(raw);
        // SAFETY: the mutex handle is ours and open. Note the wait ACQUIRES
        // the mutex when it is free/abandoned: release it immediately so a
        // respawned agent can take ownership.
        match unsafe { WaitForSingleObject(mutex.0, 0) } {
            WAIT_TIMEOUT => true, // owned by the agent
            WAIT_OBJECT_0 | WAIT_ABANDONED => {
                warn!("session mutex is free while the agent is alive");
                unsafe {
                    let _ = ReleaseMutex(mutex.0);
                }
                false
            }
            other => {
                warn!(?other, "unexpected mutex wait result");
                true
            }
        }
    }

    /// Forcefully terminates a wedged agent.
    pub(crate) fn kill(&self) {
        // SAFETY: the process handle is ours and open.
        unsafe {
            let _ = TerminateProcess(self.process.0, 1);
        }
    }
}

/// Puts `process` into a fresh job object configured with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: when the service process dies for
/// any reason (clean exit forgotten paths, `taskkill /F`, crash), its job
/// handle is closed and the kernel terminates every process in the job.
/// Returns the job handle (the caller owns it for the agent's lifetime).
fn assign_kill_on_close_job(process: HANDLE) -> Result<OwnedHandle> {
    // SAFETY: null attributes/name create an unnamed job with default security.
    let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.context("CreateJobObject")?;
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `job` is valid; `info` is a correctly sized input struct.
    unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .expect("struct fits in u32"),
        )
        .context("SetInformationJobObject(KILL_ON_JOB_CLOSE)")?;
    }
    assign_process_to_job(job, process)?;
    Ok(OwnedHandle(job))
}

/// Adds a process to an existing job object.
fn assign_process_to_job(job: HANDLE, process: HANDLE) -> Result<()> {
    // SAFETY: both handles are valid and outlive the call.
    unsafe { AssignProcessToJobObject(job, process) }.context("AssignProcessToJobObject")?;
    Ok(())
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

/// Lab trigger for the secure-desktop probe (§18.41, M9): when this file
/// exists, the watch loop spawns one probe instance and removes the flag.
pub(crate) const PROBE_FLAG: &str = r"C:\ProgramData\UdsRp\probe-secure.flag";

/// Spawns the secure-desktop probe (`server.exe --secure-probe`) as SYSTEM
/// inside the console session (lab tool, §18.41). Fire-and-forget: the
/// probe exits by itself and logs to its own file, so the process handle is
/// dropped. Returns `Ok(None)` when there is no interactive console session.
pub(crate) fn spawn_probe_in_console() -> Result<Option<u32>> {
    let log_path = format!(r"{}\probe.log", crate::logging::STATE_DIR);
    let spawned = spawn_system_in_console(
        &format!("--secure-probe \"{log_path}\""),
        "winsta0\\default",
    )?;
    Ok(spawned.map(|(pid, _process)| pid))
}

/// Spawns the secure-desktop helper (`server.exe --secure-helper`) as SYSTEM
/// **born on the Winlogon desktop** (lab-validated: only a process born on
/// the secure desktop can `SendInput` into it). The caller keeps the process
/// handle for liveness/kill. Returns `Ok(None)` with no console session.
pub(crate) fn spawn_secure_helper(session_id: u32) -> Result<Option<(u32, OwnedHandle)>> {
    spawn_system_in_console(&format!("--secure-helper {session_id}"), "winsta0\\Winlogon")
}

/// Shared SYSTEM-in-session spawn (§18.41): the token is a duplicate of the
/// service's own SYSTEM token with its session id retargeted
/// (`SetTokenInformation(TokenSessionId)`), falling back to the token of
/// `winlogon.exe` of the target session when the retarget is denied
/// (observed in the lab; the PsExec/VNC recipe). NOT `WTSQueryUserToken`:
/// that yields the session user's token (no rights on the secure desktop,
/// and the wrong identity for the helper).
///
/// `args` is appended to the quoted server.exe path; `desktop` is the
/// STARTUPINFO desktop (`winsta0\default` for capture-only lab tools,
/// `winsta0\Winlogon` for the secure helper).
fn spawn_system_in_console(args: &str, desktop: &str) -> Result<Option<(u32, OwnedHandle)>> {
    let session_id = unsafe { WTSGetActiveConsoleSessionId() };
    if session_id == 0xFFFF_FFFF {
        return Ok(None);
    }
    // SAFETY: GetCurrentProcess returns a pseudo-handle (always valid).
    let mut token_handle = HANDLE::default();
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ACCESS_MASK(TOKEN_DUPLICATE.0 | TOKEN_QUERY.0 | TOKEN_ADJUST_PRIVILEGES.0),
            &raw mut token_handle,
        )
    }
    .context("OpenProcessToken(self)")?;
    let own_token = OwnedHandle(token_handle);

    // SeTcbPrivilege is present in a SYSTEM token but DISABLED by default.
    // The privilege check of SetTokenInformation(TokenSessionId) runs against
    // the CALLER's token, so it must be enabled on our own process token
    // (persists for the process lifetime — the future secure-desktop helper
    // spawn needs it too).
    let mut luid = windows::Win32::Foundation::LUID::default();
    unsafe { LookupPrivilegeValueW(None, SE_TCB_NAME, &raw mut luid) }
        .context("LookupPrivilegeValue(SeTcbPrivilege)")?;
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [windows::Win32::Security::LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: `own_token` has ADJUST_PRIVILEGES; `privileges` outlives the
    // call. AdjustTokenPrivileges reports success even when a privilege was
    // not present in the token (ERROR_NOT_ALL_ASSIGNED) — check for it.
    unsafe {
        AdjustTokenPrivileges(own_token.0, false, Some(&raw const privileges), 0, None, None)
    }
    .context("AdjustTokenPrivileges(SeTcbPrivilege)")?;
    if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
        warn!("SeTcbPrivilege is not present in the service token");
    }

    let mut dup_handle = HANDLE::default();
    let dup_access = TOKEN_ACCESS_MASK(
        TOKEN_ASSIGN_PRIMARY.0
            | TOKEN_DUPLICATE.0
            | TOKEN_QUERY.0
            | TOKEN_ADJUST_SESSIONID.0,
    );
    // SAFETY: `own_token` is open with DUPLICATE; the out pointer is valid.
    unsafe {
        DuplicateTokenEx(
            own_token.0,
            dup_access,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &raw mut dup_handle,
        )
    }
    .context("DuplicateTokenEx(self)")?;
    let probe_token = OwnedHandle(dup_handle);
    // SAFETY: `probe_token` has ADJUST_SESSIONID; `session_id` outlives the
    // call; the caller's SeTcbPrivilege is enabled above. When the retarget
    // is denied anyway (observed in the lab), fall back to duplicating the
    // token of a SYSTEM process already running in the target session.
    let helper_token = match unsafe {
        SetTokenInformation(
            probe_token.0,
            TokenSessionId,
            (&raw const session_id).cast(),
            u32::try_from(size_of::<u32>()).expect("u32 fits"),
        )
    } {
        Ok(()) => probe_token,
        Err(err) => {
            warn!(error = %format!("{err:#}"), "TokenSessionId retarget denied; borrowing a session SYSTEM token");
            drop(probe_token);
            system_session_token(session_id)?
        }
    };

    let exe = agent_exe_path()?;
    let mut cmdline: Vec<u16> = format!("\"{}\" {args}", exe.display())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut desktop: Vec<u16> = desktop.encode_utf16().chain(std::iter::once(0)).collect();
    let startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).expect("struct fits in u32"),
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: `helper_token` is a primary SYSTEM token for `session_id`;
    // `cmdline`/`desktop` are null-terminated, mutable and outlive the call.
    unsafe {
        CreateProcessAsUserW(
            Some(helper_token.0),
            PCWSTR::null(),
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &raw const startup,
            &raw mut info,
        )
    }
    .with_context(|| format!("CreateProcessAsUserW ({args}) on session {session_id}"))?;
    let _thread = OwnedHandle(info.hThread);
    Ok(Some((info.dwProcessId, OwnedHandle(info.hProcess))))
}

/// Fallback token source for the secure-desktop probe/helper (§18.41):
/// duplicates the token of a SYSTEM process already running in `session_id`
/// (winlogon.exe), avoiding `SetTokenInformation(TokenSessionId)` entirely.
/// That token already carries the target session id.
fn system_session_token(session_id: u32) -> Result<OwnedHandle> {
    // SAFETY: plain snapshot of the process list.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .context("CreateToolhelp32Snapshot")?;
    let snapshot = OwnedHandle(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: u32::try_from(size_of::<PROCESSENTRY32W>()).expect("struct fits in u32"),
        ..Default::default()
    };
    // SAFETY: `entry` is correctly sized and ours; iterating the snapshot is
    // read-only.
    unsafe { Process32FirstW(snapshot.0, &raw mut entry) }.context("Process32FirstW")?;
    let mut pid = None;
    loop {
        let name_len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]);
        if name.eq_ignore_ascii_case("winlogon.exe") {
            let mut proc_session = 0u32;
            // SAFETY: valid out pointer.
            if unsafe { ProcessIdToSessionId(entry.th32ProcessID, &raw mut proc_session) }.is_ok()
                && proc_session == session_id
            {
                pid = Some(entry.th32ProcessID);
                break;
            }
        }
        if unsafe { Process32NextW(snapshot.0, &raw mut entry) }.is_err() {
            break;
        }
    }
    let pid = pid.with_context(|| format!("no winlogon.exe found in session {session_id}"))?;
    // SAFETY: opening a live process by id for token query.
    let process = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION, false, pid) }
        .with_context(|| format!("OpenProcess(winlogon pid={pid})"))?;
    let process = OwnedHandle(process);
    let mut token_handle = HANDLE::default();
    // SAFETY: `process` is open with QUERY_INFORMATION; the out pointer is
    // valid.
    unsafe {
        OpenProcessToken(
            process.0,
            TOKEN_ACCESS_MASK(TOKEN_DUPLICATE.0 | TOKEN_QUERY.0),
            &raw mut token_handle,
        )
    }
    .context("OpenProcessToken(winlogon)")?;
    let session_token = OwnedHandle(token_handle);
    let mut dup_handle = HANDLE::default();
    // SAFETY: `session_token` is open with DUPLICATE; the out pointer is
    // valid.
    unsafe {
        DuplicateTokenEx(
            session_token.0,
            TOKEN_ACCESS_MASK(TOKEN_ASSIGN_PRIMARY.0 | TOKEN_DUPLICATE.0 | TOKEN_QUERY.0),
            None,
            SecurityImpersonation,
            TokenPrimary,
            &raw mut dup_handle,
        )
    }
    .context("DuplicateTokenEx(winlogon)")?;
    Ok(OwnedHandle(dup_handle))
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

/// Creates the global secure-desktop event (manual-reset, initially clear)
/// with an Everyone DACL granting only set/reset/wait rights
/// (`EVENT_MODIFY_STATE | SYNCHRONIZE`) — same reasoning as the session
/// mutex DACL above. Returns `None` (supervision disabled) on any failure.
fn ensure_secure_event() -> Option<OwnedHandle> {
    let mut world_sid = vec![0u8; 68];
    let mut sid_len = world_sid.len() as u32;
    let sid_ok = unsafe {
        CreateWellKnownSid(
            WinWorldSid,
            None,
            Some(PSID(world_sid.as_mut_ptr().cast())),
            &mut sid_len,
        )
    };
    if sid_ok.is_err() {
        warn!("cannot build the Everyone SID; secure-helper supervision disabled");
        return None;
    }
    let mut sec_desc = Box::new(SECURITY_DESCRIPTOR::default());
    let mut acl_buf = vec![0u8; 1024];
    let ready = unsafe {
        InitializeSecurityDescriptor(
            PSECURITY_DESCRIPTOR(sec_desc.as_mut() as *mut _ as _),
            windows::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION,
        )
        .is_ok()
            && InitializeAcl(acl_buf.as_mut_ptr().cast::<ACL>(), acl_buf.len() as u32, ACL_REVISION)
                .is_ok()
            && AddAccessAllowedAce(
                acl_buf.as_mut_ptr().cast::<ACL>(),
                ACL_REVISION,
                EVENT_MODIFY_STATE.0 | SYNCHRONIZATION_SYNCHRONIZE.0,
                PSID(world_sid.as_mut_ptr().cast()),
            )
            .is_ok()
            && SetSecurityDescriptorDacl(
                PSECURITY_DESCRIPTOR(sec_desc.as_mut() as *mut _ as _),
                true,
                Some(acl_buf.as_mut_ptr().cast::<ACL>()),
                false,
            )
            .is_ok()
    };
    if !ready {
        warn!("cannot build the secure-event DACL; supervision disabled");
        return None;
    }
    let attrs = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: sec_desc.as_mut() as *mut _ as *mut core::ffi::c_void,
        bInheritHandle: BOOL(0),
    };
    let name: Vec<u16> = SECURE_EVENT.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `attrs` points to a valid security descriptor; `name` is
    // null-terminated. Manual-reset, initially clear.
    match unsafe { CreateEventW(Some(&raw const attrs), true, false, PCWSTR(name.as_ptr())) } {
        Ok(h) => {
            std::mem::forget(sec_desc);
            std::mem::forget(acl_buf);
            std::mem::forget(world_sid);
            Some(OwnedHandle(h))
        }
        Err(err) => {
            warn!(error = %format!("{err:#}"), "cannot create the secure-desktop event; supervision disabled");
            None
        }
    }
}

/// Whether the secure-desktop event is currently set (manual-reset: the wait
/// does not consume the state).
fn secure_event_set(event: &OwnedHandle) -> bool {
    // SAFETY: the event handle is ours and open; a zero timeout never blocks.
    (unsafe { WaitForSingleObject(event.0, 0) }) == WAIT_OBJECT_0
}

/// True when the console session shows the lock screen (WTS SessionFlags:
/// 0 = locked, 1 = unlocked). Distinguishes a lock from a mere UAC prompt —
/// both own the Winlogon secure desktop (§18.41).
fn console_session_locked(session_id: u32) -> bool {
    use windows::Win32::System::RemoteDesktop::{
        WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfoEx, WTSINFOEXW,
    };
    let mut buf = windows::core::PWSTR::null();
    let mut bytes = 0u32;
    // SAFETY: valid out-params; on success the buffer is freed below.
    let queried = unsafe {
        WTSQuerySessionInformationW(None, session_id, WTSSessionInfoEx, &raw mut buf, &raw mut bytes)
    };
    if queried.is_err() || buf.is_null() {
        return false;
    }
    // SAFETY: on success the buffer holds a WTSINFOEXW; Level 1 carries the
    // LEVEL1 union arm with the lock flag.
    let locked = unsafe {
        let info = buf.0 as *const WTSINFOEXW;
        let locked = (*info).Level == 1 && (*info).Data.WTSInfoExLevel1.SessionFlags == 0;
        WTSFreeMemory(buf.0.cast());
        locked
    };
    locked
}

/// Re-arms the credential provider for auto-unlock when the console session
/// is locked and we hold its credentials (§18.41) — same one-shot arm flag
/// as the autologon path (`credprov.rs`). Without credentials the user types
/// the password over the secure-desktop helper instead.
fn maybe_arm_auto_unlock(
    creds_slot: &std::sync::Arc<std::sync::Mutex<Option<udscredprov::logic::Credentials>>>,
) {
    let session_id = unsafe { WTSGetActiveConsoleSessionId() };
    if session_id == 0xFFFF_FFFF || !console_session_locked(session_id) {
        return;
    }
    let have_creds = creds_slot.lock().expect("creds slot poisoned").is_some()
        || std::path::Path::new(crate::credprov::CREDS_PATH).exists();
    if !have_creds {
        info!("console locked but no session credentials held; manual unlock required");
        return;
    }
    match std::fs::write(crate::credprov::ARM_PATH, "") {
        Ok(()) => {
            info!(session_id, "console locked; credential provider re-armed for auto-unlock");
        }
        Err(err) => warn!(error = %format!("{err:#}"), "cannot arm auto-unlock"),
    }
}

/// Supervises the secure-desktop helper against the event (§18.41): spawns
/// it when the event is set and no helper runs, kills it when the event is
/// clear. The helper also self-exits when the desktop returns to Default;
/// this is the belt-and-braces path.
fn supervise_secure_helper(event: &OwnedHandle, helper: &mut Option<(u32, OwnedHandle)>) {
    // Drop finished helpers first (they exit by themselves on Default).
    if let Some((_, process)) = helper {
        // SAFETY: the process handle is ours and open.
        if unsafe { WaitForSingleObject(process.0, 0) } == WAIT_OBJECT_0 {
            *helper = None;
        }
    }
    match (helper.is_some(), secure_event_set(event)) {
        (false, true) => {
            let session_id = unsafe { WTSGetActiveConsoleSessionId() };
            match spawn_secure_helper(session_id) {
                Ok(Some((pid, process))) => {
                    info!(pid, session_id, "secure-desktop helper spawned");
                    *helper = Some((pid, process));
                }
                Ok(None) => {}
                Err(err) => {
                    warn!(error = %format!("{err:#}"), "cannot spawn the secure-desktop helper");
                }
            }
        }
        (true, false) => {
            if let Some((pid, process)) = helper.take() {
                info!(pid, "secure desktop left; stopping the helper");
                // SAFETY: the process handle is ours and open.
                unsafe {
                    let _ = TerminateProcess(process.0, 0);
                }
            }
        }
        _ => {}
    }
}

/// Service body: keep one agent running in the console session until the SCM
/// asks us to stop. Errors are logged and retried with backoff; the loop only
/// ends on stop (M5 revisits the exact recovery policy).
/// Creates the agent session mutex in the **global** namespace, unowned,
/// with an Everyone DACL. Creating `Global\` objects requires
/// `SeCreateGlobalPrivilege` (services have it; standard session users do
/// not — their `Global\` is silently redirected to the session-local
/// namespace, which made the watchdog blind and caused a kill/respawn loop
/// once the agent ran as a standard user). The agent OPENS this mutex and
/// takes ownership for its lifetime; this handle just keeps the object
/// alive and unowned between agents.
fn ensure_session_mutex() -> Option<OwnedHandle> {
    // DACL: Everyone gets ONLY acquire/release rights (SYNCHRONIZE |
    // MUTEX_MODIFY_STATE) — never WRITE_DAC/WRITE_OWNER/DELETE, so a local
    // process cannot tamper with or destroy the watchdog token. The agent
    // runs as an arbitrary standard user, so acquire rights must be granted
    // broadly; the residual risk (a local process hogging the mutex to block
    // agent startup) is loud (the agent refuses to start) and inherent — a
    // local user can always DoS their own session (§18.33).
    let mut world_sid = vec![0u8; 68];
    let mut sid_len = world_sid.len() as u32;
    let sid_ok = unsafe {
        CreateWellKnownSid(
            WinWorldSid,
            None,
            Some(PSID(world_sid.as_mut_ptr().cast())),
            &mut sid_len,
        )
    };
    if sid_ok.is_err() {
        warn!("cannot build the Everyone SID; session-mutex watchdog disabled");
        return None;
    }
    let mut sec_desc = Box::new(SECURITY_DESCRIPTOR::default());
    let mut acl_buf = vec![0u8; 1024];
    let ready = unsafe {
        InitializeSecurityDescriptor(
            PSECURITY_DESCRIPTOR(sec_desc.as_mut() as *mut _ as _),
            windows::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION,
        )
        .is_ok()
            && InitializeAcl(acl_buf.as_mut_ptr().cast::<ACL>(), acl_buf.len() as u32, ACL_REVISION)
                .is_ok()
            && AddAccessAllowedAce(
                acl_buf.as_mut_ptr().cast::<ACL>(),
                ACL_REVISION,
                MUTEX_MODIFY_STATE.0 | SYNCHRONIZATION_SYNCHRONIZE.0,
                PSID(world_sid.as_mut_ptr().cast()),
            )
            .is_ok()
            && SetSecurityDescriptorDacl(
                PSECURITY_DESCRIPTOR(sec_desc.as_mut() as *mut _ as _),
                true,
                Some(acl_buf.as_mut_ptr().cast::<ACL>()),
                false,
            )
            .is_ok()
    };
    if !ready {
        warn!("cannot build the session-mutex DACL; watchdog disabled");
        return None;
    }
    let attrs = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: sec_desc.as_mut() as *mut _ as *mut core::ffi::c_void,
        bInheritHandle: BOOL(0),
    };
    let mut name: Vec<u16> = SESSION_MUTEX.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `attrs` points to a valid security descriptor; `name` is
    // null-terminated. Created unowned: the agent takes ownership.
    let handle = unsafe { CreateMutexW(Some(&raw const attrs), false, PWSTR(name.as_mut_ptr())) };
    match handle {
        Ok(h) => {
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                info!("session mutex already existed (leftover); reusing it");
            } else {
                info!("global session mutex pre-created for the agent");
            }
            // The security buffers must outlive the handle; leak them
            // (one per service run, a few KB).
            std::mem::forget(sec_desc);
            std::mem::forget(acl_buf);
            std::mem::forget(world_sid);
            Some(OwnedHandle(h))
        }
        Err(err) => {
            warn!(error = %format!("{err:#}"), "cannot pre-create the session mutex; watchdog disabled");
            None
        }
    }
}

pub(crate) fn watch_loop(
    stop: &'static AtomicBool,
    runtime: Option<crate::standalone::Runtime>,
    creds_slot: std::sync::Arc<std::sync::Mutex<Option<udscredprov::logic::Credentials>>>,
) {
    // Pre-create the global session mutex before any agent spawns: only a
    // privileged process can create Global\ objects (see ensure_session_mutex).
    let _session_mutex = ensure_session_mutex();
    // Standalone/pq modes (§18.32): the service owns the validating listener
    // and spawns one agent per validated connection; the watch loop only
    // watches.
    if let Some(runtime) = runtime {
        watch_with_listener(stop, runtime, creds_slot);
        return;
    }
    let mut agent: Option<Agent> = None;
    let mut respawns = 0u32;
    // Key and ticket are generated once per service run; the agent gets them
    // through the stdin pipe (M3). Regenerating per respawn would invalidate
    // connected clients for no benefit. In standalone mode they come from
    // the shared config file instead (fixed, documented ticket).
    let mut key = [0u8; 32];
    let mut ticket = [0u8; 48];
    if let Err(err) = getrandom::fill(&mut key).and_then(|()| getrandom::fill(&mut ticket)) {
        error!(error = %format!("{err:#}"), "cannot generate session credentials");
        return;
    }
    let config = service_proto::AgentConfig {
        key,
        ticket,
        bind: "0.0.0.0:14433".to_owned(),
        output_name: String::new(),
        preset: "ladder".to_owned(),
        ladder: String::new(),
        fps_adapt: String::new(),
        stats_csv: String::new(),
        fixed_resolution: (0, 0),
        handoff: None,
    };

    // Unattended auto-logon (see sas.rs), gated by the one-shot arm flag
    // (§18.30): only while a logon is armed do we nudge LogonUI with a
    // synthetic SAS — without CAD satisfied it never enumerates providers,
    // so an armed auto-logon would wait for a human pressing Ctrl+Alt+Del
    // forever. Without the flag the machine must boot to a plain logon
    // screen and stay there.
    let has_logon_creds = std::path::Path::new(crate::credprov::CREDS_PATH).exists();
    let mut last_sas: Option<Instant> = None;
    let nudge_towards_logon = |last_sas: &mut Option<Instant>| {
        if has_logon_creds
            && std::path::Path::new(crate::credprov::ARM_PATH).exists()
            && last_sas.is_none_or(|t| t.elapsed() >= SAS_COOLDOWN)
        {
            match crate::sas::send_sas() {
                Ok(()) => info!("armed with no interactive session; synthetic SAS sent"),
                Err(err) => warn!(error = %format!("{err:#}"), "cannot send the synthetic SAS"),
            }
            *last_sas = Some(Instant::now());
        }
    };

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
            None => match Agent::spawn_in_console_session(&config) {
                Ok(Some(spawned)) => {
                    respawns += 1;
                    agent = Some(spawned);
                }
                Ok(None) => {
                    nudge_towards_logon(&mut last_sas);
                    std::thread::sleep(Duration::from_secs(2)); // no console session yet
                }
                Err(err) => {
                    error!(error = %format!("{err:#}"), "cannot spawn the session agent");
                    nudge_towards_logon(&mut last_sas);
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

/// Validating-listener watch body (§18.32): the listener thread owns the
/// public TCP endpoint, validates each connection (handshake + possession
/// proof, against the fixed standalone ticket or the broker in pq mode) and
/// spawns one session agent per validated client. This loop only supervises:
/// it clears dead agents, kills wedged ones, and cleans up on service stop.
/// No proactive spawn and no respawn cap: agents map one-to-one to client
/// connections, so a crash loop cannot spin here.
fn watch_with_listener(
    stop: &'static AtomicBool,
    runtime: crate::standalone::Runtime,
    creds_slot: std::sync::Arc<std::sync::Mutex<Option<udscredprov::logic::Credentials>>>,
) {
    let slot = std::sync::Arc::new(crate::listener::AgentSlot::default());
    crate::listener::spawn(stop, runtime, std::sync::Arc::clone(&slot), std::sync::Arc::clone(&creds_slot));
    let secure_event = ensure_secure_event();
    let mut helper: Option<(u32, OwnedHandle)> = None;
    let mut unlock_armed = false;
    let mut sas_sent = 0u32;
    while !stop.load(Ordering::Relaxed) {
        slot.clear_if_dead();
        slot.kill_if_unhealthy();
        if let Some(event) = &secure_event {
            supervise_secure_helper(event, &mut helper);
        }
        // Auto-unlock (§18.41/§18.52): on a LOCKED console (a UAC prompt is
        // not a lock), re-arm the credential provider so the session unlocks
        // by itself while we hold its creds, and nudge LogonUI with a
        // synthetic SAS. Driven by the WTS lock state, NOT by the secure
        // event: the lock curtain is painted by LockApp on the *Default*
        // desktop, so the OpenInputDesktop-based watch cannot see it — only
        // the post-SAS credential UI lives on the secure Winlogon desktop.
        // The SAS is sent at most twice per lock episode, and only while a
        // client session is live — never at boot or on an idle machine.
        let session_id = unsafe { WTSGetActiveConsoleSessionId() };
        let locked = session_id != 0xFFFF_FFFF && console_session_locked(session_id);
        if !locked {
            unlock_armed = false;
            sas_sent = 0;
        } else if slot.has_live_agent() {
            if !unlock_armed {
                unlock_armed = true;
                maybe_arm_auto_unlock(&creds_slot);
            }
            if sas_sent < 2 {
                match crate::sas::send_sas() {
                    Ok(()) => {
                        info!("console locked; synthetic SAS sent to reach the credential prompt")
                    }
                    Err(err) => {
                        warn!(error = %format!("{err:#}"), "cannot send the synthetic SAS")
                    }
                }
                sas_sent += 1;
            }
        }
        // Lab trigger (§18.41): one secure-desktop probe per flag file.
        if std::path::Path::new(PROBE_FLAG).exists() {
            let _ = std::fs::remove_file(PROBE_FLAG);
            match spawn_probe_in_console() {
                Ok(Some(pid)) => info!(pid, "secure-desktop probe spawned"),
                Ok(None) => info!("secure-desktop probe requested but there is no console session"),
                Err(err) => {
                    warn!(error = %format!("{err:#}"), "cannot spawn the secure-desktop probe");
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    // Never leave a secure helper behind on service stop.
    if let Some((_, process)) = helper.take() {
        // SAFETY: the process handle is ours and open.
        unsafe {
            let _ = TerminateProcess(process.0, 0);
        }
    }
    slot.kill();
}
