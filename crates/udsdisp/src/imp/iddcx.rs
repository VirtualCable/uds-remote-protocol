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

/// Version negotiation global expected by `iddcxstub.lib`: in C this is the
/// `__declspec(selectany) ULONG IddMinimumVersionRequired =
/// IDDCX_MINIMUM_VERSION_REQUIRED` definition that `IddCxFuncEnum.h` injects
/// into the client (the stub itself is built with `IDD_STUB`, so it only
/// references the symbol). We build against the 1.7 headers and require
/// IddCx >= 1.7 (Windows Server 2022 / Windows 11 21H2+).
#[unsafe(no_mangle)]
#[allow(non_upper_case_globals)]
static IddMinimumVersionRequired: u32 = 7;

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
    IDARG_IN_ADAPTER_INIT, IDARG_IN_GETDIRTYRECTS, IDARG_IN_MONITORCREATE, IDARG_IN_SWAPCHAINSETDEVICE,
    IDARG_OUT_ADAPTER_INIT, IDARG_OUT_GETDIRTYRECTS, IDARG_OUT_GETVERSION, IDARG_OUT_MONITORARRIVAL,
    IDARG_OUT_MONITORCREATE, IDARG_OUT_RELEASEANDACQUIREBUFFER, IDDCX_ADAPTER, IDDCX_MONITOR,
    IDDCX_SWAPCHAIN, IDD_CX_CLIENT_CONFIG, PFN_IDDCXADAPTERINITASYNC, PFN_IDDCXDEVICEINITCONFIG,
    PFN_IDDCXDEVICEINITIALIZE, PFN_IDDCXGETVERSION, PFN_IDDCXMONITORARRIVAL, PFN_IDDCXMONITORCREATE,
    PFN_IDDCXSWAPCHAINFINISHEDPROCESSINGFRAME, PFN_IDDCXSWAPCHAINGETDIRTYRECTS,
    PFN_IDDCXSWAPCHAINRELEASEANDACQUIREBUFFER, PFN_IDDCXSWAPCHAINSETDEVICE, RECT,
    _IDDFUNCENUM_IddCxAdapterInitAsyncTableIndex, _IDDFUNCENUM_IddCxDeviceInitConfigTableIndex,
    _IDDFUNCENUM_IddCxDeviceInitializeTableIndex, _IDDFUNCENUM_IddCxGetVersionTableIndex,
    _IDDFUNCENUM_IddCxMonitorArrivalTableIndex, _IDDFUNCENUM_IddCxMonitorCreateTableIndex,
    _IDDFUNCENUM_IddCxSwapChainFinishedProcessingFrameTableIndex,
    _IDDFUNCENUM_IddCxSwapChainGetDirtyRectsTableIndex,
    _IDDFUNCENUM_IddCxSwapChainReleaseAndAcquireBufferTableIndex,
    _IDDFUNCENUM_IddCxSwapChainSetDeviceTableIndex,
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

/// `IddCxAdapterInitAsync`: starts the asynchronous initialization of the
/// WDDM adapter object for `in_args.WdfDevice`. Completion is reported through
/// the `EvtIddCxAdapterInitFinished` callback. Returns `(status, out-args)`,
/// where `out-args.AdapterObject` is the adapter handle.
///
/// # Safety
/// `in_args` must be fully initialized (caps with correct `Size` fields); the
/// WDFDEVICE inside must be the device created in `EvtDriverDeviceAdd`.
pub unsafe fn adapter_init_async(
    in_args: &IDARG_IN_ADAPTER_INIT,
) -> (NTSTATUS, IDARG_OUT_ADAPTER_INIT) {
    // SAFETY: POD out-struct.
    let mut out: IDARG_OUT_ADAPTER_INIT = unsafe { core::mem::zeroed() };
    // SAFETY: guaranteed by the caller.
    let status = call_iddcx!(
        PFN_IDDCXADAPTERINITASYNC,
        _IDDFUNCENUM_IddCxAdapterInitAsyncTableIndex,
        core::ptr::from_ref(in_args),
        core::ptr::from_mut(&mut out),
    );
    (status, out)
}

/// `IddCxMonitorCreate`: creates a monitor object on `adapter`. The monitor
/// is not visible to the OS until [`monitor_arrival`] is called.
///
/// # Safety
/// `adapter` must be a live adapter handle and `in_args.pMonitorInfo` must
/// point to a fully initialized `IDDCX_MONITOR_INFO`.
pub unsafe fn monitor_create(
    adapter: IDDCX_ADAPTER,
    in_args: &IDARG_IN_MONITORCREATE,
) -> (NTSTATUS, IDARG_OUT_MONITORCREATE) {
    // SAFETY: POD out-struct.
    let mut out: IDARG_OUT_MONITORCREATE = unsafe { core::mem::zeroed() };
    // SAFETY: guaranteed by the caller.
    let status = call_iddcx!(
        PFN_IDDCXMONITORCREATE,
        _IDDFUNCENUM_IddCxMonitorCreateTableIndex,
        adapter,
        core::ptr::from_ref(in_args),
        core::ptr::from_mut(&mut out),
    );
    (status, out)
}

/// `IddCxMonitorArrival`: reports the monitor as plugged in. Returns
/// `(status, out-args)` with the OS-assigned adapter LUID and target id.
///
/// # Safety
/// `monitor` must be a live monitor handle from [`monitor_create`].
pub unsafe fn monitor_arrival(monitor: IDDCX_MONITOR) -> (NTSTATUS, IDARG_OUT_MONITORARRIVAL) {
    // SAFETY: POD out-struct.
    let mut out: IDARG_OUT_MONITORARRIVAL = unsafe { core::mem::zeroed() };
    // SAFETY: guaranteed by the caller.
    let status = call_iddcx!(
        PFN_IDDCXMONITORARRIVAL,
        _IDDFUNCENUM_IddCxMonitorArrivalTableIndex,
        monitor,
        core::ptr::from_mut(&mut out),
    );
    (status, out)
}

/// `IddCxSwapChainSetDevice`: hands the DXGI device that will process frames
/// to the swap-chain. Returns an HRESULT (not NTSTATUS).
///
/// # Safety
/// `swapchain` must be the handle received in `EvtIddCxMonitorAssignSwapChain`
/// and `in_args.pDevice` a valid `IDXGIDevice` pointer.
pub unsafe fn swapchain_set_device(swapchain: IDDCX_SWAPCHAIN, in_args: &IDARG_IN_SWAPCHAINSETDEVICE) -> i32 {
    // SAFETY: guaranteed by the caller.
    call_iddcx!(
        PFN_IDDCXSWAPCHAINSETDEVICE,
        _IDDFUNCENUM_IddCxSwapChainSetDeviceTableIndex,
        swapchain,
        core::ptr::from_ref(in_args),
    )
}

/// `IddCxSwapChainReleaseAndAcquireBuffer`: releases the current frame and
/// acquires the next one. Returns `(HRESULT, out-args)`; `E_PENDING` means no
/// new frame is ready (wait on the swap-chain event). On success the caller
/// owns a reference on `out.MetaData.pSurface` that it must release.
///
/// # Safety
/// `swapchain` must be a live swap-chain handle with a device set via
/// [`swapchain_set_device`].
pub unsafe fn swapchain_release_and_acquire(
    swapchain: IDDCX_SWAPCHAIN,
) -> (i32, IDARG_OUT_RELEASEANDACQUIREBUFFER) {
    // SAFETY: POD out-struct.
    let mut out: IDARG_OUT_RELEASEANDACQUIREBUFFER = unsafe { core::mem::zeroed() };
    // SAFETY: guaranteed by the caller.
    let hr = call_iddcx!(
        PFN_IDDCXSWAPCHAINRELEASEANDACQUIREBUFFER,
        _IDDFUNCENUM_IddCxSwapChainReleaseAndAcquireBufferTableIndex,
        swapchain,
        core::ptr::from_mut(&mut out),
    );
    (hr, out)
}

/// `IddCxSwapChainGetDirtyRects`: copies the dirty rects of the currently
/// acquired frame into `rects`. Returns `(HRESULT, count actually copied)`.
///
/// # Safety
/// `swapchain` must be in a frame-acquired state (a successful
/// [`swapchain_release_and_acquire`] without an intervening one).
pub unsafe fn swapchain_get_dirty_rects(swapchain: IDDCX_SWAPCHAIN, rects: &mut [RECT]) -> (i32, u32) {
    let in_args = IDARG_IN_GETDIRTYRECTS {
        DirtyRectInCount: {
            assert!(rects.len() <= u32::MAX as usize);
            #[allow(clippy::cast_possible_truncation)]
            {
                rects.len() as u32
            }
        },
        pDirtyRects: rects.as_mut_ptr(),
    };
    // SAFETY: POD out-struct.
    let mut out: IDARG_OUT_GETDIRTYRECTS = unsafe { core::mem::zeroed() };
    // SAFETY: `rects` is a valid writable buffer of DirtyRectInCount entries.
    let hr = call_iddcx!(
        PFN_IDDCXSWAPCHAINGETDIRTYRECTS,
        _IDDFUNCENUM_IddCxSwapChainGetDirtyRectsTableIndex,
        swapchain,
        core::ptr::from_ref(&in_args),
        core::ptr::from_mut(&mut out),
    );
    (hr, out.DirtyRectOutCount)
}

/// `IddCxSwapChainFinishedProcessingFrame`: tells the OS the driver is done
/// with the current frame. Must be called after every successful acquire
/// before acquiring again. Returns an HRESULT.
///
/// # Safety
/// `swapchain` must be in a frame-acquired state.
pub unsafe fn swapchain_finished_processing_frame(swapchain: IDDCX_SWAPCHAIN) -> i32 {
    // SAFETY: guaranteed by the caller.
    call_iddcx!(
        PFN_IDDCXSWAPCHAINFINISHEDPROCESSINGFRAME,
        _IDDFUNCENUM_IddCxSwapChainFinishedProcessingFrameTableIndex,
        swapchain,
    )
}
