//! Virtual monitor lifecycle and the mode-related IddCx callbacks.
//!
//! The product default is a single EDID-less monitor (the OS falls back to
//! `EvtIddCxMonitorGetDefaultDescriptionModes` for its mode list); the
//! `connector_index` parameter and the handle-keyed state keep the design
//! open to several monitors later.

use wdk_sys::NTSTATUS;

use super::{adapter::size32, iddcx, iddcx::bindings, log_line, shared};
use crate::logic;

/// `STATUS_BUFFER_TOO_SMALL` (0xC0000023): buffer given but undersized.
const STATUS_BUFFER_TOO_SMALL: NTSTATUS = -1_073_741_789;

/// Fixed container ID for the virtual monitor. A stable ID (instead of a
/// random per-boot one) lets the OS recognize the monitor across reboots.
const CONTAINER_ID: bindings::GUID = bindings::GUID {
    Data1: 0x7564_7364, // "udsd"
    Data2: 0x6973,
    Data3: 0x7000,
    Data4: [0x55, 0x44, 0x53, 0, 0, 0, 0, 1], // "UDS" + counter
};

/// Creates the monitor object and reports it as connected. Called from the
/// adapter init-finished callback (like IddSampleDriver's `FinishInit`).
pub fn create_and_arrive(adapter: bindings::IDDCX_ADAPTER, connector_index: u32) {
    // IddSampleDriver passes non-null object attributes here (it attaches a
    // context); we keep monitor state globally instead, but still pass an
    // initialized attributes struct — the field is not documented optional.
    // SAFETY: POD struct; zeroed + Size is WDF_OBJECT_ATTRIBUTES_INIT.
    let mut attrs: bindings::_WDF_OBJECT_ATTRIBUTES = unsafe { core::mem::zeroed() };
    attrs.Size = size32::<bindings::_WDF_OBJECT_ATTRIBUTES>();

    let mut monitor_info = bindings::IDDCX_MONITOR_INFO {
        Size: size32::<bindings::IDDCX_MONITOR_INFO>(),
        MonitorType:
            bindings::DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY_DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI,
        ConnectorIndex: connector_index,
        MonitorDescription: bindings::IDDCX_MONITOR_DESCRIPTION {
            Size: size32::<bindings::IDDCX_MONITOR_DESCRIPTION>(),
            Type: bindings::IDDCX_MONITOR_DESCRIPTION_TYPE_IDDCX_MONITOR_DESCRIPTION_TYPE_EDID,
            // EDID-less: no description data, so the OS sources modes from
            // EvtIddCxMonitorGetDefaultDescriptionModes instead.
            DataSize: 0,
            pData: core::ptr::null_mut(),
        },
        MonitorContainerId: CONTAINER_ID,
    };

    let in_create = bindings::IDARG_IN_MONITORCREATE {
        ObjectAttributes: &mut attrs,
        pMonitorInfo: &mut monitor_info,
    };
    // SAFETY: all stack structures are fully initialized and outlive the call.
    let (status, out) = unsafe { iddcx::monitor_create(adapter, &in_create) };
    log_line(&format!("IddCxMonitorCreate connector={connector_index} -> {status:#010x}"));
    if status < 0 {
        return;
    }
    let monitor = out.MonitorObject;

    // SAFETY: `monitor` is a fresh handle from IddCxMonitorCreate.
    let (status, arrival) = unsafe { iddcx::monitor_arrival(monitor) };
    log_line(&format!(
        "IddCxMonitorArrival -> {status:#010x}, OsAdapterLuid={:#x}:{:#x}, OsTargetId={}",
        arrival.OsAdapterLuid.HighPart, arrival.OsAdapterLuid.LowPart, arrival.OsTargetId
    ));
    if status >= 0 {
        shared::lock().monitors.push(shared::MonitorEntry {
            handle: monitor as usize,
            connector_index,
            swapchain_task: None,
        });
    }
}

/// Fills a `DISPLAYCONFIG_VIDEO_SIGNAL_INFO` from a [`logic::Mode`],
/// mirroring IddSampleDriver's `FillSignalInfo`.
fn fill_signal_info(
    info: &mut bindings::DISPLAYCONFIG_VIDEO_SIGNAL_INFO,
    mode: logic::Mode,
    monitor_mode: bool,
) {
    info.pixelRate = mode.pixel_rate();
    info.hSyncFreq = bindings::DISPLAYCONFIG_RATIONAL {
        Numerator: mode.hsync_numerator(),
        Denominator: 1,
    };
    info.vSyncFreq = bindings::DISPLAYCONFIG_RATIONAL {
        Numerator: mode.vsync,
        Denominator: 1,
    };
    info.activeSize = bindings::DISPLAYCONFIG_2DREGION {
        cx: mode.width,
        cy: mode.height,
    };
    info.totalSize = bindings::DISPLAYCONFIG_2DREGION {
        cx: mode.width,
        cy: mode.height,
    };
    // Plain-data union; we write the whole 32-bit storage.
    info.__bindgen_anon_1.videoStandard = logic::additional_signal_info(monitor_mode);
    info.scanLineOrdering =
        bindings::DISPLAYCONFIG_SCANLINE_ORDERING_DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
}

/// Builds an `IDDCX_MONITOR_MODE` (driver-origin) from a [`logic::Mode`].
fn build_monitor_mode(mode: logic::Mode) -> bindings::IDDCX_MONITOR_MODE {
    // SAFETY: POD struct; every field is filled below.
    let mut out: bindings::IDDCX_MONITOR_MODE = unsafe { core::mem::zeroed() };
    out.Size = size32::<bindings::IDDCX_MONITOR_MODE>();
    out.Origin = bindings::IDDCX_MONITOR_MODE_ORIGIN_IDDCX_MONITOR_MODE_ORIGIN_DRIVER;
    fill_signal_info(&mut out.MonitorVideoSignalInfo, mode, true);
    out
}

/// Builds an `IDDCX_TARGET_MODE` from a [`logic::Mode`].
fn build_target_mode(mode: logic::Mode) -> bindings::IDDCX_TARGET_MODE {
    // SAFETY: POD struct; every field is filled below.
    let mut out: bindings::IDDCX_TARGET_MODE = unsafe { core::mem::zeroed() };
    out.Size = size32::<bindings::IDDCX_TARGET_MODE>();
    fill_signal_info(&mut out.TargetVideoSignalInfo.targetVideoSignalInfo, mode, false);
    out.RequiredBandwidth = 0;
    out
}

/// Two-phase monitor-mode serving shared by the parse and default-modes
/// callbacks: zero input count (or null buffer) asks for the required count;
/// an undersized buffer gets `STATUS_BUFFER_TOO_SMALL`; otherwise the modes
/// are copied out. Returns `(status, count_for_out_args)`.
///
/// # Safety
/// `buffer`, when non-null, must point to `input_count` writable
/// `IDDCX_MONITOR_MODE` entries.
unsafe fn serve_default_modes(input_count: u32, buffer: *mut bindings::IDDCX_MONITOR_MODE) -> (NTSTATUS, u32) {
    let modes = &logic::DEFAULT_MONITOR_MODES;
    let needed = modes.len() as u32;
    if input_count == 0 || buffer.is_null() {
        return (0, needed);
    }
    if input_count < needed {
        return (STATUS_BUFFER_TOO_SMALL, needed);
    }
    for (i, mode) in modes.iter().enumerate() {
        // SAFETY: guaranteed by the caller; i < needed <= input_count.
        unsafe { *buffer.add(i) = build_monitor_mode(*mode) };
    }
    (0, needed)
}

/// `EvtIddCxMonitorGetDefaultDescriptionModes`: mode list for our EDID-less
/// monitor (IddSampleDriver's `IddSampleMonitorGetDefaultModes`).
///
/// # Safety
/// Registered with IddCx; `input`/`output` are valid for the call duration.
pub unsafe extern "C" fn get_default_description_modes(
    _monitor: bindings::IDDCX_MONITOR,
    input: *const bindings::IDARG_IN_GETDEFAULTDESCRIPTIONMODES,
    output: *mut bindings::IDARG_OUT_GETDEFAULTDESCRIPTIONMODES,
) -> NTSTATUS {
    // SAFETY: guaranteed by IddCx for the duration of the callback.
    let (input, output) = unsafe { (&*input, &mut *output) };
    log_line(&format!(
        "EvtIddCxMonitorGetDefaultDescriptionModes input_count={}",
        input.DefaultMonitorModeBufferInputCount
    ));
    // SAFETY: the buffer contract is exactly what serve_default_modes documents.
    let (status, count) = unsafe {
        serve_default_modes(input.DefaultMonitorModeBufferInputCount, input.pDefaultMonitorModes)
    };
    output.DefaultMonitorModeBufferOutputCount = count;
    if status >= 0 && !input.pDefaultMonitorModes.is_null() {
        output.PreferredMonitorModeIdx = logic::PREFERRED_DEFAULT_MODE;
    }
    status
}

/// `EvtIddCxParseMonitorDescription`: never expected — we ship no EDID — but
/// if the OS calls it anyway, serve the default modes rather than failing
/// (a monitor with no modes is worse than a fib).
///
/// # Safety
/// Registered with IddCx; `input`/`output` are valid for the call duration.
pub unsafe extern "C" fn parse_description(
    input: *const bindings::IDARG_IN_PARSEMONITORDESCRIPTION,
    output: *mut bindings::IDARG_OUT_PARSEMONITORDESCRIPTION,
) -> NTSTATUS {
    // SAFETY: guaranteed by IddCx for the duration of the callback.
    let (input, output) = unsafe { (&*input, &mut *output) };
    log_line(&format!(
        "EvtIddCxParseMonitorDescription (EDID-less: unexpected) desc_size={} input_count={}",
        input.MonitorDescription.DataSize, input.MonitorModeBufferInputCount
    ));
    // SAFETY: the buffer contract is exactly what serve_default_modes documents.
    let (status, count) =
        unsafe { serve_default_modes(input.MonitorModeBufferInputCount, input.pMonitorModes) };
    output.MonitorModeBufferOutputCount = count;
    if status >= 0 && !input.pMonitorModes.is_null() {
        output.PreferredMonitorModeIdx = logic::PREFERRED_DEFAULT_MODE;
    }
    status
}

/// `EvtIddCxMonitorQueryTargetModes`: the modes the adapter can process; the
/// OS exposes their intersection with the monitor modes.
///
/// # Safety
/// Registered with IddCx; `input`/`output` are valid for the call duration.
pub unsafe extern "C" fn query_target_modes(
    _monitor: bindings::IDDCX_MONITOR,
    input: *const bindings::IDARG_IN_QUERYTARGETMODES,
    output: *mut bindings::IDARG_OUT_QUERYTARGETMODES,
) -> NTSTATUS {
    // SAFETY: guaranteed by IddCx for the duration of the callback.
    let (input, output) = unsafe { (&*input, &mut *output) };
    let modes = &logic::TARGET_MODES;
    let needed = modes.len() as u32;
    output.TargetModeBufferOutputCount = needed;
    if input.TargetModeBufferInputCount == 0 || input.pTargetModes.is_null() {
        return 0;
    }
    if input.TargetModeBufferInputCount < needed {
        return STATUS_BUFFER_TOO_SMALL;
    }
    for (i, mode) in modes.iter().enumerate() {
        // SAFETY: the OS guarantees TargetModeBufferInputCount writable entries.
        unsafe { *input.pTargetModes.add(i) = build_target_mode(*mode) };
    }
    0
}

/// `EvtIddCxAdapterCommitModes`: the OS picked a mode per path. Nothing to
/// reconfigure on a virtual adapter — IddCx reassigns the swap-chain itself —
/// but log the committed modes (IddSampleDriver also no-ops here).
///
/// # Safety
/// Registered with IddCx; `input` is valid for the call duration.
pub unsafe extern "C" fn commit_modes(
    _adapter: bindings::IDDCX_ADAPTER,
    input: *const bindings::IDARG_IN_COMMITMODES,
) -> NTSTATUS {
    // SAFETY: guaranteed by IddCx for the duration of the callback.
    let input = unsafe { &*input };
    log_line(&format!("EvtIddCxAdapterCommitModes paths={}", input.PathCount));
    for i in 0..input.PathCount as usize {
        // SAFETY: the OS guarantees PathCount readable entries.
        let path = unsafe { &*input.pPaths.add(i) };
        let active = path.Flags & bindings::IDDCX_PATH_FLAGS_IDDCX_PATH_FLAGS_ACTIVE != 0;
        let info = &path.TargetVideoSignalInfo;
        log_line(&format!(
            "  path[{i}] monitor={:?} active={active} {}x{}@{}Hz",
            path.MonitorObject, info.activeSize.cx, info.activeSize.cy, info.vSyncFreq.Numerator
        ));
    }
    0
}
