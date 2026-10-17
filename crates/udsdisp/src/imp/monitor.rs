//! Virtual monitor lifecycle and the mode-related IddCx callbacks.
//!
//! The product default is a single EDID-less monitor (the OS falls back to
//! `EvtIddCxMonitorGetDefaultDescriptionModes` for its mode list); monitors
//! are keyed by connector index so the control channel (`IOCTL_CREATE` /
//! `IOCTL_REMOVE` / `IOCTL_SET_MODE`) can hot-plug and re-mode them.

use wdk_sys::{NTSTATUS, WDFOBJECT, call_unsafe_wdf_function_binding};

use super::{adapter::size32, cursor, iddcx, iddcx::bindings, log_line, shared, swapchain};
use crate::logic;

/// `STATUS_BUFFER_TOO_SMALL` (0xC0000023): buffer given but undersized.
const STATUS_BUFFER_TOO_SMALL: NTSTATUS = -1_073_741_789;

/// `STATUS_NOT_FOUND` (0xC0000225): no monitor on the requested connector.
const STATUS_NOT_FOUND: NTSTATUS = -1_073_741_275;

/// The mode the EDID advertises: the preferred one (index 0), or 1920x1080@60
/// when the mode list is empty.
fn mode_for_edid(modes: &[logic::Mode]) -> logic::Mode {
    modes.first().copied().unwrap_or(logic::Mode::new(1920, 1080, 60))
}

/// Fixed container ID for the virtual monitor. A stable ID (instead of a
/// random per-boot one) lets the OS recognize the monitor across reboots.
const CONTAINER_ID: bindings::GUID = bindings::GUID {
    Data1: 0x7564_7364, // "udsd"
    Data2: 0x6973,
    Data3: 0x7000,
    Data4: [0x55, 0x44, 0x53, 0, 0, 0, 0, 1], // "UDS" + counter
};

/// Creates a monitor object on `connector_index` serving `modes` (index 0 =
/// preferred) and reports it as connected. Returns the NTSTATUS of the last
/// IddCx call; on failure all partial state is rolled back. Called from the
/// adapter init-finished callback (like IddSampleDriver's `FinishInit`) and
/// from the control channel (`IOCTL_CREATE`).
pub fn create_and_arrive(
    adapter: bindings::IDDCX_ADAPTER,
    connector_index: u32,
    modes: Vec<logic::Mode>,
) -> NTSTATUS {
    // WDF_OBJECT_ATTRIBUTES_INIT is not a plain zero-init: it also sets
    // ExecutionLevel and SynchronizationScope to InheritFromParent — a zeroed
    // struct has both set to *Invalid and WDF rejects it with
    // STATUS_WDF_OBJECT_ATTRIBUTES_INVALID (0xC0200209).
    // SAFETY: POD struct; the macro equivalent is zeroed + Size + the two
    // inherit-from-parent fields.
    let mut attrs: bindings::_WDF_OBJECT_ATTRIBUTES = unsafe { core::mem::zeroed() };
    attrs.Size = size32::<bindings::_WDF_OBJECT_ATTRIBUTES>();
    attrs.ExecutionLevel = bindings::_WDF_EXECUTION_LEVEL_WdfExecutionLevelInheritFromParent;
    attrs.SynchronizationScope =
        bindings::_WDF_SYNCHRONIZATION_SCOPE_WdfSynchronizationScopeInheritFromParent;

    // The EDID carries the preferred mode (first of the list) as its
    // detailed timing. It lives on the stack and only needs to outlive
    // `IddCxMonitorArrival` below, which copies it.
    let edid = udsdisp_proto::edid::edid_for_mode(mode_for_edid(&modes));
    let mut monitor_info = bindings::IDDCX_MONITOR_INFO {
        Size: size32::<bindings::IDDCX_MONITOR_INFO>(),
        MonitorType:
            bindings::DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY_DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI,
        ConnectorIndex: connector_index,
        MonitorDescription: bindings::IDDCX_MONITOR_DESCRIPTION {
            Size: size32::<bindings::IDDCX_MONITOR_DESCRIPTION>(),
            Type: bindings::IDDCX_MONITOR_DESCRIPTION_TYPE_IDDCX_MONITOR_DESCRIPTION_TYPE_EDID,
            DataSize: edid.len() as u32,
            pData: edid.as_ptr().cast_mut().cast(),
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
        return status;
    }
    let monitor = out.MonitorObject;

    // Register the entry before arrival: the OS queries the monitor modes
    // (EvtIddCxMonitorGetDefaultDescriptionModes) from inside
    // IddCxMonitorArrival, and the callback looks the list up by handle.
    shared::lock().monitors.push(shared::MonitorEntry {
        handle: monitor as usize,
        connector_index,
        modes,
        current_mode: None,
        swapchain_task: None,
        cursor: None,
        cursor_task: None,
    });

    // SAFETY: `monitor` is a fresh handle from IddCxMonitorCreate.
    let (status, arrival) = unsafe { iddcx::monitor_arrival(monitor) };
    log_line(&format!(
        "IddCxMonitorArrival -> {status:#010x}, OsAdapterLuid={:#x}:{:#x}, OsTargetId={}",
        arrival.OsAdapterLuid.HighPart, arrival.OsAdapterLuid.LowPart, arrival.OsTargetId
    ));
    if status < 0 {
        // Roll back: forget the entry and release the monitor object.
        shared::lock().monitors.retain(|m| m.handle != monitor as usize);
        // SAFETY: `monitor` is ours and never arrived.
        unsafe { delete_object(monitor as usize) };
    }
    status
}

/// Unplugs the monitor on `connector_index` (`IddCxMonitorDeparture`).
/// Returns `STATUS_NOT_FOUND` when no monitor sits on that connector.
///
/// The monitor object is NOT deleted afterwards: `IddCxMonitorDeparture`
/// itself destroys the `IDDCX_MONITOR` object (documented in "IddCx
/// Objects"), so an extra `WdfObjectDelete` double-frees it and takes the
/// whole WUDFHost process down (observed in M4).
pub fn depart(connector_index: u32) -> NTSTATUS {
    let handle = {
        let state = shared::lock();
        state
            .monitors
            .iter()
            .find(|m| m.connector_index == connector_index)
            .map(|m| m.handle)
    };
    let Some(handle) = handle else {
        log_line(&format!("IOCTL remove: no monitor on connector {connector_index}"));
        return STATUS_NOT_FOUND;
    };

    // Stop the frame loop first; the OS also unassigns the swap-chain during
    // departure, and stop_task is idempotent (the task is an Option::take).
    swapchain::stop_task(handle);
    // Stop cursor tracking before departure destroys the monitor object (the
    // tracking thread queries through that handle).
    cursor::stop(handle);

    // SAFETY: `handle` is a live monitor object from the state map.
    let status = unsafe { iddcx::monitor_departure(handle as bindings::IDDCX_MONITOR) };
    log_line(&format!(
        "IddCxMonitorDeparture connector={connector_index} -> {status:#010x}"
    ));
    if status >= 0 {
        shared::lock().monitors.retain(|m| m.handle != handle);
    }
    status
}

/// Changes the preferred mode of the monitor on `connector_index`: rewrites
/// the stored mode list (requested mode first, so it is the preferred index
/// 0 if the OS re-queries the default modes) and pushes the new target-mode
/// list through `IddCxMonitorUpdateModes`, which forces the OS to re-evaluate
/// modes on the monitor. Returns `STATUS_NOT_FOUND` when the connector is
/// empty.
pub fn set_monitor_mode(connector_index: u32, mode: logic::Mode) -> NTSTATUS {
    let handle = {
        let mut state = shared::lock();
        match state
            .monitors
            .iter_mut()
            .find(|m| m.connector_index == connector_index)
        {
            Some(entry) => {
                entry.modes = logic::modes_with_preferred(mode);
                entry.handle
            }
            None => {
                log_line(&format!("IOCTL set-mode: no monitor on connector {connector_index}"));
                return STATUS_NOT_FOUND;
            }
        }
    };

    // IDARG_IN_UPDATEMODES carries TARGET modes; the requested mode is in
    // TARGET_MODES by validation at the control-channel boundary, so the
    // monitor∩target intersection the OS exposes always contains it.
    let targets: Vec<bindings::IDDCX_TARGET_MODE> =
        logic::TARGET_MODES.iter().map(|m| build_target_mode(*m)).collect();
    let in_args = bindings::IDARG_IN_UPDATEMODES {
        Reason: bindings::IDDCX_UPDATE_REASON_IDDCX_UPDATE_REASON_OTHER,
        TargetModeCount: {
            assert!(targets.len() <= u32::MAX as usize);
            #[allow(clippy::cast_possible_truncation)]
            {
                targets.len() as u32
            }
        },
        pTargetModes: targets.as_ptr().cast_mut(),
    };
    // SAFETY: `handle` is a live monitor object; `targets` outlives the call.
    let status = unsafe { iddcx::monitor_update_modes(handle as bindings::IDDCX_MONITOR, &in_args) };
    log_line(&format!(
        "IddCxMonitorUpdateModes connector={connector_index} {}x{}@{} -> {status:#010x}",
        mode.width, mode.height, mode.vsync
    ));
    status
}

/// `WdfObjectDelete` on a raw handle (monitors and swap-chains are WDF
/// objects).
///
/// # Safety
/// `object` must be a valid, owned WDF object handle.
unsafe fn delete_object(object: usize) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfObjectDelete, object as WDFOBJECT);
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
unsafe fn serve_modes(
    modes: &[logic::Mode],
    input_count: u32,
    buffer: *mut bindings::IDDCX_MONITOR_MODE,
) -> (NTSTATUS, u32) {
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

/// The mode list stored for `monitor` (its preferred mode first), falling
/// back to the static defaults when the handle is unknown (the OS may query
/// modes for a monitor whose arrival failed midway).
fn modes_for(monitor: bindings::IDDCX_MONITOR) -> Vec<logic::Mode> {
    let state = shared::lock();
    state
        .monitors
        .iter()
        .find(|m| m.handle == monitor as usize)
        .map_or_else(|| logic::DEFAULT_MONITOR_MODES.to_vec(), |e| e.modes.clone())
}

/// `EvtIddCxMonitorGetDefaultDescriptionModes`: mode list for our EDID-less
/// monitor (IddSampleDriver's `IddSampleMonitorGetDefaultModes`). Serves the
/// per-monitor list stored at creation/`set-mode` time.
///
/// # Safety
/// Registered with IddCx; `input`/`output` are valid for the call duration.
pub unsafe extern "C" fn get_default_description_modes(
    monitor: bindings::IDDCX_MONITOR,
    input: *const bindings::IDARG_IN_GETDEFAULTDESCRIPTIONMODES,
    output: *mut bindings::IDARG_OUT_GETDEFAULTDESCRIPTIONMODES,
) -> NTSTATUS {
    // SAFETY: guaranteed by IddCx for the duration of the callback.
    let (input, output) = unsafe { (&*input, &mut *output) };
    let modes = modes_for(monitor);
    log_line(&format!(
        "EvtIddCxMonitorGetDefaultDescriptionModes monitor={monitor:?} input_count={} modes={}",
        input.DefaultMonitorModeBufferInputCount,
        modes.len()
    ));
    // SAFETY: the buffer contract is exactly what serve_modes documents.
    let (status, count) = unsafe {
        serve_modes(
            &modes,
            input.DefaultMonitorModeBufferInputCount,
            input.pDefaultMonitorModes,
        )
    };
    output.DefaultMonitorModeBufferOutputCount = count;
    if status >= 0 && !input.pDefaultMonitorModes.is_null() {
        // Index 0 is the preferred mode in every list we serve
        // (modes_with_preferred and DEFAULT_MONITOR_MODES both order so).
        output.PreferredMonitorModeIdx = 0;
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
    // SAFETY: the buffer contract is exactly what serve_modes documents.
    let (status, count) = unsafe {
        serve_modes(
            &logic::DEFAULT_MONITOR_MODES,
            input.MonitorModeBufferInputCount,
            input.pMonitorModes,
        )
    };
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
/// but record the committed mode per monitor (the control channel reports it
/// in `IOCTL_LIST`) and log the paths (IddSampleDriver also no-ops here).
/// Every active path also (re-)enables hardware cursor: a mode commit resets
/// the path to software cursor, so the setup must be re-armed on every commit.
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
        if !active {
            continue;
        }
        {
            let mut state = shared::lock();
            if let Some(entry) =
                state.monitors.iter_mut().find(|m| m.handle == path.MonitorObject as usize)
            {
                entry.current_mode = Some(logic::Mode::new(
                    info.activeSize.cx,
                    info.activeSize.cy,
                    info.vSyncFreq.Numerator,
                ));
            }
        }
        // Lock released: cursor::enable locks the state internally.
        cursor::enable(path.MonitorObject);
    }
    0
}
