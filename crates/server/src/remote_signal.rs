//! Remote-session signal (M6): a named manual-reset event,
//! `Global\UDSRP-RemoteSession`, raised by the agent while a remote client
//! session is active. The service polls it to attach the IddCx virtual
//! monitor on demand (and detach it after an idle timeout), so the physical
//! console is only degraded while a remote session actually runs.
//!
//! The event is a kernel object: if the agent dies (crash, kill), the handle
//! is closed, the object is destroyed and the service sees "not signaled" —
//! no stale state, matching the kill-on-close job design (M5).

use anyhow::{Context as _, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent};
use windows::core::PCWSTR;

/// Event name. `Global\` so the service (session 0) and the agent (user
/// session) refer to the same object.
pub const REMOTE_SESSION_EVENT: &str = r"Global\UDSRP-RemoteSession";

/// RAII guard: the event stays set while the guard lives.
pub struct RaisedSignal {
    event: HANDLE,
}

/// Creates (or opens) the event and sets it. `ResetEvent` + close on drop.
pub fn raise() -> Result<RaisedSignal> {
    let wide: Vec<u16> = REMOTE_SESSION_EVENT
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is null-terminated and outlives the call; null security
    // attributes give the default DACL (same-user + SYSTEM can open it).
    let event = unsafe { CreateEventW(None, true, false, PCWSTR(wide.as_ptr())) }
        .context("CreateEvent(UDSRP-RemoteSession)")?;
    // SAFETY: `event` is a valid handle we own.
    unsafe { SetEvent(event) }.context("SetEvent(UDSRP-RemoteSession)")?;
    Ok(RaisedSignal { event })
}

impl Drop for RaisedSignal {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and open.
        unsafe {
            let _ = ResetEvent(self.event);
            let _ = CloseHandle(self.event);
        }
    }
}
