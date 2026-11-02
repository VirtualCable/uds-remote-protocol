// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Windows implementation of the UDS Credential Provider (auto-logon).
//!
//! Mirrors the interface structure of the reference provider in the
//! `win-credentials-provider` repository (`classfactory`, `credentials`
//! provider/credential/filter/types/fields, `utils` com/safe/helpers/log/lsa),
//! but the credential source is the local `uds-rp` service over the
//! `\\.\pipe\udsrp-credprov` named pipe (see `crate::logic`) instead of HTTP
//! calls to the UDS broker.

// The windows-crate `#[implement]` macro expansions trigger pedantic lints we
// cannot act on (inline_always, ref_as_ptr), and the Win32/COM call style is
// idiomatic for this API (implicit borrows as raw pointers, `windows::core::*`
// wildcards, fixed COM signatures that do not use `self` or take small
// wrappers by value) — same borrow_as_ptr policy as udsaudio-ctl.
#![allow(
    clippy::inline_always,
    clippy::ref_as_ptr,
    clippy::borrow_as_ptr,
    clippy::wildcard_imports,
    clippy::unused_self,
    clippy::needless_pass_by_value,
    clippy::trivially_copy_pass_by_ref
)]

mod classfactory;
mod credentials;
mod globals;
mod pipe;
mod utils;

use std::sync::atomic::Ordering;

use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, HINSTANCE, S_FALSE, S_OK};
use windows::Win32::System::Com::IClassFactory;
use windows::Win32::System::LibraryLoader::DisableThreadLibraryCalls;
use windows::Win32::System::SystemServices::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH};
use windows::core::*;

use crate::debug_flow;
use utils::log;

/// `DllMain` entry point: stores the module instance and sets up logging.
///
/// # Safety
/// Called by the loader with valid arguments.
#[unsafe(no_mangle)]
pub extern "system" fn DllMain(
    hinst_dll: HINSTANCE,
    fdw_reason: u32,
    _lp_reserved: *mut core::ffi::c_void,
) -> BOOL {
    // Store the instance for later retrieval
    globals::set_instance(hinst_dll);
    unsafe {
        match fdw_reason {
            DLL_PROCESS_ATTACH => {
                let _ = DisableThreadLibraryCalls(hinst_dll.into());
                log::setup_logging("info");
                debug_flow!("DLL_PROCESS_ATTACH");
            }
            DLL_PROCESS_DETACH => {
                debug_flow!("DLL_PROCESS_DETACH");
            }
            _ => {}
        }
    }
    BOOL::from(true)
}

/// Standard COM unload check.
///
/// # Safety
/// Called by COM; no pointers involved.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if globals::DLL_REF_COUNT.load(Ordering::SeqCst) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

/// Standard COM class-object entry point.
///
/// # Safety
/// `rclsid`, `riid` and `ppv` are provided by COM and are valid for the call.
#[allow(clippy::not_unsafe_ptr_arg_deref)] // System calls cannot be unsafe
#[unsafe(no_mangle)]
pub extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut core::ffi::c_void,
) -> HRESULT {
    unsafe {
        if *rclsid != globals::CLSID_UDS_RP_CREDENTIAL_PROVIDER {
            return CLASS_E_CLASSNOTAVAILABLE;
        }

        let factory: IClassFactory = classfactory::ClassFactory::new().into();
        factory.query(riid, ppv)
    }
}

/// `regsvr32`-style self-registration: writes the CLSID, `InprocServer32`
/// and the Credential Provider (+ Filter) entries.
///
/// # Safety
/// Called by `regsvr32` (or our installer) with the DLL loaded.
#[unsafe(no_mangle)]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    match registration::register() {
        Ok(()) => S_OK,
        Err(e) => {
            log::error!("DllRegisterServer failed: {e:?}");
            e.code()
        }
    }
}

/// Removes everything [`DllRegisterServer`] wrote.
///
/// # Safety
/// Called by `regsvr32 /u` (or our uninstaller) with the DLL loaded.
#[unsafe(no_mangle)]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    match registration::unregister() {
        Ok(()) => S_OK,
        Err(e) => {
            log::error!("DllUnregisterServer failed: {e:?}");
            e.code()
        }
    }
}

/// COM self-registration for the credential provider.
///
/// The reference provider registers via `.reg` files; we implement
/// `DllRegisterServer`/`DllUnregisterServer` so the DLL can be registered
/// with `regsvr32` and, later, by the product installer without extra files.
mod registration {
    use windows::Win32::Foundation::E_INVALIDARG;
    use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CLASSES_ROOT, HKEY_LOCAL_MACHINE, REG_SZ, RegCloseKey, RegCreateKeyExW,
        RegDeleteTreeW, RegSetValueExW,
    };
    use windows::core::*;

    use super::globals;

    /// Registry string form of our CLSID (with braces).
    const CLSID_STR: &str = "{effe8c95-8e24-4d7f-bc4a-2c377479cf32}";
    const PROVIDER_NAME: &str = "UDSRemoteProtocolCredentialProvider";
    const CRED_PROVIDERS_KEY: &str =
        "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Authentication\\Credential Providers";
    const CRED_PROVIDER_FILTERS_KEY: &str =
        "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Authentication\\Credential Provider Filters";

    /// Creates `root\path` and sets a `REG_SZ` value on it.
    fn set_key_string(root: HKEY, path: &str, value_name: Option<&str>, data: &str) -> Result<()> {
        let path_w = widestring::U16CString::from_str(path)
            .map_err(|_| Error::from_hresult(E_INVALIDARG))?;
        let data_w = widestring::U16CString::from_str(data)
            .map_err(|_| Error::from_hresult(E_INVALIDARG))?;
        let value_name_w = value_name
            .map(widestring::U16CString::from_str)
            .transpose()
            .map_err(|_| Error::from_hresult(E_INVALIDARG))?;

        let mut key = HKEY::default();
        unsafe {
            RegCreateKeyExW(
                root,
                PCWSTR::from_raw(path_w.as_ptr()),
                Some(0),
                PCWSTR::null(),
                windows::Win32::System::Registry::REG_OPTION_NON_VOLATILE,
                windows::Win32::System::Registry::KEY_WRITE,
                None,
                &mut key,
                None,
            )
            .ok()?;

            let result = RegSetValueExW(
                key,
                value_name_w
                    .as_ref()
                    .map_or(PCWSTR::null(), |v| PCWSTR::from_raw(v.as_ptr())),
                Some(0),
                REG_SZ,
                Some(std::slice::from_raw_parts(
                    data_w.as_ptr().cast::<u8>(),
                    (data_w.len() + 1) * 2,
                )),
            );
            let _ = RegCloseKey(key);
            result.ok()?;
        }
        Ok(())
    }

    fn delete_tree(root: HKEY, path: &str) -> Result<()> {
        let path_w = widestring::U16CString::from_str(path)
            .map_err(|_| Error::from_hresult(E_INVALIDARG))?;
        unsafe { RegDeleteTreeW(root, PCWSTR::from_raw(path_w.as_ptr())).ok() }
    }

    /// Full path of this DLL, used as the `InprocServer32` value.
    fn module_path() -> Result<String> {
        let mut buf = [0u16; 1024];
        let len = unsafe { GetModuleFileNameW(Some(globals::get_instance().into()), &mut buf) };
        if len == 0 {
            return Err(Error::from_thread());
        }
        Ok(String::from_utf16_lossy(&buf[..len as usize]))
    }

    /// Writes all registry entries for the credential provider and filter.
    pub fn register() -> Result<()> {
        let dll_path = module_path()?;
        let clsid_key = format!("CLSID\\{CLSID_STR}");

        set_key_string(HKEY_CLASSES_ROOT, &clsid_key, None, PROVIDER_NAME)?;
        set_key_string(
            HKEY_CLASSES_ROOT,
            &format!("{clsid_key}\\InprocServer32"),
            None,
            &dll_path,
        )?;
        set_key_string(
            HKEY_CLASSES_ROOT,
            &format!("{clsid_key}\\InprocServer32"),
            Some("ThreadingModel"),
            "Apartment",
        )?;
        set_key_string(
            HKEY_LOCAL_MACHINE,
            &format!("{CRED_PROVIDERS_KEY}\\{CLSID_STR}"),
            None,
            PROVIDER_NAME,
        )?;
        set_key_string(
            HKEY_LOCAL_MACHINE,
            &format!("{CRED_PROVIDER_FILTERS_KEY}\\{CLSID_STR}"),
            None,
            PROVIDER_NAME,
        )?;
        Ok(())
    }

    /// Removes all registry entries written by [`register`].
    pub fn unregister() -> Result<()> {
        delete_tree(
            HKEY_LOCAL_MACHINE,
            &format!("{CRED_PROVIDERS_KEY}\\{CLSID_STR}"),
        )?;
        delete_tree(
            HKEY_LOCAL_MACHINE,
            &format!("{CRED_PROVIDER_FILTERS_KEY}\\{CLSID_STR}"),
        )?;
        delete_tree(HKEY_CLASSES_ROOT, &format!("CLSID\\{CLSID_STR}"))?;
        Ok(())
    }
}
