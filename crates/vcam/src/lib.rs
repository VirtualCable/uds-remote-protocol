// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! UDS virtual camera: a DirectShow push source filter serving the webcam
//! frames the server agent publishes into the session shared slot
//! (`vcam::ipc`). Registered under `CLSID_VideoInputDeviceCategory`, so
//! applications (Chrome, Teams, Zoom, ...) enumerate it as a camera named
//! "UDS Webcam".
//!
//! Chosen over Media Foundation Virtual Camera because `MFCreateVirtualCamera`
//! requires build 22000+ (Win11/Server 2025) and our VDI targets include
//! Server 2019/2022 (see plan §18.57).

// The windows-crate `#[implement]` macro expansions trigger pedantic lints we
// cannot act on, and the Win32/COM call style is idiomatic for this API —
// same policy as udscredprov/udsaudio-ctl.
#![allow(
    clippy::inline_always,
    clippy::ref_as_ptr,
    clippy::borrow_as_ptr,
    clippy::wildcard_imports,
    clippy::unused_self,
    clippy::needless_pass_by_value,
    clippy::trivially_copy_pass_by_ref
)]

pub mod ipc;
pub mod pixel;

#[cfg(windows)]
mod classfactory;
#[cfg(windows)]
mod filter;
#[cfg(windows)]
mod globals;
#[cfg(windows)]
pub mod log;
#[cfg(windows)]
mod media;
#[cfg(windows)]
mod pin;
#[cfg(windows)]
mod registration;
#[cfg(windows)]
mod state;

#[cfg(windows)]
mod imp {
    use std::sync::atomic::Ordering;

    use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, HINSTANCE, S_FALSE, S_OK};
    use windows::Win32::System::Com::IClassFactory;
    use windows::Win32::System::LibraryLoader::DisableThreadLibraryCalls;
    use windows::Win32::System::SystemServices::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH};
    use windows::core::*;

    use crate::{classfactory, globals, registration};

    /// `DllMain` entry point: stores the module instance.
    ///
    /// # Safety
    /// Called by the loader with valid arguments.
    #[unsafe(no_mangle)]
    pub extern "system" fn DllMain(
        hinst_dll: HINSTANCE,
        fdw_reason: u32,
        _lp_reserved: *mut core::ffi::c_void,
    ) -> BOOL {
        globals::set_instance(hinst_dll);
        if fdw_reason == DLL_PROCESS_ATTACH {
            unsafe {
                let _ = DisableThreadLibraryCalls(hinst_dll.into());
            }
            crate::vlog!("process attach");
        } else if fdw_reason == DLL_PROCESS_DETACH {
            crate::vlog!("process detach");
            globals::set_detached();
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
    #[allow(clippy::not_unsafe_ptr_arg_deref)] // system calls cannot be unsafe
    #[unsafe(no_mangle)]
    pub extern "system" fn DllGetClassObject(
        rclsid: *const GUID,
        riid: *const GUID,
        ppv: *mut *mut core::ffi::c_void,
    ) -> HRESULT {
        unsafe {
            if *rclsid != globals::CLSID_UDS_VCAM {
                return CLASS_E_CLASSNOTAVAILABLE;
            }
            crate::vlog!("DllGetClassObject");
            let factory: IClassFactory = classfactory::ClassFactory::new().into();
            factory.query(riid, ppv)
        }
    }

    /// `regsvr32`-style self-registration.
    ///
    /// # Safety
    /// Called by `regsvr32` (or our installer) with the DLL loaded.
    #[unsafe(no_mangle)]
    pub extern "system" fn DllRegisterServer() -> HRESULT {
        match registration::register() {
            Ok(()) => S_OK,
            Err(e) => e.code(),
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
            Err(e) => e.code(),
        }
    }
}
