//! Self-installation: `udsrp-service --install` / `--uninstall`.
//!
//! Registers (or removes) the service in the SCM with auto-start and the M5
//! failure recovery policy: restart the service 5s after a crash, three
//! times, with the failure counter reset after a quiet day.

use anyhow::{Context, Result};
use tracing::info;
use windows::Win32::System::Services::{
    CloseServiceHandle, CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW,
    SC_ACTION, SC_HANDLE, SC_MANAGER_CREATE_SERVICE, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW,
    SERVICE_WIN32_OWN_PROCESS, ChangeServiceConfig2W,
};
use windows::core::HSTRING;

use crate::logging;

/// SCM service name (as in `sc.exe`).
pub const SERVICE_NAME: &str = "UdsRpService";
/// Human-readable display name.
pub const DISPLAY_NAME: &str = "UDS Remote Protocol Service";

/// RAII wrapper for an `SC_HANDLE` (SCM or service).
struct ScHandle(SC_HANDLE);

impl Drop for ScHandle {
    fn drop(&mut self) {
        // CloseServiceHandle returns BOOL; nothing useful to do on failure.
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}

/// Install the service pointing at the current executable.
pub fn install() -> Result<()> {
    logging::init();
    let exe = std::env::current_exe().context("resolving own executable path")?;
    let exe = exe.to_str().context("executable path is not valid UTF-8")?;

    let scm = ScHandle(
        unsafe { OpenSCManagerW(None, None, SC_MANAGER_CREATE_SERVICE) }
            .context("OpenSCManager (run elevated?)")?,
    );
    let service = unsafe {
        CreateServiceW(
            scm.0,
            &HSTRING::from(SERVICE_NAME),
            &HSTRING::from(DISPLAY_NAME),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            &HSTRING::from(exe),
            None,
            None,
            None,
            None,
            None,
        )
    }
    .context("CreateService")?;
    let _service = ScHandle(service);
    set_failure_actions(service)?;
    info!(exe, "service installed");
    println!("installed {SERVICE_NAME} -> {exe}");
    Ok(())
}

/// SCM failure recovery (M5): restart the service 5s after an unexpected
/// stop, up to three attempts, resetting the failure counter after a day
/// without failures.
fn set_failure_actions(service: SC_HANDLE) -> Result<()> {
    let mut actions = [
        SC_ACTION {
            Type: windows::Win32::System::Services::SC_ACTION_RESTART,
            Delay: 5000,
        };
        3
    ];
    let failure_actions = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 86400,
        lpRebootMsg: windows::core::PWSTR::null(),
        lpCommand: windows::core::PWSTR::null(),
        cActions: u32::try_from(actions.len()).expect("3 actions fit in u32"),
        lpsaActions: actions.as_mut_ptr(),
    };
    // SAFETY: `service` is a valid handle; `failure_actions` and `actions`
    // outlive the call.
    unsafe {
        ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            Some((&raw const failure_actions).cast()),
        )
    }
    .context("ChangeServiceConfig2(FAILURE_ACTIONS)")?;
    info!("failure actions: restart after 5s, x3, counter resets after 1 day");
    Ok(())
}

/// Remove the service from the SCM.
pub fn uninstall() -> Result<()> {
    logging::init();
    let scm = ScHandle(
        unsafe { OpenSCManagerW(None, None, SC_MANAGER_CREATE_SERVICE) }
            .context("OpenSCManager (run elevated?)")?,
    );
    let service = ScHandle(
        unsafe { OpenServiceW(scm.0, &HSTRING::from(SERVICE_NAME), SERVICE_ALL_ACCESS) }
            .context("OpenService (is it installed?)")?,
    );
    unsafe { DeleteService(service.0) }.context("DeleteService")?;
    info!("service uninstalled");
    println!("uninstalled {SERVICE_NAME}");
    Ok(())
}
