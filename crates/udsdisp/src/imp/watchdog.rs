//! Service-liveness watchdog: a monitor may only outlive a live, mutex-owning
//! orchestrator service.
//!
//! The service holds the named mutex [`logic::WATCHDOG_MUTEX_NAME`] for its
//! whole lifetime. A dedicated driver thread waits on it in a loop and acts on
//! the outcome ([`logic::watchdog_action`]):
//!
//! - timeout → the service owns the mutex → alive → nothing to do;
//! - acquired (normal) → nobody owns it (clean stop, or monitors created by
//!   hand with `udsdisp-ctl` in the lab) → release and touch nothing;
//! - acquired as **abandoned** → the owner died holding it → detach every
//!   monitor, then release.
//!
//! The driver creates the mutex if it does not exist (never owning it), so a
//! service crash is detectable from the very first contact and no orphan
//! monitor can survive a `taskkill /F` — the failure mode that swallowed the
//! console mouse in the lab (see the study, section 18.21).

use std::time::Duration;

use windows::Win32::Foundation::{WAIT_EVENT, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::core::PCWSTR;

use super::{log_line, monitor, shared};
use crate::logic;

/// `WAIT_ABANDONED` (0x00000080): acquired because the previous owner died.
const WAIT_ABANDONED: WAIT_EVENT = WAIT_EVENT(0x0000_0080);

/// How long each wait blocks. With the mutex owned by the service, every wait
/// runs the full timeout, so this doubles as the loop period: a dead service
/// is detected in under `WAIT_TIMEOUT` plus one depart pass.
const WAIT_TIMEOUT_MS: u32 = 2_000;

/// Pause after acquiring the mutex (any reason) before waiting again: gives a
/// restarting service a wide window to take ownership without racing us.
const POST_ACQUIRE_PAUSE: Duration = Duration::from_secs(8);

/// Spawns the watchdog thread (idempotent). Called from the adapter
/// init-finished callback, where IddCx is fully operational.
pub fn spawn() {
    static START: std::sync::Once = std::sync::Once::new();
    START.call_once(|| {
        std::thread::spawn(run);
    });
}

fn run() {
    let wide: Vec<u16> = logic::WATCHDOG_MUTEX_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // Create-if-missing without owning (`false`): the service is the owner;
    // we only wait on it. If creation fails there is nothing sane to wait
    // on — log and give up (attach/detach IOCTLs keep working regardless).
    // SAFETY: `wide` is a valid null-terminated name; default security.
    let mutex = unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) };
    let mutex = match mutex {
        Ok(handle) if !handle.is_invalid() => handle,
        _ => {
            log_line("watchdog: CreateMutexW failed; watchdog disabled");
            return;
        }
    };
    log_line("watchdog: monitoring service owner mutex");
    loop {
        // SAFETY: `mutex` is a live handle owned by this thread.
        let wait = unsafe { WaitForSingleObject(mutex, WAIT_TIMEOUT_MS) };
        let outcome = if wait == WAIT_OBJECT_0 {
            Some(logic::WaitOutcome::Acquired)
        } else if wait == WAIT_ABANDONED {
            Some(logic::WaitOutcome::Abandoned)
        } else {
            None
        };
        let Some(outcome) = outcome else {
            continue; // WAIT_TIMEOUT: service alive and owning.
        };
        if logic::watchdog_action(outcome) == logic::WatchdogAction::DetachAll {
            detach_all();
        }
        // SAFETY: the wait acquired the mutex, so we must release it.
        if let Err(err) = unsafe { ReleaseMutex(mutex) } {
            log_line(&format!("watchdog: ReleaseMutex failed: {err:?}"));
        }
        std::thread::sleep(POST_ACQUIRE_PAUSE);
    }
    // The loop never exits; the handle dies with the host process.
}

/// Detaches every connected monitor (the dead service can no longer do it).
fn detach_all() {
    let connectors: Vec<u32> = {
        let state = shared::lock();
        state.monitors.iter().map(|m| m.connector_index).collect()
    };
    if connectors.is_empty() {
        log_line("watchdog: service owner mutex abandoned; no monitors to detach");
        return;
    }
    log_line(&format!(
        "watchdog: service owner mutex abandoned; detaching {} monitor(s)",
        connectors.len()
    ));
    for connector in connectors {
        let status = monitor::depart(connector);
        log_line(&format!(
            "watchdog: depart connector={connector} -> {status:#010x}"
        ));
    }
}
