// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Self-installation: `udsrp-service --install` / `--uninstall`.
//!
//! Registers (or removes) the service in the SCM with auto-start and the M5
//! failure recovery policy: restart the service 5s after a crash, three
//! times, with the failure counter reset after a quiet day.

use anyhow::{Context, Result};
use tracing::info;
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, CloseServiceHandle, ControlService, CreateServiceW, DeleteService,
    OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_ACTION, SC_HANDLE,
    SC_MANAGER_CREATE_SERVICE, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_STOP, SERVICE_ERROR_NORMAL,
    SERVICE_FAILURE_ACTIONSW, SERVICE_STATUS, SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS,
    StartServiceW,
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
    let mut actions = [SC_ACTION {
        Type: windows::Win32::System::Services::SC_ACTION_RESTART,
        Delay: 5000,
    }; 3];
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
    // Stopping first avoids the "marked for deletion" limbo when the service
    // is running (the process holds the binary locked until it exits).
    stop().ok();
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

/// Start the service (no-op if already running).
pub fn start() -> Result<()> {
    logging::init();
    let scm = ScHandle(
        unsafe { OpenSCManagerW(None, None, SC_MANAGER_CREATE_SERVICE) }
            .context("OpenSCManager (run elevated?)")?,
    );
    let service = ScHandle(
        unsafe { OpenServiceW(scm.0, &HSTRING::from(SERVICE_NAME), SERVICE_ALL_ACCESS) }
            .context("OpenService (is it installed?)")?,
    );
    unsafe { StartServiceW(service.0, None) }.context("StartService")?;
    info!("service start requested");
    println!("started {SERVICE_NAME}");
    Ok(())
}

/// Stop the service and wait (up to ~30s) until it is fully stopped, so a
/// following delete/replace of the binary does not race a dying process.
pub fn stop() -> Result<()> {
    logging::init();
    let scm = ScHandle(
        unsafe { OpenSCManagerW(None, None, SC_MANAGER_CREATE_SERVICE) }
            .context("OpenSCManager (run elevated?)")?,
    );
    let service = ScHandle(
        unsafe { OpenServiceW(scm.0, &HSTRING::from(SERVICE_NAME), SERVICE_ALL_ACCESS) }
            .context("OpenService (is it installed?)")?,
    );
    let mut status = SERVICE_STATUS::default();
    unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) }
        .context("ControlService(STOP)")?;
    for _ in 0..300 {
        let mut status = SERVICE_STATUS::default();
        unsafe { QueryServiceStatus(service.0, &mut status) }.context("QueryServiceStatus")?;
        if status.dwCurrentState == SERVICE_STOPPED {
            info!("service stopped");
            println!("stopped {SERVICE_NAME}");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    anyhow::bail!("timed out waiting for {SERVICE_NAME} to stop")
}
