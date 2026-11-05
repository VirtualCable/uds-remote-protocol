// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Process-wide state for the vcam DLL: CLSID, module instance handle and
//! the COM reference count (same pattern as `udscredprov`).

use std::sync::{
    OnceLock,
    atomic::{AtomicU32, Ordering},
};

use windows::Win32::Foundation::HINSTANCE;
use windows::core::GUID;

/// CLSID of the UDS virtual camera DirectShow source filter.
pub const CLSID_UDS_VCAM: GUID = GUID::from_u128(0x9e1a7c42_3b5d_4f6a_8c1e_2d3f4a5b6c7d);

/// Friendly name shown to applications.
pub const VCAM_NAME: &str = "UDS Webcam";

/// Global DLL reference count (for `DllCanUnloadNow`).
pub static DLL_REF_COUNT: AtomicU32 = AtomicU32::new(0);

static DLL_INSTANCE: OnceLock<SafeHInstance> = OnceLock::new();

#[derive(Clone, Copy)]
struct SafeHInstance(HINSTANCE);

// HINSTANCE is a module base address: safe to share across threads.
unsafe impl Sync for SafeHInstance {}
unsafe impl Send for SafeHInstance {}

/// Stores the module instance; only the first call has an effect.
pub fn set_instance(h: HINSTANCE) {
    DLL_INSTANCE.set(SafeHInstance(h)).ok();
}

/// Returns the stored module instance.
pub fn get_instance() -> HINSTANCE {
    DLL_INSTANCE.get().copied().unwrap_or(SafeHInstance(HINSTANCE::default())).0
}

/// Increments the global DLL reference count.
pub fn dll_add_ref() {
    DLL_REF_COUNT.fetch_add(1, Ordering::SeqCst);
}

/// Decrements the global DLL reference count.
pub fn dll_release() {
    DLL_REF_COUNT.fetch_sub(1, Ordering::SeqCst);
}
