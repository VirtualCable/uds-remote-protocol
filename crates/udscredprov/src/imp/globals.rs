//! Process-wide state for the credential provider DLL: CLSID, module
//! instance handle and the COM reference count.

use std::sync::{
    OnceLock,
    atomic::{AtomicU32, Ordering},
};

use windows::Win32::Foundation::HINSTANCE;
use windows::core::GUID;

/// CLSID of this credential provider. Distinct from the UDS RDP credential
/// provider (`win-credentials-provider` repo), so both coexist on the same
/// machine without clashing.
pub const CLSID_UDS_RP_CREDENTIAL_PROVIDER: GUID =
    GUID::from_u128(0xeffe8c95_8e24_4d7f_bc4a_2c377479cf32);

/// Global DLL reference count (for `DllCanUnloadNow`).
pub static DLL_REF_COUNT: AtomicU32 = AtomicU32::new(0);

static DLL_INSTANCE: OnceLock<SafeHInstance> = OnceLock::new();

#[derive(Clone, Copy)]
struct SafeHInstance(HINSTANCE);

// HINSTANCE is a module base address: safe to share across threads.
unsafe impl Sync for SafeHInstance {}
unsafe impl Send for SafeHInstance {}

/// Stores the module instance; only the first call has an effect
/// (`DllMain` runs before anything else needs it).
pub fn set_instance(h: HINSTANCE) {
    DLL_INSTANCE.set(SafeHInstance(h)).ok();
}

/// Returns the stored module instance.
pub fn get_instance() -> HINSTANCE {
    DLL_INSTANCE.get().expect("DLL_INSTANCE not initialized").0
}

/// Increments the global DLL reference count.
pub fn dll_add_ref() {
    DLL_REF_COUNT.fetch_add(1, Ordering::SeqCst);
}

/// Decrements the global DLL reference count.
pub fn dll_release() {
    DLL_REF_COUNT.fetch_sub(1, Ordering::SeqCst);
}
