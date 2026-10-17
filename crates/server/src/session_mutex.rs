//! Session mutex: the agent holds `Global\UDSRP-Session` for its whole
//! lifetime. It serves two purposes:
//!
//! - single-instance guard: a second agent fails to acquire ownership;
//! - watchdog signal: the orchestrator service checks the mutex is held
//!   while the agent process is alive; a wedged agent (alive but not
//!   holding it) is killed and respawned.
//!
//! Windows-only (the orchestrated model is Windows); creating it is best
//! effort in `--console` style runs? No — when the mutex already exists and
//! is owned, another agent is serving and we must refuse to start.

use anyhow::{Context, Result, bail};
use tracing::info;
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::PWSTR;

/// Must match `service::agent::SESSION_MUTEX`.
const SESSION_MUTEX: &str = r"Global\UDSRP-Session";

/// RAII guard holding the session mutex until process exit.
pub struct SessionMutex(HANDLE);

impl Drop for SessionMutex {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and open.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Creates and owns the session mutex. Fails if another agent already holds
/// it (`ERROR_ALREADY_EXISTS` with an owned mutex means a live instance).
pub fn acquire() -> Result<SessionMutex> {
    let mut name: Vec<u16> = SESSION_MUTEX.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `name` is null-terminated and outlives the call; we request
    // initial ownership.
    let handle = unsafe { CreateMutexW(None, true, PWSTR(name.as_mut_ptr())) }
        .context("CreateMutexW failed")?;
    if unsafe { windows::Win32::Foundation::GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(handle);
        }
        bail!("another udsrp agent already holds {SESSION_MUTEX}; refusing to start");
    }
    info!("holding session mutex {SESSION_MUTEX}");
    Ok(SessionMutex(handle))
}
