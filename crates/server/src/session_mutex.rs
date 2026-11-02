// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Session mutex: the agent holds `Global\UDSRP-Session` for its whole
//! lifetime. It serves two purposes:
//!
//! - single-instance guard: a second agent fails to acquire ownership;
//! - watchdog signal: the orchestrator service checks the mutex is held
//!   while the agent process is alive; a wedged agent (alive but not
//!   holding it) is killed and respawned.
//!
//! The mutex must be created by the **service** (session 0, LocalSystem):
//! creating a `Global\` object requires `SeCreateGlobalPrivilege`, which a
//! standard session user does not have — from such a user the `Global\`
//! prefix is silently redirected to the session-local namespace, making the
//! mutex invisible to the service (this exact bug caused a kill/respawn
//! loop once the agent ran as a standard user). The agent therefore OPENS
//! the service-created mutex and takes ownership of it. When no
//! service-created mutex exists (dev runs without the service), the agent
//! falls back to a session-local mutex and the watchdog simply cannot see
//! it — degraded, but functional.

use anyhow::{Context, Result, bail};
use tracing::{info, warn};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Threading::{
    CreateMutexW, MUTEX_MODIFY_STATE, OpenMutexW, ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE,
    WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

/// Must match `service::agent::SESSION_MUTEX`.
const SESSION_MUTEX: &str = r"Global\UDSRP-Session";

/// RAII guard holding the session mutex until process exit.
pub struct SessionMutex(HANDLE);

impl Drop for SessionMutex {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and open; we hold ownership.
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

/// Acquires ownership of the session mutex. Fails if another live agent
/// already owns it.
pub fn acquire() -> Result<SessionMutex> {
    let name: Vec<u16> = SESSION_MUTEX.encode_utf16().chain(std::iter::once(0)).collect();
    // Preferred path: open the service-created global mutex (the service has
    // the privilege to create Global\ objects; a standard session user does
    // not) and take ownership of it. Only the rights we use are requested
    // (SYNCHRONIZE to wait, MODIFY_STATE to release): the service's DACL
    // grants Everyone exactly those, nothing more (§18.33).
    // SAFETY: `name` is null-terminated and outlives the call.
    if let Ok(handle) = unsafe {
        OpenMutexW(
            MUTEX_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE,
            false,
            PCWSTR(name.as_ptr()),
        )
    } {
        // SAFETY: `handle` is live and unowned-by-us so far.
        match unsafe { WaitForSingleObject(handle, 0) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => {
                info!("holding session mutex {SESSION_MUTEX}");
                return Ok(SessionMutex(handle));
            }
            WAIT_TIMEOUT => {
                // SAFETY: we never owned it; just close the handle.
                unsafe {
                    let _ = CloseHandle(handle);
                }
                bail!("another udsrp agent already holds {SESSION_MUTEX}; refusing to start");
            }
            other => {
                // SAFETY: `handle` is ours and open.
                unsafe {
                    let _ = CloseHandle(handle);
                }
                bail!("unexpected wait result on the session mutex: {other:?}");
            }
        }
    }
    // Fallback: no service-created mutex (dev run without the service). A
    // Local mutex still gives the single-instance guard inside this session.
    warn!("no service-created session mutex; falling back to a session-local one (degraded watchdog)");
    let mut local: Vec<u16> = SESSION_MUTEX.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `local` is null-terminated and outlives the call; a standard
    // user can always create session-local objects. Note: `Global\` from a
    // user without SeCreateGlobalPrivilege is silently redirected to Local\,
    // which is exactly the session-local behavior we want here.
    let handle = unsafe { CreateMutexW(None, true, PWSTR(local.as_mut_ptr())) }
        .context("CreateMutexW failed")?;
    if unsafe { windows::Win32::Foundation::GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(handle);
        }
        bail!("another udsrp agent already holds {SESSION_MUTEX} (local); refusing to start");
    }
    info!("holding session-local mutex {SESSION_MUTEX}");
    Ok(SessionMutex(handle))
}
