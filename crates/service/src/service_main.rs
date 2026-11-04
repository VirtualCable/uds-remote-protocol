// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Service entry points: SCM dispatch (production) and `--console` (debug).

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use tracing::{error, info};
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_STATUS,
    SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

use crate::install::SERVICE_NAME;
use crate::logging;

/// Set by the control handler when the SCM asks us to stop or on shutdown.
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Run as a real service: connect to the SCM dispatcher. Blocks until the
/// service stops. This is the entry point when the SCM starts the process.
pub fn run_dispatch() -> Result<()> {
    logging::init();
    let name = HSTRING::from(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            // The SCM does not mutate the name despite the PWSTR type.
            lpServiceName: PWSTR(name.as_ptr().cast_mut()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    // SAFETY: `table` lives until StartServiceCtrlDispatcherW returns, and
    // `name` outlives it. The dispatcher only reads the table during the call.
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }.context("StartServiceCtrlDispatcher")?;
    Ok(())
}

/// Debug mode: run the service body in the foreground console.
pub fn run_console() {
    logging::init();
    info!("console mode: running service body in the foreground (Ctrl+C to stop)");
    let runtime = crate::standalone::load();
    let creds_slot = std::sync::Arc::new(std::sync::Mutex::new(match &runtime {
        Some(crate::standalone::Runtime::Standalone(standalone)) => standalone.creds.clone(),
        _ => None,
    }));
    crate::credprov::spawn(&STOP_REQUESTED, std::sync::Arc::clone(&creds_slot));
    service_body(runtime, creds_slot);
}

/// `ServiceMain` callback invoked by the SCM dispatcher.
///
/// # Safety
/// Called only by the SCM dispatcher with valid arguments.
unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    logging::init();
    if let Err(err) = service_main_impl() {
        error!(error = %format!("{err:#}"), "service_main failed");
    }
}

fn service_main_impl() -> Result<()> {
    let name = HSTRING::from(SERVICE_NAME);
    let status_handle =
        unsafe { RegisterServiceCtrlHandlerExW(PCWSTR(name.as_ptr()), Some(control_handler), None) }
            .context("RegisterServiceCtrlHandlerEx")?;

    report_status(status_handle, SERVICE_RUNNING)?;
    info!("service started");
    let runtime = crate::standalone::load();
    let creds_slot = std::sync::Arc::new(std::sync::Mutex::new(match &runtime {
        Some(crate::standalone::Runtime::Standalone(standalone)) => standalone.creds.clone(),
        _ => None,
    }));
    crate::credprov::spawn(&STOP_REQUESTED, std::sync::Arc::clone(&creds_slot));
    service_body(runtime, creds_slot);

    report_status(status_handle, SERVICE_STOPPED)?;
    info!("service stopped");
    Ok(())
}

/// Service body: the console state machine (§18.55) owns every console
/// side effect — pre-session sequence for validated connections, agent
/// supervision, secure-desktop helper, auto-unlock and legacy proactive
/// spawn.
fn service_body(
    runtime: Option<crate::standalone::Runtime>,
    creds_slot: std::sync::Arc<std::sync::Mutex<Option<udscredprov::logic::Credentials>>>,
) {
    crate::console_fsm::run(&STOP_REQUESTED, runtime, creds_slot);
}

/// `HandlerEx` callback: the SCM asks us to stop/shutdown.
extern "system" fn control_handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    if control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN {
        info!(control, "stop requested by SCM");
        STOP_REQUESTED.store(true, Ordering::Relaxed);
    }
    0 // NO_ERROR
}

fn report_status(handle: SERVICE_STATUS_HANDLE, state: SERVICE_STATUS_CURRENT_STATE) -> Result<()> {
    // Without ACCEPT_STOP the SCM refuses to send us control requests at
    // all — the service becomes un-stoppable until the process dies.
    let accepted = if state == SERVICE_RUNNING {
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
    } else {
        0
    };
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        ..Default::default()
    };
    // SAFETY: `status` is a valid, initialized struct; SetServiceStatus copies it.
    unsafe { SetServiceStatus(handle, &raw const status) }.context("SetServiceStatus")?;
    Ok(())
}
