// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! LSA helpers: packing credentials into a `KERB_INTERACTIVE_UNLOCK_LOGON`
//! serialization buffer and resolving the Negotiate authentication package.
//! Taken from the reference provider (the unpack/protect helpers are dropped:
//! this provider never receives pre-serialized credentials).

use widestring::U16CString;
use windows::{
    Win32::{
        Foundation::{E_OUTOFMEMORY, HANDLE, LUID},
        Security::Authentication::Identity::{
            KERB_INTERACTIVE_UNLOCK_LOGON, LSA_STRING, LSA_UNICODE_STRING, LsaConnectUntrusted,
            LsaDeregisterLogonProcess, LsaLookupAuthenticationPackage, NEGOSSP_NAME_A,
        },
        System::Com::CoTaskMemAlloc,
    },
    core::*,
};

/// Owning `LSA_UNICODE_STRING`: keeps the UTF-16 buffer alive alongside the
/// LSA structure pointing into it.
pub struct LsaUnicodeString {
    _keepalive: U16CString,
    lsa: LSA_UNICODE_STRING,
}

impl LsaUnicodeString {
    /// Builds an `LSA_UNICODE_STRING` from a Rust string.
    pub fn new(s: &str) -> Self {
        let wide = U16CString::from_str(s).expect("invalid UTF-16");
        // len() does not count the null terminator
        let len_bytes = u16::try_from(wide.len() * 2).expect("string too long for LSA");
        let lsa = LSA_UNICODE_STRING {
            Length: len_bytes,
            MaximumLength: len_bytes,
            Buffer: PWSTR(wide.as_ptr().cast_mut()),
        };
        Self {
            _keepalive: wide,
            lsa,
        }
    }

    /// Borrows the inner LSA structure.
    pub fn as_lsa(&self) -> &LSA_UNICODE_STRING {
        &self.lsa
    }
}

/// Owning `LSA_STRING` (ANSI).
pub struct LsaString {
    _keepalive: String,
    lsa: LSA_STRING,
}

impl LsaString {
    /// Builds an `LSA_STRING` from a Rust string.
    pub fn new(s: &str) -> Self {
        let keepalive = s.to_string();
        let len = u16::try_from(keepalive.len()).expect("string too long for LSA");
        let lsa = LSA_STRING {
            Length: len,
            MaximumLength: len,
            Buffer: PSTR(keepalive.as_ptr().cast_mut()),
        };
        Self {
            _keepalive: keepalive,
            lsa,
        }
    }

    /// Builds an `LSA_STRING` from a raw `PCSTR`.
    pub fn from_pcstr(pcstr: PCSTR) -> Self {
        let mut len = 0;
        unsafe {
            while *pcstr.0.add(len) != 0 && len < u16::MAX as usize {
                len += 1;
            }
        }
        let slice = unsafe { std::slice::from_raw_parts(pcstr.0, len) };
        let keepalive = String::from_utf8_lossy(slice).to_string();
        Self::new(&keepalive)
    }

    /// Borrows the inner LSA structure.
    pub fn as_lsa(&self) -> &LSA_STRING {
        &self.lsa
    }
}

// Copies one LSA_UNICODE_STRING payload after the struct and rewrites its
// Buffer as a struct-relative offset (as LSA expects).
// Alignment: `base` comes from CoTaskMemAlloc (8-byte aligned) and the cursor
// advances by whole LSA_UNICODE_STRING payloads (even byte counts), so the
// u16 cast is always sufficiently aligned.
#[allow(clippy::cast_ptr_alignment)]
fn copy_and_update_ref(
    src: &LSA_UNICODE_STRING,
    dst_struct: &mut LSA_UNICODE_STRING,
    base: *mut u8,
    buf_cursor: &mut *mut u8,
) {
    unsafe {
        std::ptr::copy_nonoverlapping(
            src.Buffer.0,
            (*buf_cursor).cast::<u16>(),
            (src.Length / 2) as usize,
        );
    }
    debug_assert!(src.Length <= src.MaximumLength);
    debug_assert_eq!(src.Length & 1, 0);

    *dst_struct = *src;
    // The packed struct stores Buffer as a struct-relative offset (isize),
    // not a pointer — an integer-to-pointer cast is intentional here.
    dst_struct.Buffer = PWSTR(unsafe { (*buf_cursor).offset_from(base) } as *mut u16);
    debug_assert!(!dst_struct.Buffer.is_null());

    *buf_cursor = unsafe { (*buf_cursor).add(src.Length as usize) };
}

/// Packs a `KERB_INTERACTIVE_UNLOCK_LOGON` struct and its strings into a
/// single `CoTaskMemAlloc` allocation, the layout WinLogon and LSA consume.
/// The strings are stored right after the struct and their `Buffer` pointers
/// are rewritten as offsets relative to the struct base.
///
/// # Safety
/// `logon` must be valid; the returned pointer is owned by the caller
/// (normally handed to COM, which frees it).
pub unsafe fn kerb_interactive_unlock_logon_pack(
    logon: &KERB_INTERACTIVE_UNLOCK_LOGON,
) -> windows::core::Result<(*mut u8, u32)> {
    let pkil_in = &logon.Logon;

    let cb_total = std::mem::size_of::<KERB_INTERACTIVE_UNLOCK_LOGON>()
        + pkil_in.UserName.Length as usize
        + pkil_in.Password.Length as usize
        + pkil_in.LogonDomainName.Length as usize;

    let pkiul_out = unsafe { CoTaskMemAlloc(cb_total).cast::<KERB_INTERACTIVE_UNLOCK_LOGON>() };
    if pkiul_out.is_null() {
        return Err(E_OUTOFMEMORY.into());
    }
    unsafe {
        std::ptr::write_bytes(pkiul_out.cast::<u8>(), 0xFF, cb_total);
        (*pkiul_out).LogonId = LUID::default();
    }

    let mut pb_buffer =
        unsafe { (pkiul_out.cast::<u8>()).add(std::mem::size_of::<KERB_INTERACTIVE_UNLOCK_LOGON>()) };

    unsafe {
        (*pkiul_out).Logon.MessageType = pkil_in.MessageType;
    }

    copy_and_update_ref(
        &pkil_in.LogonDomainName,
        unsafe { &mut (*pkiul_out).Logon.LogonDomainName },
        pkiul_out.cast(),
        &mut pb_buffer,
    );
    copy_and_update_ref(
        &pkil_in.UserName,
        unsafe { &mut (*pkiul_out).Logon.UserName },
        pkiul_out.cast(),
        &mut pb_buffer,
    );
    copy_and_update_ref(
        &pkil_in.Password,
        unsafe { &mut (*pkiul_out).Logon.Password },
        pkiul_out.cast(),
        &mut pb_buffer,
    );

    debug_assert_eq!(pb_buffer as usize, pkiul_out as usize + cb_total);

    Ok((
        pkiul_out.cast(),
        u32::try_from(cb_total).expect("serialization buffer fits in u32"),
    ))
}

/// Resolves the Negotiate authentication package ID via LSA.
pub fn retrieve_negotiate_auth_package() -> windows::core::Result<u32> {
    let mut lsahandle = HANDLE::default();
    let status = unsafe { LsaConnectUntrusted(&mut lsahandle) };

    if status.to_hresult().is_err() {
        return Err(status.into());
    }
    let nego_name = LsaString::from_pcstr(NEGOSSP_NAME_A);

    let mut authenticationpackage = 0u32;
    let status = unsafe {
        LsaLookupAuthenticationPackage(lsahandle, nego_name.as_lsa(), &mut authenticationpackage)
    };
    _ = unsafe { LsaDeregisterLogonProcess(lsahandle) };
    if status.to_hresult().is_err() {
        Err(status.into())
    } else {
        Ok(authenticationpackage)
    }
}
