//! Adapter initialization: `EvtDeviceD0Entry` builds the adapter capabilities
//! and starts the asynchronous IddCx adapter init; the init-finished callback
//! starts the service-liveness watchdog. **No monitor is created here**: the
//! adapter always comes up with zero monitors and the orchestrator service
//! hot-plugs one per active session through the control channel — a monitor
//! that outlives its service becomes a ghost display that swallows the
//! console input (see the study, sections 18.21-18.22).

use wdk_sys::{NTSTATUS, WDFDEVICE, WDF_POWER_DEVICE_STATE};

use super::{iddcx, iddcx::bindings, log_line, shared, watchdog};
use crate::logic;

/// `size_of::<T>()` as `u32` for IddCx `Size` fields; every structure involved
/// is far below 4 GiB.
#[allow(clippy::cast_possible_truncation)]
pub(super) const fn size32<T>() -> u32 {
    size_of::<T>() as u32
}

/// `EvtDeviceD0Entry`: the device reached the fully-on state, so create the
/// indirect display adapter. Registered via
/// `WdfDeviceInitSetPnpPowerEventCallbacks` in `EvtDriverDeviceAdd`.
///
/// # Safety
/// Registered with WDF as `PFN_WDF_DEVICE_D0_ENTRY`; WDF calls it with a valid
/// device handle once per D0 entry.
pub unsafe extern "C" fn evt_device_d0_entry(
    device: WDFDEVICE,
    _previous_state: WDF_POWER_DEVICE_STATE,
) -> NTSTATUS {
    log_line("EvtDeviceD0Entry");
    if shared::lock().adapter.is_some() {
        log_line("adapter already initialized, skipping");
        return 0;
    }

    let mut endpoint_version = bindings::IDDCX_ENDPOINT_VERSION {
        Size: size32::<bindings::IDDCX_ENDPOINT_VERSION>(),
        MajorVer: 1,
        MinorVer: 0,
        Build: 0,
        SKU: 0,
    };

    // Mirrors IddSampleDriver's InitAdapter: basic feature support plus
    // endpoint diagnostics (telemetry-only, but the name/version fields are
    // required by the OS).
    let mut caps = bindings::IDDCX_ADAPTER_CAPS {
        Size: size32::<bindings::IDDCX_ADAPTER_CAPS>(),
        Flags: bindings::IDDCX_ADAPTER_FLAGS_IDDCX_ADAPTER_FLAGS_NONE,
        MaxDisplayPipelineRate: 0, // driver-defined units; 0 = no accounting
        MaxMonitorsSupported: logic::MAX_MONITORS,
        EndPointDiagnostics: bindings::IDDCX_ENDPOINT_DIAGNOSTIC_INFO {
            Size: size32::<bindings::IDDCX_ENDPOINT_DIAGNOSTIC_INFO>(),
            TransmissionType:
                bindings::IDDCX_TRANSMISSION_TYPE_IDDCX_TRANSMISSION_TYPE_WIRED_OTHER,
            pEndPointFriendlyName: windows::core::w!("UDS Virtual Display").as_ptr(),
            pEndPointModelName: windows::core::w!("UDS Virtual Monitor").as_ptr(),
            pEndPointManufacturerName: windows::core::w!("Virtual Cable").as_ptr(),
            pHardwareVersion: &mut endpoint_version,
            pFirmwareVersion: &mut endpoint_version,
            GammaSupport: bindings::IDDCX_FEATURE_IMPLEMENTATION_IDDCX_FEATURE_IMPLEMENTATION_NONE,
        },
        StaticDesktopReencodeFrameCount: 0,
    };

    let in_init = bindings::IDARG_IN_ADAPTER_INIT {
        WdfDevice: device.cast(),
        pCaps: &mut caps,
        ObjectAttributes: core::ptr::null_mut(),
    };

    // SAFETY: `caps` and `in_init` are fully initialized stack structures and
    // outlive the call; IddCx copies what it needs synchronously.
    let (status, out) = unsafe { iddcx::adapter_init_async(&in_init) };
    log_line(&format!("IddCxAdapterInitAsync -> {status:#010x}"));
    if status >= 0 {
        shared::lock().adapter = Some(out.AdapterObject as usize);
    }
    status
}

/// `EvtIddCxAdapterInitFinished`: the adapter is ready. No monitor is plugged
/// here — the service creates one per session via the control channel. This
/// callback only starts the watchdog that detaches every monitor if the
/// service dies.
///
/// # Safety
/// Registered with IddCx in `IDD_CX_CLIENT_CONFIG`; called with valid args.
pub unsafe extern "C" fn on_init_finished(
    _adapter: bindings::IDDCX_ADAPTER,
    in_args: *const bindings::IDARG_IN_ADAPTER_INIT_FINISHED,
) -> NTSTATUS {
    // SAFETY: guaranteed by IddCx for the duration of the callback.
    let init_status = unsafe { (*in_args).AdapterInitStatus };
    log_line(&format!(
        "EvtIddCxAdapterInitFinished status={init_status:#010x} (no default monitor; watchdog starting)"
    ));
    if init_status >= 0 {
        watchdog::spawn();
    }
    0
}
