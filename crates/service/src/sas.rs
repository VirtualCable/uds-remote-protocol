// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Synthetic Secure Attention Sequence (Ctrl+Alt+Del) via `sas.dll`.
//!
//! When CAD (the secure attention prompt) is required — always on Server
//! SKUs, and on clients with the GPO enabled — LogonUI does not enumerate
//! credential providers until a SAS arrives, so our credential provider
//! never gets the chance to auto-log the console session on. Services
//! running as LocalSystem may synthesize the SAS themselves with
//! `sas.dll!SendSAS` (the supported mechanism remote-access products use
//! for exactly this); on client SKUs without CAD the auto-logon fires on
//! its own and this is never needed.
//!
//! Loaded dynamically so the service still runs on hypothetical images
//! without `sas.dll` (the failure is logged, never fatal).

use anyhow::{Context, Result};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{BOOL, s, w};

/// Sends one synthetic SAS (as a service, not as a user keystroke).
pub fn send_sas() -> Result<()> {
    // SAFETY: `sas.dll` is a Microsoft system DLL present since Vista;
    // the library reference is released before returning. The restricted
    // search flag below is what makes that assumption true: a bare-name
    // `LoadLibraryW` would also search the *application directory* (the
    // folder holding `udsrp-service.exe`), the current working directory
    // and `PATH`, so a DLL planted next to the service binary would run
    // with the service's SYSTEM privileges. `LOAD_LIBRARY_SEARCH_SYSTEM32`
    // confines the search to the system directory, where Microsoft's own
    // `sas.dll` lives.
    let sas: HMODULE = unsafe { LoadLibraryExW(w!("sas.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
        .context("LoadLibraryExW(sas.dll)")?;
    let result = (|| {
        let addr =
            unsafe { GetProcAddress(sas, s!("SendSAS")) }.context("sas.dll!SendSAS missing")?;
        let send_sas: unsafe extern "system" fn(BOOL) = unsafe { std::mem::transmute(addr) };
        // FALSE: the SAS is simulated by a service, not sent on behalf of
        // the logged-on user.
        unsafe { send_sas(BOOL(0)) };
        Ok(())
    })();
    // SAFETY: `sas` was obtained above; the function pointer is not used
    // after this point.
    unsafe {
        let _ = FreeLibrary(sas);
    }
    result
}
