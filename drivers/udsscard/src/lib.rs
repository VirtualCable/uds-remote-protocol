// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.

//! # udsscard (spike)
//!
//! UMDF 2 virtual smart card reader. Goal of the spike: prove that an UMDF 2
//! driver can expose the SmartCardReader device interface and satisfy the
//! minimal IOCTL set SCardSvr exercises, with one always-present fake card
//! (fixed ATR, `90 00` to every APDU). If `SCardListReaders` /
//! `certutil -scinfo` see it, the real reader (remote card over the uds-rp
//! channel, doc/plan section 17) is a port of this skeleton.
//!
//! Reference: BixVReader (vsmartcard) — same IOCTL surface, but UMDF 1/COM.
//!
//! This is a spike: state is global (single device), and the pending
//! `IS_ABSENT` request never completes because the fake card is never
//! removed. The real driver will track per-reader state and complete
//! pending presence watches on remote card insertion/removal events.

use std::sync::Mutex;

use wdk::println;
use wdk_sys::{
    call_unsafe_wdf_function_binding, GUID, NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT,
    STATUS_BUFFER_TOO_SMALL, STATUS_CANCELLED, STATUS_INVALID_DEVICE_REQUEST,
    STATUS_INVALID_PARAMETER, STATUS_NOT_SUPPORTED, STATUS_SUCCESS, ULONG, WDFDEVICE,
    WDFDEVICE_INIT, WDFDRIVER, WDFREQUEST, WDF_DRIVER_CONFIG, WDF_IO_QUEUE_CONFIG,
    WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
};

// ---------------------------------------------------------------------------
// Constants (WDK winsmcrd.h)
// ---------------------------------------------------------------------------

/// `GUID_DEVINTERFACE_SMARTCARD_READER`.
const SMARTCARD_READER_GUID: GUID = GUID {
    Data1: 0x50DD_5230,
    Data2: 0xBA8A,
    Data3: 0x11D1,
    Data4: [0xBF, 0x5D, 0x00, 0x00, 0xF8, 0x05, 0xF5, 0x30],
};

const FILE_DEVICE_SMARTCARD: u32 = 0x31;
const fn scard_ctl(code: u32) -> u32 {
    // CTL_CODE(FILE_DEVICE_SMARTCARD, code, METHOD_BUFFERED, FILE_ANY_ACCESS)
    (FILE_DEVICE_SMARTCARD << 16) | (code << 2)
}

const IOCTL_SMARTCARD_POWER: u32 = scard_ctl(1);
const IOCTL_SMARTCARD_GET_ATTRIBUTE: u32 = scard_ctl(2);
const IOCTL_SMARTCARD_SET_ATTRIBUTE: u32 = scard_ctl(3);
const IOCTL_SMARTCARD_TRANSMIT: u32 = scard_ctl(5);
const IOCTL_SMARTCARD_IS_PRESENT: u32 = scard_ctl(10);
const IOCTL_SMARTCARD_IS_ABSENT: u32 = scard_ctl(11);
const IOCTL_SMARTCARD_SET_PROTOCOL: u32 = scard_ctl(12);
const IOCTL_SMARTCARD_GET_STATE: u32 = scard_ctl(14);

const SCARD_POWER_DOWN: u32 = 0;
const SCARD_COLD_RESET: u32 = 1;
const SCARD_WARM_RESET: u32 = 2;

const SCARD_ABSENT: u32 = 1;
const SCARD_SWALLOWED: u32 = 3;
const SCARD_NEGOTIABLE: u32 = 5;
const SCARD_SPECIFIC: u32 = 6;

const SCARD_PROTOCOL_T0: u32 = 0x1;
const SCARD_PROTOCOL_T1: u32 = 0x2;

const SCARD_ATTR_VENDOR_NAME: u32 = 0x0003_0100;
const SCARD_ATTR_VENDOR_IFD_TYPE: u32 = 0x0003_0101;
const SCARD_ATTR_VENDOR_IFD_VERSION: u32 = 0x0003_0102;
const SCARD_ATTR_CHANNEL_ID: u32 = 0x0002_0110;
const SCARD_ATTR_CHARACTERISTICS: u32 = 0x0006_0150;
const SCARD_ATTR_CURRENT_PROTOCOL_TYPE: u32 = 0x0008_0201;
const SCARD_ATTR_ATR_STRING: u32 = 0x0009_0303;
const SCARD_ATTR_DEVICE_UNIT: u32 = 0x7FFF_0001;

/// Fake card ATR ("eUDS-Card", TCK verified in freerdp-integration).
const FAKE_ATR: [u8; 13] = [
    0x3B, 0x89, 0x01, 0x45, 0x55, 0x44, 0x53, 0x2D, 0x43, 0x61, 0x72, 0x64, 0x96,
];

// ---------------------------------------------------------------------------
// Device state (spike: global, single reader, card always present)
// ---------------------------------------------------------------------------

struct DeviceState {
    /// SCARD_ABSENT/SWALLOWED/NEGOTIABLE/SPECIFIC.
    state: u32,
    /// Active protocol after SET_PROTOCOL.
    protocol: u32,
}

static STATE: Mutex<DeviceState> = Mutex::new(DeviceState {
    state: SCARD_NEGOTIABLE,
    protocol: SCARD_PROTOCOL_T1,
});

struct SendRequest(WDFREQUEST);
// SAFETY: WDFREQUEST handles are usable from any thread; WDF serializes
// access through its own locking and we only store/complete it.
unsafe impl Send for SendRequest {}

/// Pending IS_ABSENT watch (completes when the card is removed — never, in
/// this spike).
static PENDING_ABSENT: Mutex<Option<SendRequest>> = Mutex::new(None);

// ---------------------------------------------------------------------------
// DriverEntry
// ---------------------------------------------------------------------------

/// # Safety
/// `DriverEntry` is invoked by the WDF loader with valid arguments.
#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(
    driver: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    println!("udsscard: DriverEntry");

    let mut driver_config = WDF_DRIVER_CONFIG {
        Size: size_of::<WDF_DRIVER_CONFIG>() as ULONG,
        EvtDriverDeviceAdd: Some(evt_driver_device_add),
        EvtDriverUnload: None,
        ..WDF_DRIVER_CONFIG::default()
    };

    // SAFETY: `driver`/`registry_path` come from the loader and are valid;
    // `driver_config` is a valid, initialized struct; the output handle is
    // allowed to be null (WDF_NO_HANDLE).
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverCreate,
            driver,
            registry_path,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut driver_config,
            WDF_NO_HANDLE.cast::<WDFDRIVER>(),
        )
    }
}

// ---------------------------------------------------------------------------
// Device add
// ---------------------------------------------------------------------------

extern "C" fn evt_driver_device_add(
    _driver: WDFDRIVER,
    mut device_init: *mut WDFDEVICE_INIT,
) -> NTSTATUS {
    println!("udsscard: EvtDriverDeviceAdd");

    // Note: WdfDeviceInitSetDeviceType is not in the wdk-sys UMDF binding
    // table; the SmartCardReader device interface below is what SCardSvr
    // actually keys on.

    let mut device: WDFDEVICE = WDF_NO_HANDLE.cast();
    // SAFETY: `device_init` is provided by the framework and valid;
    // attributes may be null; `device` receives the created handle.
    let ntstatus = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            &mut device_init,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut device,
        )
    };
    if ntstatus != STATUS_SUCCESS {
        println!("udsscard: WdfDeviceCreate failed {ntstatus:#010x}");
        return ntstatus;
    }

    // Expose the SmartCardReader device interface: this is what makes
    // SCardSvr enumerate us as a reader.
    // SAFETY: `device` is a live handle; the GUID pointer is valid; the
    // reference string is optional and null.
    let ntstatus = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreateDeviceInterface,
            device,
            &SMARTCARD_READER_GUID,
            core::ptr::null::<wdk_sys::UNICODE_STRING>(),
        )
    };
    if ntstatus != STATUS_SUCCESS {
        println!("udsscard: CreateDeviceInterface failed {ntstatus:#010x}");
        return ntstatus;
    }

    let mut queue_config = WDF_IO_QUEUE_CONFIG {
        Size: size_of::<WDF_IO_QUEUE_CONFIG>() as ULONG,
        DefaultQueue: 1,
        DispatchType: wdk_sys::_WDF_IO_QUEUE_DISPATCH_TYPE::WdfIoQueueDispatchParallel,
        EvtIoDeviceControl: Some(evt_io_device_control),
        ..WDF_IO_QUEUE_CONFIG::default()
    };

    // SAFETY: `device` is a live handle; `queue_config` is valid; the queue
    // handle output may be null.
    let ntstatus = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoQueueCreate,
            device,
            &mut queue_config,
            WDF_NO_OBJECT_ATTRIBUTES,
            WDF_NO_HANDLE.cast::<wdk_sys::WDFQUEUE>(),
        )
    };
    if ntstatus != STATUS_SUCCESS {
        println!("udsscard: WdfIoQueueCreate failed {ntstatus:#010x}");
        return ntstatus;
    }

    println!("udsscard: device ready");
    STATUS_SUCCESS
}

// ---------------------------------------------------------------------------
// Request helpers
// ---------------------------------------------------------------------------

/// Input buffer of a request as a byte slice (empty when shorter than
/// `min_len` — caller then fails the request).
fn request_input(request: WDFREQUEST, min_len: usize) -> Option<Vec<u8>> {
    let mut buffer: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut length: usize = 0;
    // SAFETY: `request` is a live request owned by this callback; the output
    // pointers are valid.
    let ntstatus = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveInputBuffer,
            request,
            min_len,
            &mut buffer,
            &mut length,
        )
    };
    if ntstatus != STATUS_SUCCESS || buffer.is_null() || length < min_len {
        return None;
    }
    // SAFETY: the framework guarantees `length` readable bytes at `buffer`
    // for the lifetime of the request.
    Some(unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), length) }.to_vec())
}

/// Output buffer of a request as a mutable byte slice.
fn request_output<'a>(request: WDFREQUEST, min_len: usize) -> Option<&'a mut [u8]> {
    let mut buffer: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut length: usize = 0;
    // SAFETY: `request` is a live request owned by this callback; the output
    // pointers are valid.
    let ntstatus = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveOutputBuffer,
            request,
            min_len,
            &mut buffer,
            &mut length,
        )
    };
    if ntstatus != STATUS_SUCCESS || buffer.is_null() || length < min_len {
        return None;
    }
    // SAFETY: the framework guarantees `length` writable bytes at `buffer`
    // for the lifetime of the request; the request is only completed once.
    Some(unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), length) })
}

fn complete(request: WDFREQUEST, status: NTSTATUS, information: usize) {
    // SAFETY: `request` is owned by this callback and completed exactly once.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfRequestCompleteWithInformation, request, status, information as u64,);
    }
}

fn read_u32(request: WDFREQUEST) -> Option<u32> {
    let input = request_input(request, 4)?;
    Some(u32::from_le_bytes(input[0..4].try_into().expect("4 bytes")))
}

fn write_bytes(request: WDFREQUEST, data: &[u8]) {
    match request_output(request, data.len()) {
        Some(out) => {
            out[..data.len()].copy_from_slice(data);
            complete(request, STATUS_SUCCESS, data.len());
        }
        None => complete(request, STATUS_BUFFER_TOO_SMALL, 0),
    }
}

// ---------------------------------------------------------------------------
// IOCTL dispatch
// ---------------------------------------------------------------------------

extern "C" fn evt_io_device_control(
    _queue: wdk_sys::WDFQUEUE,
    request: WDFREQUEST,
    _output_length: usize,
    _input_length: usize,
    io_control_code: ULONG,
) {
    println!("udsscard: IOCTL {io_control_code:#010x}");
    match io_control_code {
        IOCTL_SMARTCARD_IS_PRESENT => {
            // Spike: the fake card is always present.
            complete(request, STATUS_SUCCESS, 0);
        }
        IOCTL_SMARTCARD_IS_ABSENT => {
            // Park the watch; SCardSvr keeps it pending and its completion
            // is the removal event (which never happens for the fake card).
            // The wdk-sys binding models WdfRequestMarkCancelable as
            // returning `()`; the cancel callback covers the race where the
            // request was already being cancelled.
            // SAFETY: `request` is live; the cancel callback is valid.
            unsafe {
                call_unsafe_wdf_function_binding!(
                    WdfRequestMarkCancelable,
                    request,
                    Some(evt_request_cancel),
                );
            }
            *PENDING_ABSENT.lock().unwrap() = Some(SendRequest(request));
        }
        IOCTL_SMARTCARD_GET_STATE => {
            let state = STATE.lock().unwrap().state;
            write_bytes(request, &state.to_le_bytes());
        }
        IOCTL_SMARTCARD_POWER => {
            let Some(action) = read_u32(request) else {
                complete(request, STATUS_BUFFER_TOO_SMALL, 0);
                return;
            };
            match action {
                SCARD_COLD_RESET | SCARD_WARM_RESET => {
                    STATE.lock().unwrap().state = SCARD_NEGOTIABLE;
                    write_bytes(request, &FAKE_ATR);
                }
                SCARD_POWER_DOWN => {
                    STATE.lock().unwrap().state = SCARD_SWALLOWED;
                    complete(request, STATUS_SUCCESS, 0);
                }
                _ => complete(request, STATUS_INVALID_PARAMETER, 0),
            }
        }
        IOCTL_SMARTCARD_SET_PROTOCOL => {
            let Some(mask) = read_u32(request) else {
                complete(request, STATUS_BUFFER_TOO_SMALL, 0);
                return;
            };
            let chosen = if mask & SCARD_PROTOCOL_T1 != 0 {
                SCARD_PROTOCOL_T1
            } else if mask & SCARD_PROTOCOL_T0 != 0 {
                SCARD_PROTOCOL_T0
            } else {
                complete(request, STATUS_INVALID_PARAMETER, 0);
                return;
            };
            let mut state = STATE.lock().unwrap();
            state.protocol = chosen;
            state.state = SCARD_SPECIFIC;
            drop(state);
            write_bytes(request, &chosen.to_le_bytes());
        }
        IOCTL_SMARTCARD_GET_ATTRIBUTE => {
            let Some(attr) = read_u32(request) else {
                complete(request, STATUS_BUFFER_TOO_SMALL, 0);
                return;
            };
            get_attribute(request, attr);
        }
        IOCTL_SMARTCARD_TRANSMIT => {
            let Some(input) = request_input(request, 8) else {
                complete(request, STATUS_BUFFER_TOO_SMALL, 0);
                return;
            };
            let send_protocol = u32::from_le_bytes(input[0..4].try_into().expect("4 bytes"));
            // Fake card: every APDU succeeds with `90 00`.
            let mut response = Vec::with_capacity(10);
            response.extend_from_slice(&send_protocol.to_le_bytes());
            response.extend_from_slice(&8u32.to_le_bytes()); // cbPciLength
            response.extend_from_slice(&[0x90, 0x00]);
            write_bytes(request, &response);
        }
        _ => complete(request, STATUS_NOT_SUPPORTED, 0),
    }
}

fn get_attribute(request: WDFREQUEST, attr: u32) {
    let state = STATE.lock().unwrap();
    match attr {
        SCARD_ATTR_VENDOR_NAME => write_bytes(request, b"VirtualCable\0"),
        SCARD_ATTR_VENDOR_IFD_TYPE => write_bytes(request, b"UDS Virtual Reader\0"),
        SCARD_ATTR_VENDOR_IFD_VERSION => write_bytes(request, &0x0001_0000u32.to_le_bytes()),
        SCARD_ATTR_CHANNEL_ID | SCARD_ATTR_DEVICE_UNIT => {
            write_bytes(request, &0u32.to_le_bytes());
        }
        SCARD_ATTR_CHARACTERISTICS => write_bytes(request, &0u32.to_le_bytes()),
        SCARD_ATTR_CURRENT_PROTOCOL_TYPE => {
            write_bytes(request, &state.protocol.to_le_bytes());
        }
        SCARD_ATTR_ATR_STRING => write_bytes(request, &FAKE_ATR),
        _ => complete(request, STATUS_NOT_SUPPORTED, 0),
    }
}

extern "C" fn evt_request_cancel(request: WDFREQUEST) {
    println!("udsscard: request cancelled");
    let mut pending = PENDING_ABSENT.lock().unwrap();
    if let Some(stored) = pending.as_ref()
        && stored.0 == request
    {
        *pending = None;
    }
    drop(pending);
    // SAFETY: the request is being cancelled by the framework; completing
    // it with STATUS_CANCELLED is the required response.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfRequestComplete, request, STATUS_CANCELLED,);
    }
}

// Keep the compiler from pruning constants used only by the real driver later.
#[allow(dead_code)]
const _: () = {
    assert!(IOCTL_SMARTCARD_SET_ATTRIBUTE != 0);
    assert!(IOCTL_SMARTCARD_TRANSMIT != 0);
    assert!(SCARD_ABSENT == 1);
    assert!(STATUS_INVALID_DEVICE_REQUEST != 0);
};
