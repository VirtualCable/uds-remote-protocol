// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.

//! # udsscard
//!
//! UMDF 2 virtual smart card reader. The driver itself knows nothing about
//! the remote card: a userland counterpart (the uds-rp server service, or
//! the `udsscard-ctl` test tool) owns two named pipes
//! (`udsscard-proto`):
//!
//! * Command pipe (userland → driver): `InjectCard(atr)` / `EjectCard`.
//! * I/O pipe (driver → userland): each `IOCTL_SMARTCARD_TRANSMIT` is
//!   forwarded as a raw APDU and the synchronous reply is returned to the
//!   caller. Transmit handling is serialized with a mutex by design; the
//!   WDF queue is parallel, so other IOCTLs are not blocked by a slow card.
//!
//! Without a connected counterpart the reader reports "no card". When the
//! command pipe disconnects, the card is implicitly ejected.
//!
//! State is global (single reader). Multi-reader support would key this
//! state by device object and is deliberately left out for now.
//!
//! Reference: BixVReader (vsmartcard) — same IOCTL surface, but UMDF 1/COM.

use std::fs::{File, OpenOptions};
use std::sync::Mutex;
use std::time::Duration;

use udsscard_proto::{
    CMD_PIPE_NAME, Command, IO_PIPE_NAME, IO_RESP_ERROR, IO_RESP_OK, IO_TRANSMIT, read_frame,
    write_frame,
};
use wdk::println;
use wdk_sys::{
    call_unsafe_wdf_function_binding, GUID, NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT,
    STATUS_BUFFER_TOO_SMALL, STATUS_CANCELLED, STATUS_DEVICE_NOT_CONNECTED,
    STATUS_DEVICE_PROTOCOL_ERROR, STATUS_INVALID_PARAMETER, STATUS_NOT_SUPPORTED, STATUS_SUCCESS,
    ULONG, WDFDEVICE, WDFDEVICE_INIT, WDFDRIVER, WDFREQUEST, WDF_DRIVER_CONFIG,
    WDF_IO_QUEUE_CONFIG, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
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

// SCARD_ATTR_VALUE(class, tag) = (class << 16) | tag — see WDK winsmcrd.h.
const SCARD_ATTR_VENDOR_NAME: u32 = 0x0001_0100;
const SCARD_ATTR_VENDOR_IFD_TYPE: u32 = 0x0001_0101;
const SCARD_ATTR_VENDOR_IFD_VERSION: u32 = 0x0001_0102;
const SCARD_ATTR_VENDOR_IFD_SERIAL_NO: u32 = 0x0001_0103;
const SCARD_ATTR_CHANNEL_ID: u32 = 0x0002_0110;
const SCARD_ATTR_PROTOCOL_TYPES: u32 = 0x0003_0120;
const SCARD_ATTR_CHARACTERISTICS: u32 = 0x0006_0150;
const SCARD_ATTR_CURRENT_PROTOCOL_TYPE: u32 = 0x0008_0201;
const SCARD_ATTR_ICC_PRESENCE: u32 = 0x0009_0300;
const SCARD_ATTR_ICC_INTERFACE_STATUS: u32 = 0x0009_0301;
const SCARD_ATTR_CURRENT_IO_STATE: u32 = 0x0009_0302;
const SCARD_ATTR_ATR_STRING: u32 = 0x0009_0303;
const SCARD_ATTR_DEVICE_UNIT: u32 = 0x7FFF_0001;
const SCARD_ATTR_MAXINPUT: u32 = 0x0007_A007;

// ---------------------------------------------------------------------------
// Device state (global, single reader)
// ---------------------------------------------------------------------------

struct Card {
    atr: Vec<u8>,
    /// SCARD_SWALLOWED/NEGOTIABLE/SPECIFIC.
    io_state: u32,
    /// Active protocol after SET_PROTOCOL.
    protocol: u32,
    /// Powered (after cold/warm reset).
    powered: bool,
}

/// `None` = no card inserted.
static CARD: Mutex<Option<Card>> = Mutex::new(None);

struct SendRequest(WDFREQUEST);
// SAFETY: WDFREQUEST handles are usable from any thread; WDF serializes
// access through its own locking and we only store/complete it.
unsafe impl Send for SendRequest {}

/// Presence watches parked until a card is injected.
static PENDING_PRESENT: Mutex<Vec<SendRequest>> = Mutex::new(Vec::new());
/// Absence watches parked until the card is ejected.
static PENDING_ABSENT: Mutex<Vec<SendRequest>> = Mutex::new(Vec::new());

/// Connected I/O pipe to the userland counterpart (`None` when nobody is
/// attached — reader behaves as an empty slot).
static IO_PIPE: Mutex<Option<File>> = Mutex::new(None);

// ---------------------------------------------------------------------------
// Diagnostics: file log (WUDFHost debug output needs a viewer; a file in
// C:\Windows\Temp is writable by the LocalService host process).
// ---------------------------------------------------------------------------

fn log_line(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(r"C:\Windows\Temp\udsscard.log")
    {
        let _ = writeln!(f, "{msg}");
    }
    println!("{msg}");
}

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
    log_line("udsscard: DriverEntry");

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
        let st = call_unsafe_wdf_function_binding!(
            WdfDriverCreate,
            driver,
            registry_path,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut driver_config,
            WDF_NO_HANDLE.cast::<WDFDRIVER>(),
        );
        log_line(&format!("udsscard: WdfDriverCreate -> {st:#010x}"));
        st
    }
}

// ---------------------------------------------------------------------------
// Device add
// ---------------------------------------------------------------------------

extern "C" fn evt_driver_device_add(
    _driver: WDFDRIVER,
    mut device_init: *mut WDFDEVICE_INIT,
) -> NTSTATUS {
    log_line("udsscard: EvtDriverDeviceAdd");

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
        log_line(&format!("udsscard: WdfDeviceCreate failed {ntstatus:#010x}"));
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
        log_line(&format!("udsscard: CreateDeviceInterface failed {ntstatus:#010x}"));
        return ntstatus;
    }

    let mut queue_config = WDF_IO_QUEUE_CONFIG {
        Size: size_of::<WDF_IO_QUEUE_CONFIG>() as ULONG,
        DefaultQueue: 1,
        // Virtual device: no power management, so requests must be
        // dispatched regardless of any framework power state.
        PowerManaged: wdk_sys::_WDF_TRI_STATE::WdfFalse,
        DispatchType: wdk_sys::_WDF_IO_QUEUE_DISPATCH_TYPE::WdfIoQueueDispatchParallel,
        EvtIoDeviceControl: Some(evt_io_device_control),
        EvtIoDefault: Some(evt_io_default),
        ..WDF_IO_QUEUE_CONFIG::default()
    };
    // wdk-sys's `default()` zeroes the struct, but WDF_IO_QUEUE_CONFIG_INIT
    // sets NumberOfPresentedRequests to -1 (unlimited) for parallel queues.
    // With 0 the framework presents no request at all and every IOCTL pends
    // forever (observed: host recycles on idle timeout after ~3 minutes).
    queue_config.Settings.Parallel.NumberOfPresentedRequests = u32::MAX; // -1 = unlimited

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
        log_line(&format!("udsscard: WdfIoQueueCreate failed {ntstatus:#010x}"));
        return ntstatus;
    }

    // IPC thread: owns the command pipe connection lifecycle. Runs for the
    // lifetime of the host process (no join on unload — the host is torn
    // down as a whole).
    std::thread::spawn(ipc_thread_main);

    log_line("udsscard: device ready");
    STATUS_SUCCESS
}

// ---------------------------------------------------------------------------
// IPC thread (command pipe consumer)
// ---------------------------------------------------------------------------

/// Open a named pipe as a client, retrying while the server side does not
/// exist yet (`ERROR_FILE_NOT_FOUND`, 2).
fn open_pipe(name: &str) -> File {
    loop {
        match OpenOptions::new().read(true).write(true).open(name) {
            Ok(f) => {
                log_line(&format!("udsscard: connected to {name}"));
                return f;
            }
            Err(e) => {
                log_line(&format!("udsscard: waiting for {name}: {e}"));
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

fn ipc_thread_main() {
    loop {
        let mut cmd = open_pipe(CMD_PIPE_NAME);
        // The counterpart creates both pipes before accepting; connect the
        // I/O pipe right after the command pipe.
        let io = open_pipe(IO_PIPE_NAME);
        *IO_PIPE.lock().unwrap() = Some(io);

        // Command loop: inject/eject until the counterpart goes away.
        loop {
            match read_frame(&mut cmd) {
                Ok((tag, payload)) => match Command::parse(tag, payload) {
                    Some(Command::InjectCard(atr)) => inject_card(atr),
                    Some(Command::EjectCard) => eject_card(),
                    None => log_line(&format!("udsscard: unknown cmd frame tag={tag}")),
                },
                Err(e) => {
                    log_line(&format!("udsscard: cmd pipe lost: {e}"));
                    break;
                }
            }
        }

        // Counterpart disconnected: implicit eject, back to reconnect.
        *IO_PIPE.lock().unwrap() = None;
        eject_card();
    }
}

/// Insert a card: store its ATR and complete parked presence watches.
fn inject_card(atr: Vec<u8>) {
    log_line(&format!("udsscard: card injected ({} bytes ATR)", atr.len()));
    {
        let mut card = CARD.lock().unwrap();
        *card = Some(Card {
            atr,
            io_state: SCARD_SWALLOWED,
            protocol: SCARD_PROTOCOL_T1,
            powered: false,
        });
    }
    let pending: Vec<SendRequest> = PENDING_PRESENT.lock().unwrap().drain(..).collect();
    for req in pending {
        unmark_and_complete(req.0, STATUS_SUCCESS, 0);
    }
}

/// Remove the card: drop state and complete parked absence watches.
fn eject_card() {
    log_line("udsscard: card ejected");
    *CARD.lock().unwrap() = None;
    let pending: Vec<SendRequest> = PENDING_ABSENT.lock().unwrap().drain(..).collect();
    for req in pending {
        unmark_and_complete(req.0, STATUS_SUCCESS, 0);
    }
}

/// Complete a parked (cancelable) request, unless the cancel callback
/// already owns it.
fn unmark_and_complete(request: WDFREQUEST, status: NTSTATUS, information: usize) {
    // SAFETY: `request` was marked cancelable when parked; unmarking races
    // with the cancel callback, and STATUS_CANCELLED tells us the callback
    // won and will complete the request itself.
    let st = unsafe { call_unsafe_wdf_function_binding!(WdfRequestUnmarkCancelable, request,) };
    if st != STATUS_CANCELLED {
        complete(request, status, information);
    }
}

// ---------------------------------------------------------------------------
// Request helpers
// ---------------------------------------------------------------------------

/// Input buffer of a request as a byte vector (empty when shorter than
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
        call_unsafe_wdf_function_binding!(
            WdfRequestCompleteWithInformation,
            request,
            status,
            information as u64,
        );
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

/// Park a presence/absence watch: mark cancelable and store.
fn park_watch(request: WDFREQUEST, pending: &'static Mutex<Vec<SendRequest>>) {
    // SAFETY: `request` is live; the cancel callback is valid. The wdk-sys
    // binding models WdfRequestMarkCancelable as returning `()`; the cancel
    // callback covers the race where the request was already being
    // cancelled.
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestMarkCancelable,
            request,
            Some(evt_request_cancel),
        );
    }
    pending.lock().unwrap().push(SendRequest(request));
}

// ---------------------------------------------------------------------------
// IOCTL dispatch
// ---------------------------------------------------------------------------

extern "C" fn evt_io_default(_queue: wdk_sys::WDFQUEUE, request: WDFREQUEST) {
    log_line("udsscard: EvtIoDefault (unexpected request)");
    complete(request, STATUS_NOT_SUPPORTED, 0);
}

extern "C" fn evt_io_device_control(
    _queue: wdk_sys::WDFQUEUE,
    request: WDFREQUEST,
    _output_length: usize,
    _input_length: usize,
    io_control_code: ULONG,
) {
    log_line(&format!("udsscard: IOCTL {io_control_code:#010x}"));
    match io_control_code {
        IOCTL_SMARTCARD_IS_PRESENT => {
            if CARD.lock().unwrap().is_some() {
                complete(request, STATUS_SUCCESS, 0);
            } else {
                park_watch(request, &PENDING_PRESENT);
            }
        }
        IOCTL_SMARTCARD_IS_ABSENT => {
            if CARD.lock().unwrap().is_none() {
                complete(request, STATUS_SUCCESS, 0);
            } else {
                park_watch(request, &PENDING_ABSENT);
            }
        }
        IOCTL_SMARTCARD_GET_STATE => {
            let state = match CARD.lock().unwrap().as_ref() {
                Some(card) => card.io_state,
                None => SCARD_ABSENT,
            };
            write_bytes(request, &state.to_le_bytes());
        }
        IOCTL_SMARTCARD_POWER => {
            let Some(action) = read_u32(request) else {
                complete(request, STATUS_BUFFER_TOO_SMALL, 0);
                return;
            };
            let mut guard = CARD.lock().unwrap();
            let Some(card) = guard.as_mut() else {
                complete(request, STATUS_DEVICE_NOT_CONNECTED, 0);
                return;
            };
            match action {
                SCARD_COLD_RESET | SCARD_WARM_RESET => {
                    card.io_state = SCARD_NEGOTIABLE;
                    card.powered = true;
                    let atr = card.atr.clone();
                    drop(guard);
                    write_bytes(request, &atr);
                }
                SCARD_POWER_DOWN => {
                    card.io_state = SCARD_SWALLOWED;
                    card.powered = false;
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
            let mut guard = CARD.lock().unwrap();
            let Some(card) = guard.as_mut() else {
                complete(request, STATUS_DEVICE_NOT_CONNECTED, 0);
                return;
            };
            card.protocol = chosen;
            card.io_state = SCARD_SPECIFIC;
            drop(guard);
            write_bytes(request, &chosen.to_le_bytes());
        }
        IOCTL_SMARTCARD_GET_ATTRIBUTE => {
            let Some(attr) = read_u32(request) else {
                complete(request, STATUS_BUFFER_TOO_SMALL, 0);
                return;
            };
            get_attribute(request, attr);
        }
        IOCTL_SMARTCARD_TRANSMIT => transmit(request),
        _ => complete(request, STATUS_NOT_SUPPORTED, 0),
    }
}

/// Forward the APDU to the userland counterpart and return its reply.
///
/// Input layout (WDK): a `SCARD_IO_REQUEST` (protocol u32, cbPciLength u32)
/// followed by the raw APDU; `cbPciLength` is the size of that header
/// itself (8 for T=0/T=1), so the APDU starts at `input[cbPciLength..]`.
/// Only the raw APDU crosses the pipe; the reply gets a fresh header.
fn transmit(request: WDFREQUEST) {
    let Some(input) = request_input(request, 8) else {
        complete(request, STATUS_BUFFER_TOO_SMALL, 0);
        return;
    };
    let send_protocol = u32::from_le_bytes(input[0..4].try_into().expect("4 bytes"));
    let cb_pci = u32::from_le_bytes(input[4..8].try_into().expect("4 bytes")) as usize;
    if cb_pci < 8 || input.len() < cb_pci {
        complete(request, STATUS_INVALID_PARAMETER, 0);
        return;
    }
    let apdu = input[cb_pci..].to_vec();
    log_line(&format!(
        "udsscard: transmit apdu {}",
        apdu.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));

    if CARD.lock().unwrap().as_ref().is_none_or(|c| !c.powered) {
        complete(request, STATUS_DEVICE_NOT_CONNECTED, 0);
        return;
    }

    // Serialized by design (iteration 1): the queue stays parallel, so
    // presence watches and attributes are not blocked by a slow card.
    let mut guard = IO_PIPE.lock().unwrap();
    let Some(pipe) = guard.as_mut() else {
        complete(request, STATUS_DEVICE_NOT_CONNECTED, 0);
        return;
    };
    let reply = write_frame(pipe, IO_TRANSMIT, &apdu)
        .and_then(|()| read_frame(pipe));
    match reply {
        Ok((IO_RESP_OK, payload)) => {
            log_line(&format!(
                "udsscard: transmit resp {}",
                payload.iter().map(|b| format!("{b:02x}")).collect::<String>()
            ));
            let mut response = Vec::with_capacity(8 + payload.len());
            response.extend_from_slice(&send_protocol.to_le_bytes());
            response.extend_from_slice(&8u32.to_le_bytes()); // cbPciLength
            response.extend_from_slice(&payload);
            drop(guard);
            write_bytes(request, &response);
        }
        Ok((IO_RESP_ERROR, payload)) => {
            let code = if payload.len() >= 4 {
                u32::from_le_bytes(payload[0..4].try_into().expect("4 bytes"))
            } else {
                0
            };
            log_line(&format!("udsscard: transmit failed at relay, scard error {code:#x}"));
            drop(guard);
            complete(request, STATUS_DEVICE_PROTOCOL_ERROR, 0);
        }
        Ok((tag, _)) => {
            log_line(&format!("udsscard: unexpected io reply tag={tag}"));
            drop(guard);
            complete(request, STATUS_DEVICE_PROTOCOL_ERROR, 0);
        }
        Err(e) => {
            // The pipe is broken; drop it so the next transmit fails fast
            // instead of blocking on a dead counterpart.
            log_line(&format!("udsscard: io pipe error: {e}"));
            *guard = None;
            drop(guard);
            complete(request, STATUS_DEVICE_PROTOCOL_ERROR, 0);
        }
    }
}

fn get_attribute(request: WDFREQUEST, attr: u32) {
    log_line(&format!("udsscard: GET_ATTRIBUTE {attr:#010x}"));
    let guard = CARD.lock().unwrap();
    match attr {
        SCARD_ATTR_VENDOR_NAME => write_bytes(request, b"VirtualCable\0"),
        SCARD_ATTR_VENDOR_IFD_TYPE => write_bytes(request, b"UDS Virtual Reader\0"),
        SCARD_ATTR_VENDOR_IFD_VERSION => write_bytes(request, &0x0001_0000u32.to_le_bytes()),
        SCARD_ATTR_VENDOR_IFD_SERIAL_NO => write_bytes(request, b"00000001\0"),
        SCARD_ATTR_CHANNEL_ID | SCARD_ATTR_DEVICE_UNIT => {
            write_bytes(request, &0u32.to_le_bytes());
        }
        SCARD_ATTR_PROTOCOL_TYPES => {
            write_bytes(request, &(SCARD_PROTOCOL_T0 | SCARD_PROTOCOL_T1).to_le_bytes());
        }
        SCARD_ATTR_CHARACTERISTICS => write_bytes(request, &0u32.to_le_bytes()),
        SCARD_ATTR_CURRENT_PROTOCOL_TYPE => {
            let protocol = guard.as_ref().map_or(0, |c| c.protocol);
            write_bytes(request, &protocol.to_le_bytes());
        }
        SCARD_ATTR_ICC_PRESENCE => {
            write_bytes(request, &u8::from(guard.is_some()).to_le_bytes());
        }
        SCARD_ATTR_ICC_INTERFACE_STATUS => {
            let active = guard.as_ref().is_some_and(|c| c.powered);
            write_bytes(request, &u8::from(active).to_le_bytes());
        }
        SCARD_ATTR_CURRENT_IO_STATE => {
            let state = guard.as_ref().map_or(SCARD_ABSENT, |c| c.io_state);
            write_bytes(request, &state.to_le_bytes());
        }
        SCARD_ATTR_ATR_STRING => match guard.as_ref() {
            Some(card) => write_bytes(request, &card.atr.clone()),
            None => complete(request, STATUS_DEVICE_NOT_CONNECTED, 0),
        },
        SCARD_ATTR_MAXINPUT => write_bytes(request, &255u32.to_le_bytes()),
        _ => complete(request, STATUS_NOT_SUPPORTED, 0),
    }
}

extern "C" fn evt_request_cancel(request: WDFREQUEST) {
    log_line("udsscard: request cancelled");
    for pending in [&PENDING_PRESENT, &PENDING_ABSENT] {
        let mut guard = pending.lock().unwrap();
        if let Some(pos) = guard.iter().position(|r| r.0 == request) {
            guard.swap_remove(pos);
            break;
        }
    }
    // SAFETY: the request is being cancelled by the framework; completing
    // it with STATUS_CANCELLED is the required response.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfRequestComplete, request, STATUS_CANCELLED,);
    }
}
