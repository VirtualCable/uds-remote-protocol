//! IddCx FFI: bindgen-generated bindings plus the function-table call
//! wrappers.
//!
//! IddCx follows the WDF function-table model: every `IddCxXxx()` in
//! `IddCx.h` is a FORCEINLINE shim that dispatches through the global
//! `IddFunctions[]` table with `IddDriverGlobals` as first argument
//! (`((PFN_IDDCXXXX) IddFunctions[IddCxXxxTableIndex])(IddDriverGlobals,
//! ...)`), so bindgen cannot export them directly. Both globals are defined
//! by `iddcxstub.lib` (linked in build.rs) and patched by the IddCx class
//! extension when the driver is loaded — the same mechanism WDF uses for
//! `WdfFunctions`, which wdk-sys wraps in `call_unsafe_wdf_function_binding!`.

#[allow(
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    clippy::all,
    missing_docs
)]
pub mod bindings {
    include!(concat!(env!("OUT_DIR"), "/iddcx_bindings.rs"));
}

use bindings::{PFN_IDD_CX, PIDD_DRIVER_GLOBALS};

unsafe extern "C" {
    /// IddCx function dispatch table, defined by `iddcxstub.lib`. Declared as
    /// a zero-length array: the real length lives in the `IddFunctionCount`
    /// global and entries are only reached through the `IDDFUNCENUM` indices
    /// (`*TableIndex` constants in [`bindings`]).
    static IddFunctions: [PFN_IDD_CX; 0];
    /// Per-driver globals passed as first argument to every IddCx function,
    /// defined by `iddcxstub.lib`.
    static IddDriverGlobals: PIDD_DRIVER_GLOBALS;
}

/// Calls the IddCx function at `$index` in the dispatch table.
///
/// The first argument is always `IddDriverGlobals`, exactly like the
/// FORCEINLINE shims in `IddCx.h` do.
///
/// # Safety
///
/// `$pfn` must be the exact `PFN_IDDCX*` typedef declared in `IddCx.h` for
/// the function at `$index`, and every argument must satisfy that function's
/// contract. The table entry must be non-null, which the caller can ensure
/// via `IddCxGetVersion` when using functions newer than the runtime IddCx.
macro_rules! call_iddcx {
    ($pfn:ty, $index:expr, $($arg:expr),* $(,)?) => {{
        // SAFETY: reading the table base address is always valid (the array
        // static is provided by iddcxstub.lib); the $index expression must
        // name a real IddCx function of the IddCx version we target.
        let entry = unsafe { *IddFunctions.as_ptr().add($index as usize) };
        // SAFETY: all PFN_IDDCX* types are thin function pointers with the
        // same representation as PFN_IDD_CX (void(*)(void)); the caller
        // guarantees $pfn matches the table entry's real signature.
        let func: $pfn = unsafe { core::mem::transmute(entry) };
        let func = func.expect("IddCx function table entry is null");
        // SAFETY: guaranteed by the caller (see macro docs).
        unsafe { func(IddDriverGlobals, $($arg),*) }
    }};
}

// Re-exported for the sibling modules that will use it from M2 on.
#[allow(unused_imports)]
pub(crate) use call_iddcx;

use bindings::{
    IDARG_OUT_GETVERSION, IDD_CX_CLIENT_CONFIG, PFN_IDDCXDEVICEINITCONFIG, PFN_IDDCXDEVICEINITIALIZE,
    PFN_IDDCXGETVERSION, _IDDFUNCENUM_IddCxDeviceInitConfigTableIndex,
    _IDDFUNCENUM_IddCxDeviceInitializeTableIndex, _IDDFUNCENUM_IddCxGetVersionTableIndex,
};
use wdk_sys::{NTSTATUS, WDFDEVICE, WDFDEVICE_INIT};

// Our bindgen module generates its own copies of the WDF handle types
// (`WDFDEVICE` etc.); they are opaque pointers with the same layout as
// wdk_sys's, so crossing the boundary is a plain pointer cast.

/// `IddCxDeviceInitConfig`: registers the IDD callback table for a device
/// being initialized. Must be called before `WdfDeviceCreate`.
///
/// # Safety
/// `device_init` must be the pointer received in `EvtDriverDeviceAdd` and
/// `config` must be a valid, correctly sized `IDD_CX_CLIENT_CONFIG`.
pub unsafe fn device_init_config(
    device_init: *mut WDFDEVICE_INIT,
    config: &mut IDD_CX_CLIENT_CONFIG,
) -> NTSTATUS {
    // SAFETY: guaranteed by the caller; the cast between the two opaque
    // WDFDEVICE_INIT pointee types is layout-neutral.
    call_iddcx!(
        PFN_IDDCXDEVICEINITCONFIG,
        _IDDFUNCENUM_IddCxDeviceInitConfigTableIndex,
        device_init.cast(),
        core::ptr::from_mut(config),
    )
}

/// `IddCxDeviceInitialize`: marks the device as an indirect display device.
/// Called right after `WdfDeviceCreate`.
///
/// # Safety
/// `device` must be a valid `WDFDEVICE` created from a device-init that went
/// through [`device_init_config`].
pub unsafe fn device_initialize(device: WDFDEVICE) -> NTSTATUS {
    // SAFETY: guaranteed by the caller (see above for the cast).
    call_iddcx!(
        PFN_IDDCXDEVICEINITIALIZE,
        _IDDFUNCENUM_IddCxDeviceInitializeTableIndex,
        device.cast(),
    )
}

/// `IddCxGetVersion`: returns the IddCx version of the running OS
/// (e.g. 0x1700 = 1.7 = Windows Server 2022) as `(status, version)`.
///
/// # Safety
/// No preconditions beyond a loaded IddCx class extension.
pub unsafe fn get_version() -> (NTSTATUS, u32) {
    let mut out = IDARG_OUT_GETVERSION { IddCxVersion: 0 };
    // SAFETY: `out` is a valid out-parameter of the right type.
    let status = call_iddcx!(
        PFN_IDDCXGETVERSION,
        _IDDFUNCENUM_IddCxGetVersionTableIndex,
        core::ptr::from_mut(&mut out),
    );
    (status, out.IddCxVersion)
}
