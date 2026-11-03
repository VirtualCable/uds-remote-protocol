// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Agent-side secure-desktop watch (§18.41, M9): polls the console input
//! desktop every 250 ms and reports transitions to the session loop, and
//! mirrors the state to the global named event the service supervises (it
//! spawns the secure helper while the event is set).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tracing::{info, warn};

use crate::secure_helper::secure_desktop_active;

/// Poll cadence for the input-desktop watch.
const POLL: Duration = Duration::from_millis(250);

/// Global event mirrored to the service (same literal as
/// `service::agent::SECURE_EVENT`; the crates do not depend on each other).
const SECURE_EVENT: &str = r"Global\UDSRP-SecureDesktop";

/// Running watch; dropping it stops the thread (bounded by [`POLL`]).
pub struct SecureWatch {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl SecureWatch {
    /// Spawns the watch thread. Transitions arrive as `bool` (true = secure
    /// desktop active) on the returned channel.
    pub fn spawn() -> (Self, tokio::sync::mpsc::UnboundedReceiver<bool>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("secure-watch".into())
            .spawn(move || watch_loop(tx, thread_stop));
        let handle = match handle {
            Ok(h) => Some(h),
            Err(e) => {
                warn!("secure watch: spawn failed: {e}");
                None
            }
        };
        (Self { stop, handle }, rx)
    }
}

impl Drop for SecureWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn watch_loop(tx: tokio::sync::mpsc::UnboundedSender<bool>, stop: Arc<AtomicBool>) {
    let event = open_secure_event();
    let mut active = secure_desktop_active();
    let _ = tx.send(active);
    mirror_event(&event, active);
    let mut ticks = 0u32;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(POLL);
        // Diagnosis (§18.52): a lock transition was missed while the capture
        // thread kept seeing the secure desktop — find out whether the probe
        // call blocks (and for how long) or quietly reports `false`.
        let started = std::time::Instant::now();
        let now = secure_desktop_active();
        let elapsed = started.elapsed();
        if elapsed > Duration::from_millis(100) {
            warn!(?elapsed, "secure-desktop probe call blocked");
        }
        ticks += 1;
        if ticks % 40 == 0 {
            info!(active, "secure-watch heartbeat");
        }
        if now != active {
            info!(active = now, "secure-desktop transition");
            active = now;
            mirror_event(&event, active);
            if tx.send(active).is_err() {
                return; // session over
            }
        }
    }
}

/// Opens the global event (created by the service) for set/reset. `None`
/// outside orchestrated runs (dev server without a service): detection still
/// works locally, just no helper is spawned.
fn open_secure_event() -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW};
    use windows::core::HSTRING;
    let name = HSTRING::from(SECURE_EVENT);
    // SAFETY: `name` is null-terminated and outlives the call.
    match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &name) } {
        Ok(h) => Some(h),
        Err(_) => None,
    }
}

fn mirror_event(event: &Option<windows::Win32::Foundation::HANDLE>, active: bool) {
    use windows::Win32::System::Threading::{ResetEvent, SetEvent};
    let Some(h) = event else { return };
    // SAFETY: the event handle is open with MODIFY_STATE.
    let ok = unsafe { if active { SetEvent(*h) } else { ResetEvent(*h) } };
    if let Err(e) = ok {
        warn!(active, "secure-event mirror failed: {e}");
    }
}
