//! Self-installation: `udsrp-service --install` / `--uninstall`.
//!
//! Registers (or removes) the service in the SCM with auto-start and a
//! failure recovery policy of "restart on crash" (M5 will revisit the exact
//! recovery options; for now the SCM defaults plus auto-start suffice).

use anyhow::{Context, Result};
use tracing::info;
use windows::Win32::System::Services::{
    CloseServiceHandle, CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW,
    SC_HANDLE, SC_MANAGER_CREATE_SERVICE, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
    SERVICE_ERROR_NORMAL, SERVICE_WIN32_OWN_PROCESS,
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
    info!(exe, "service installed");
    println!("installed {SERVICE_NAME} -> {exe}");
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
