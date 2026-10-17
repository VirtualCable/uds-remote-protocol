//! Service entry points: SCM dispatch (production) and `--console` (debug).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{error, info};
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP,
    SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE,
    SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus,
    StartServiceCtrlDispatcherW,
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
pub fn run_console() -> Result<()> {
    logging::init();
    info!("console mode: running service body in the foreground (Ctrl+C to stop)");
    service_body();
    Ok(())
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
    service_body();

    report_status(status_handle, SERVICE_STOPPED)?;
    info!("service stopped");
    Ok(())
}

/// M0 service body: idle loop until a stop is requested. M1+ attaches the
/// IddCx monitor, spawns the session agent and runs the watchdog here.
fn service_body() {
    while !STOP_REQUESTED.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(250));
    }
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
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        ..Default::default()
    };
    // SAFETY: `status` is a valid, initialized struct; SetServiceStatus copies it.
    unsafe { SetServiceStatus(handle, &status) }.context("SetServiceStatus")?;
    Ok(())
}
