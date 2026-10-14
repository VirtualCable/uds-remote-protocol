//! Control channel: `EvtIddCxDeviceIoControl` receives the IOCTLs sent by
//! `udsdisp-ctl` (list / create / remove / set-mode) and dispatches them to
//! the monitor lifecycle functions. The wire format is defined (and unit-
//! tested on Linux) in [`crate::logic`]; this module is only the plumbing
//! between WDF requests and those functions.

use wdk_sys::{NTSTATUS, WDFREQUEST, call_unsafe_wdf_function_binding};

use super::{iddcx::bindings, log_line, monitor, shared};
use crate::logic;

/// `STATUS_INVALID_PARAMETER` (0xC000000D): malformed body or out-of-range
/// arguments (bad connector, unsupported mode).
const STATUS_INVALID_PARAMETER: NTSTATUS = -1_073_741_811;

/// `STATUS_INVALID_DEVICE_REQUEST` (0xC0000010): unknown IOCTL code.
const STATUS_INVALID_DEVICE_REQUEST: NTSTATUS = -1_073_741_808;

/// `STATUS_BUFFER_TOO_SMALL` (0xC0000023): request buffers undersized.
const STATUS_BUFFER_TOO_SMALL: NTSTATUS = -1_073_741_789;

/// `STATUS_OBJECT_NAME_COLLISION` (0xC0000035): connector already in use.
const STATUS_OBJECT_NAME_COLLISION: NTSTATUS = -1_073_741_771;

/// `STATUS_DEVICE_NOT_READY` (0xC00000A3): the adapter is not up yet.
const STATUS_DEVICE_NOT_READY: NTSTATUS = -1_073_741_661;

/// `EvtIddCxDeviceIoControl`: IddCx forwards `DeviceIoControl` calls made on
/// our device interface to this callback (its internal queue); the request
/// must be completed exactly once.
///
/// # Safety
/// Registered with IddCx in `IDD_CX_CLIENT_CONFIG`; IddCx calls it with a
/// live request and valid buffer lengths.
pub unsafe extern "C" fn device_io_control(
    _device: bindings::WDFDEVICE,
    request: bindings::WDFREQUEST,
    _output_buffer_length: usize,
    _input_buffer_length: usize,
    io_control_code: u32,
) {
    let request = request as WDFREQUEST;
    log_line(&format!("EvtIddCxDeviceIoControl code={io_control_code:#010x}"));
    let (status, information) = match io_control_code {
        logic::IOCTL_LIST => handle_list(request),
        logic::IOCTL_CREATE => handle_create(request),
        logic::IOCTL_REMOVE => handle_remove(request),
        logic::IOCTL_SET_MODE => handle_set_mode(request),
        other => {
            log_line(&format!("unknown IOCTL {other:#010x}"));
            (STATUS_INVALID_DEVICE_REQUEST, 0)
        }
    };
    // SAFETY: `request` is live until completed, which happens exactly once,
    // here, after the handler produced its output.
    unsafe { complete_request(request, status, information) };
}

/// Retrieves the request's input buffer. The returned pointer is valid until
/// the request is completed.
///
/// # Safety
/// `request` must be a live, uncompleted WDF request.
unsafe fn input_buffer(request: WDFREQUEST, min_len: usize) -> Result<*const u8, NTSTATUS> {
    let mut buffer: *mut core::ffi::c_void = core::ptr::null_mut();
    // SAFETY: `request` is live; `buffer` is a valid out-pointer and the
    // length out-pointer may be null.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveInputBuffer,
            request,
            min_len,
            &mut buffer,
            core::ptr::null_mut::<usize>(),
        )
    };
    if status < 0 {
        log_line(&format!("WdfRequestRetrieveInputBuffer -> {status:#010x}"));
        return Err(status);
    }
    Ok(buffer.cast::<u8>().cast_const())
}

/// Retrieves the request's output buffer. The returned pointer is valid for
/// `min_len` writable bytes until the request is completed.
///
/// # Safety
/// `request` must be a live, uncompleted WDF request.
unsafe fn output_buffer(request: WDFREQUEST, min_len: usize) -> Result<*mut u8, NTSTATUS> {
    let mut buffer: *mut core::ffi::c_void = core::ptr::null_mut();
    // SAFETY: same contract as input_buffer.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveOutputBuffer,
            request,
            min_len,
            &mut buffer,
            core::ptr::null_mut::<usize>(),
        )
    };
    if status < 0 {
        log_line(&format!("WdfRequestRetrieveOutputBuffer -> {status:#010x}"));
        return Err(status);
    }
    Ok(buffer.cast::<u8>())
}

/// Completes the request with a status and an output byte count.
///
/// # Safety
/// `request` must be live and completed nowhere else.
unsafe fn complete_request(request: WDFREQUEST, status: NTSTATUS, information: usize) {
    // WDF ULONG_PTR is u64 in the wdk-sys bindings.
    let information = u64::try_from(information).unwrap_or(u64::MAX);
    // SAFETY: guaranteed by the caller.
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestCompleteWithInformation,
            request,
            status,
            information,
        );
    }
}

/// Reads and decodes a fixed-size request body.
fn read_body<T>(
    request: WDFREQUEST,
    len: usize,
    decode: impl FnOnce(&[u8]) -> Option<T>,
) -> Result<T, NTSTATUS> {
    // SAFETY: `request` is live until the caller completes it.
    let ptr = unsafe { input_buffer(request, len) }.map_err(|_| STATUS_BUFFER_TOO_SMALL)?;
    // SAFETY: retrieval succeeded, so `ptr` is valid for `len` bytes.
    let body = unsafe { core::slice::from_raw_parts(ptr, len) };
    decode(body).ok_or(STATUS_INVALID_PARAMETER)
}

/// `IOCTL_LIST`: report every connector slot (connected or not).
fn handle_list(request: WDFREQUEST) -> (NTSTATUS, usize) {
    // SAFETY: `request` is live until completed by the caller.
    let ptr = match unsafe { output_buffer(request, logic::LIST_RESPONSE_LEN) } {
        Ok(ptr) => ptr,
        Err(status) => return (status, 0),
    };
    let statuses: Vec<logic::MonitorStatus> = {
        let state = shared::lock();
        (0..logic::MAX_MONITORS)
            .map(
                |connector_index| match state.monitors.iter().find(|m| m.connector_index == connector_index) {
                    Some(entry) => logic::MonitorStatus {
                        connector_index,
                        connected: true,
                        mode: entry
                            .current_mode
                            .or_else(|| entry.modes.first().copied())
                            .unwrap_or(logic::Mode::new(0, 0, 0)),
                    },
                    None => logic::MonitorStatus {
                        connector_index,
                        connected: false,
                        mode: logic::Mode::new(0, 0, 0),
                    },
                },
            )
            .collect()
    };
    let bytes = logic::encode_list(&statuses);
    // SAFETY: `ptr` is valid for LIST_RESPONSE_LEN writable bytes.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len()) };
    (0, logic::LIST_RESPONSE_LEN)
}

/// `IOCTL_CREATE`: hot-plug a monitor on a free connector.
fn handle_create(request: WDFREQUEST) -> (NTSTATUS, usize) {
    let (connector_index, mode) =
        match read_body(request, logic::MODE_REQUEST_LEN, logic::decode_mode_request) {
            Ok(ok) => ok,
            Err(status) => return (status, 0),
        };
    if connector_index >= logic::MAX_MONITORS || !mode.is_supported() {
        log_line(&format!(
            "IOCTL create rejected: connector={connector_index} mode={mode:?} (out of range or unsupported)"
        ));
        return (STATUS_INVALID_PARAMETER, 0);
    }
    let adapter = {
        let state = shared::lock();
        if state.monitors.iter().any(|m| m.connector_index == connector_index) {
            log_line(&format!("IOCTL create rejected: connector {connector_index} already in use"));
            return (STATUS_OBJECT_NAME_COLLISION, 0);
        }
        state.adapter
    };
    let Some(adapter) = adapter else {
        log_line("IOCTL create rejected: adapter not initialized yet");
        return (STATUS_DEVICE_NOT_READY, 0);
    };
    let status = monitor::create_and_arrive(
        adapter as bindings::IDDCX_ADAPTER,
        connector_index,
        logic::modes_with_preferred(mode),
    );
    (status, 0)
}

/// `IOCTL_REMOVE`: unplug and destroy the monitor on a connector.
fn handle_remove(request: WDFREQUEST) -> (NTSTATUS, usize) {
    let connector_index =
        match read_body(request, logic::CONNECTOR_REQUEST_LEN, logic::decode_connector) {
            Ok(ok) => ok,
            Err(status) => return (status, 0),
        };
    (monitor::depart(connector_index), 0)
}

/// `IOCTL_SET_MODE`: change the preferred mode of a live monitor.
fn handle_set_mode(request: WDFREQUEST) -> (NTSTATUS, usize) {
    let (connector_index, mode) =
        match read_body(request, logic::MODE_REQUEST_LEN, logic::decode_mode_request) {
            Ok(ok) => ok,
            Err(status) => return (status, 0),
        };
    if !mode.is_supported() {
        log_line(&format!("IOCTL set-mode rejected: mode {mode:?} not in target modes"));
        return (STATUS_INVALID_PARAMETER, 0);
    }
    (monitor::set_monitor_mode(connector_index, mode), 0)
}
