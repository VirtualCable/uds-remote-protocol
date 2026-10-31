//! M6 attach-on-demand: virtual monitor lifecycle driven by remote sessions.
//!
//! The adapter device is **enabled** for as long as the service runs (a
//! disabled adapter keeps console agents like vdagent ≤0.10 happy while
//! nobody is connected), but the virtual monitor is only **attached** while
//! a remote client is connected, plus a lazy idle timeout afterwards. The
//! session agent signals "a client is connected" by setting the named
//! manual-reset event `Global\UDSRP-RemoteSession`; when the agent dies the
//! handle closes and the event disappears, which we treat as "not signaled".
//!
//! The lazy detach (default 10 minutes, overridable for the lab through
//! `C:\ProgramData\UdsRp\display-idle-secs.txt`) avoids flapping on quick
//! reconnects and limits how often we poke the display stack, which does
//! not tolerate endless attach/detach cycles gracefully.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tracing::{error, info, warn};
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    OpenEventW, SYNCHRONIZATION_ACCESS_RIGHTS, WaitForSingleObject,
};

/// SYNCHRONIZE access right: enough to wait on the event, we never set/reset it.
const SYNCHRONIZE: SYNCHRONIZATION_ACCESS_RIGHTS = SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000);
use windows::core::HSTRING;

use crate::adapter;

/// Named event the session agent sets while a remote client is connected.
const REMOTE_SESSION_EVENT: &str = r"Global\UDSRP-RemoteSession";

/// How often the manager re-evaluates the signal.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Default time the monitor stays attached after the last client left.
const DEFAULT_IDLE: Duration = Duration::from_secs(600);

/// Lab hook: a plain integer in this file overrides the idle timeout.
const IDLE_OVERRIDE_FILE: &str = r"C:\ProgramData\UdsRp\display-idle-secs.txt";

/// Spawn the display manager thread. It enables the adapter, reclaims any
/// orphaned monitors from a crashed previous instance, and then attaches /
/// detaches the virtual monitor following the remote-session signal. On
/// stop it detaches the monitor and disables the adapter again.
///
/// `attach_on_session` gates the whole attach/detach cycle: sessions that
/// capture the **physical console** (`output_name` pointing at the console
/// adapter) do not need the virtual monitor at all, and attaching it on
/// every session only destabilizes single-head console drivers (QXL loses
/// its desktop duplication for good after enough topology churn) while
/// accumulating ghost GDI display entries. The adapter itself is still
/// enabled so console agents keep seeing a stable device list.
pub fn spawn(stop: &'static AtomicBool, attach_on_session: bool) {
    std::thread::spawn(move || run(stop, attach_on_session));
}

fn run(stop: &AtomicBool, attach_on_session: bool) {
    let adapter_enabled = match adapter::set_enabled(true) {
        Ok(found) => {
            if !found {
                warn!("udsdisp adapter not found; virtual display disabled");
            }
            found
        }
        Err(err) => {
            error!(error = %format!("{err:#}"), "cannot enable the udsdisp adapter");
            false
        }
    };

    let mut state = ManagerState::default();
    if adapter_enabled {
        // A crashed previous instance can leave monitors attached (the IOCTL
        // channel outlives its creator); start from a clean slate.
        if let Some(client) = open_client() {
            match client.remove_all() {
                Ok(0) => {}
                Ok(n) => warn!(removed = n, "reclaimed orphaned virtual monitors"),
                Err(err) => warn!(error = %format!("{err:#}"), "cannot list/remove existing monitors"),
            }
        }
    }

    while !stop.load(Ordering::Relaxed) {
        let signaled = attach_on_session && adapter_enabled && remote_session_signaled();
        if signaled {
            state.last_signal = Instant::now();
            if !state.attached {
                state.attach();
            }
        } else if state.attached && state.last_signal.elapsed() > idle_timeout() {
            state.detach();
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    if state.attached {
        state.detach();
    }
    if adapter_enabled {
        match adapter::set_enabled(false) {
            Ok(_) => info!("udsdisp adapter disabled"),
            Err(err) => warn!(error = %format!("{err:#}"), "cannot disable the udsdisp adapter"),
        }
    }
}

struct ManagerState {
    client: Option<udsdisp_client::client::Client>,
    attached: bool,
    /// Set on first signal; `Instant::now()` default keeps a never-signaled
    /// manager from detaching (it has nothing attached anyway).
    last_signal: Instant,
}

impl Default for ManagerState {
    fn default() -> Self {
        Self {
            client: None,
            attached: false,
            last_signal: Instant::now(),
        }
    }
}

impl ManagerState {
    fn attach(&mut self) {
        if self.client.is_none() {
            self.client = open_client();
        }
        let Some(client) = &self.client else {
            return;
        };
        let mode = udsdisp_client::client::Mode::new(1920, 1080, 60);
        // The attach can race the driver stack: a failed attempt may still
        // leave the monitor connected behind our back, so re-check the
        // connector list after every error before retrying.
        for _ in 0..20 {
            match client.create(0, mode) {
                Ok(()) => {
                    info!(mode = ?mode, "virtual monitor attached on connector 0");
                    self.attached = true;
                    return;
                }
                Err(err) => {
                    if monitor_connected(client) {
                        warn!(error = %format!("{err:#}"), "attach reported an error but the monitor is connected; continuing");
                        self.attached = true;
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
        error!("cannot attach virtual monitor after retries");
    }

    fn detach(&mut self) {
        // Drop the client handle BEFORE touching the adapter: an open
        // device-interface handle makes a later PnP query-remove fail.
        if let Some(client) = self.client.take() {
            match client.remove(0) {
                Ok(()) => info!("virtual monitor detached (idle)"),
                Err(err) => warn!(error = %format!("{err:#}"), "cannot detach virtual monitor"),
            }
            drop(client);
        }
        self.attached = false;
    }
}

/// Open the udsdisp device interface, retrying briefly while PnP settles.
fn open_client() -> Option<udsdisp_client::client::Client> {
    for _ in 0..20 {
        if let Ok(client) = udsdisp_client::client::Client::open() {
            return Some(client);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    warn!("udsdisp device interface never appeared");
    None
}

/// Whether connector 0 currently has a connected virtual monitor.
fn monitor_connected(client: &udsdisp_client::client::Client) -> bool {
    client
        .list()
        .is_ok_and(|mons| mons.iter().any(|m| m.connector_index == 0 && m.connected))
}

/// True while the session agent holds `Global\UDSRP-RemoteSession` signaled.
/// A missing event (no agent alive) is "not signaled".
fn remote_session_signaled() -> bool {
    let name = HSTRING::from(REMOTE_SESSION_EVENT);
    let Ok(handle) = (unsafe { OpenEventW(SYNCHRONIZE, false, &name) }) else {
        return false;
    };
    let signaled = unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0;
    // SAFETY: owned handle opened above.
    if let Err(err) = unsafe { CloseHandle(handle) } {
        warn!(error = %format!("{err:#}"), "cannot close remote-session event handle");
    }
    signaled
}

fn idle_timeout() -> Duration {
    match std::fs::read_to_string(IDLE_OVERRIDE_FILE)
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
    {
        Some(secs) => Duration::from_secs(secs),
        None => DEFAULT_IDLE,
    }
}

