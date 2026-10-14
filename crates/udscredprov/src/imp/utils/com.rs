//! COM utilities: task-allocated strings and Global Interface Table (GIT)
//! helpers. Taken from the reference provider almost verbatim — LogonUI
//! callbacks arrive on threads where the original interface pointers cannot
//! be used directly, so event interfaces are marshalled through the GIT.

use std::{mem, ptr};
use widestring::U16CString;

use windows::{
    Win32::System::Com::{CoCreateInstance, CoTaskMemAlloc, IGlobalInterfaceTable},
    core::{GUID, IUnknown, Interface, PWSTR},
};

/// CLSID of the standard Global Interface Table.
pub const CLSID_STD_GLOBAL_INTERFACE_TABLE: GUID =
    GUID::from_u128(0x0000_0323_0000_0000_C000_0000_0000_0046);

/// Errors allocating a COM string.
#[derive(Debug)]
pub enum AllocPwstrError {
    /// The string contains an interior NUL and cannot be converted to UTF-16.
    InvalidString,
    /// `CoTaskMemAlloc` returned null.
    AllocationFailed,
}

/// Allocates a COM task-memory `PWSTR` copy of `s`.
pub fn alloc_pwstr(s: &str) -> Result<PWSTR, AllocPwstrError> {
    let wide = U16CString::from_str(s).map_err(|_| AllocPwstrError::InvalidString)?;
    let len = wide.len() + 1;

    unsafe {
        // Allow for null terminator
        let mem = CoTaskMemAlloc(len * mem::size_of::<u16>()).cast::<u16>();
        if mem.is_null() {
            return Err(AllocPwstrError::AllocationFailed);
        }
        ptr::copy_nonoverlapping(wide.as_ptr(), mem, len);
        Ok(PWSTR(mem))
    }
}

/// Retrieves the process-wide Global Interface Table (GIT) instance.
/// COM must be initialized on the current thread before calling.
pub fn get_git() -> windows::core::Result<IGlobalInterfaceTable> {
    unsafe {
        CoCreateInstance(
            &CLSID_STD_GLOBAL_INTERFACE_TABLE,
            None,
            windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
        )
    }
}

/// Registers a COM interface in the GIT, returning its cookie.
pub fn register_in_git<T: Interface>(interface: T) -> windows::core::Result<u32> {
    let git = get_git()?;
    let iunk = interface.cast::<IUnknown>()?;
    let cookie = unsafe { git.RegisterInterfaceInGlobal(&iunk, &T::IID)? };
    Ok(cookie)
}

/// Removes a previously registered interface from the GIT.
pub fn unregister_from_git(cookie: u32) -> windows::core::Result<()> {
    let git = get_git()?;
    unsafe { git.RevokeInterfaceFromGlobal(cookie)? };
    Ok(())
}

/// Retrieves a COM interface from the GIT using its cookie.
pub fn get_from_git<T: Interface>(cookie: u32) -> windows::core::Result<T> {
    let mut ptr = std::ptr::null_mut();
    let git = get_git()?;
    unsafe { git.GetInterfaceFromGlobal(cookie, &T::IID, &mut ptr)? };
    Ok(unsafe { T::from_raw(ptr.cast()) })
}
