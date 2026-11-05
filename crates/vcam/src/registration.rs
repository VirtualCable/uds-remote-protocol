// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! COM self-registration: CLSID + InprocServer32 + the
//! `CLSID_VideoInputDeviceCategory` instance entry, so camera-consuming
//! applications enumerate us.

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, REG_SZ, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
};
use windows::core::{HSTRING, Result};

use crate::globals;

/// `CLSID_VideoInputDeviceCategory` (860BB310-5D01-11D0-BD3B-00A0C911CE86).
const VIDEO_INPUT_CATEGORY: &str = "{860BB310-5D01-11D0-BD3B-00A0C911CE86}";

fn clsid_string() -> String {
    format!("{:?}", globals::CLSID_UDS_VCAM) // GUID Debug prints {XXXXXXXX-...}
}

/// Path of this DLL (for `InprocServer32`).
fn module_path() -> Result<String> {
    let mut buf = [0u16; 1024];
    let hmodule = HMODULE(globals::get_instance().0);
    let len = unsafe { GetModuleFileNameW(Some(hmodule), &mut buf) };
    if len == 0 {
        return Err(windows::core::Error::from_thread());
    }
    Ok(String::from_utf16_lossy(&buf[..len as usize]))
}

/// Creates `key_path` under HKCR and returns the open key.
fn create_key(key_path: &str) -> Result<HKEY> {
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_CLASSES_ROOT,
            &HSTRING::from(key_path),
            Some(0),
            None,
            windows::Win32::System::Registry::REG_OPTION_NON_VOLATILE,
            windows::Win32::System::Registry::KEY_WRITE,
            None,
            &mut key,
            None,
        )
        .ok()?;
    }
    Ok(key)
}

/// Sets a REG_SZ value; `None` name = the default value.
fn set_string(key: HKEY, name: Option<&str>, value: &str) -> Result<()> {
    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let name_w: Option<Vec<u16>> = name.map(|n| n.encode_utf16().chain(std::iter::once(0)).collect());
    let name_ptr = name_w
        .as_deref()
        .map_or_else(windows::core::PCWSTR::null, |p| windows::core::PCWSTR(p.as_ptr()));
    unsafe {
        RegSetValueExW(
            key,
            name_ptr,
            Some(0),
            REG_SZ,
            Some(std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2)),
        )
        .ok()
    }
}

/// Writes all registry entries.
pub fn register() -> Result<()> {
    let clsid = clsid_string();
    let path = module_path()?;

    let clsid_key = format!(r"CLSID\{clsid}");
    let key = create_key(&clsid_key)?;
    set_string(key, None, globals::VCAM_NAME)?;
    let inproc = create_key(&format!(r"{clsid_key}\InprocServer32"))?;
    set_string(inproc, None, &path)?;
    set_string(inproc, Some("ThreadingModel"), "Both")?;

    let instance = format!(r"CLSID\{VIDEO_INPUT_CATEGORY}\Instance\{clsid}");
    let key = create_key(&instance)?;
    set_string(key, Some("CLSID"), &clsid)?;
    set_string(key, Some("FriendlyName"), globals::VCAM_NAME)?;
    Ok(())
}

/// Removes all registry entries written by [`register`].
pub fn unregister() -> Result<()> {
    let clsid = clsid_string();
    unsafe {
        RegDeleteTreeW(
            HKEY_CLASSES_ROOT,
            &HSTRING::from(format!(r"CLSID\{VIDEO_INPUT_CATEGORY}\Instance\{clsid}")),
        )
        .ok()?;
        RegDeleteTreeW(HKEY_CLASSES_ROOT, &HSTRING::from(format!(r"CLSID\{clsid}"))).ok()?;
    }
    Ok(())
}
