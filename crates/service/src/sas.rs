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
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{BOOL, s, w};

/// Sends one synthetic SAS (as a service, not as a user keystroke).
pub fn send_sas() -> Result<()> {
    // SAFETY: `sas.dll` is a Microsoft system DLL present since Vista;
    // the library reference is released before returning.
    let sas: HMODULE = unsafe { LoadLibraryW(w!("sas.dll")) }.context("LoadLibraryW(sas.dll)")?;
    let result = (|| {
        let addr = unsafe { GetProcAddress(sas, s!("SendSAS")) }.context("sas.dll!SendSAS missing")?;
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
