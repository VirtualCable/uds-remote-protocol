//! Control device and IOCTL channel.
//!
//! The agent in the remote session pushes microphone PCM through
//! `\\.\UDSAudioCtl` (`\Device\UDSAudioCtl` + `\DosDevices\UDSAudioCtl`
//! symlink) using a single METHOD_BUFFERED IOCTL,
//! [`logic::IOCTL_PUSH_MIC`]. The bytes land in the mic ring; the capture
//! stream pulls them out from its `GetPosition` (see `stream.rs`).
//!
//! The device is created once per driver load in `AddDevice` (the adapter
//! FDO is a named portcls device — user mode cannot `CreateFile` it) and
//! removed in `DriverUnload`. `PcInitializeAdapterDriver` points every
//! `MajorFunction` at `PcDispatchIrp`, so we install our own dispatchers
//! for CREATE/CLOSE/DEVICE_CONTROL *after* it; each of them forwards any
//! IRP not aimed at the control device back to PortCls untouched.
//!
//! All IOCTL handling runs at PASSIVE_LEVEL in the caller's thread context.

use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

use wdk_sys::ntddk::{
    IoCreateDevice, IoCreateSymbolicLink, IoDeleteDevice, IoDeleteSymbolicLink,
    IofCompleteRequest, RtlInitUnicodeString,
};
use wdk_sys::{
    DEVICE_OBJECT, DO_BUFFERED_IO, DO_DEVICE_INITIALIZING, FILE_DEVICE_UNKNOWN, IO_NO_INCREMENT,
    NTSTATUS, PDEVICE_OBJECT, PDRIVER_OBJECT, PIRP, UNICODE_STRING,
};

use crate::common::{
    STATUS_INVALID_BUFFER_SIZE, STATUS_INVALID_DEVICE_REQUEST, STATUS_INVALID_PARAMETER,
    STATUS_SUCCESS,
};
use crate::dbg::dlog;
use crate::logic::{self, MicRing, SpscRing};
use crate::portcls as pc;

/// The mic feed ring. Producer: the IOCTL handler (PASSIVE). Consumer: the
/// capture stream's `GetPosition` (any IRQL).
static MIC_RING: MicRing = SpscRing::new();

/// The control device object (null until `create` succeeds). Only ever
/// transitions null -> device (AddDevice) -> null (DriverUnload).
static CONTROL_DEVICE: AtomicPtr<DEVICE_OBJECT> = AtomicPtr::new(ptr::null_mut());

// NT device name and DOS symlink for the control channel.
static NT_NAME: [u16; 20] = adapter_wide(b"\\Device\\UDSAudioCtl");
static DOS_NAME: [u16; 24] = adapter_wide(b"\\DosDevices\\UDSAudioCtl");

/// Local copy of the adapter's UTF-16 literal helper (kept private there on
/// purpose; duplicating the 10 lines beats exporting a util module).
const fn adapter_wide<const N: usize>(s: &[u8]) -> [u16; N] {
    let mut out = [0u16; N];
    let mut i = 0;
    while i < s.len() {
        out[i] = s[i] as u16;
        i += 1;
    }
    out
}

/// The mic feed ring, for the capture stream.
pub fn mic_ring() -> &'static MicRing {
    &MIC_RING
}

/// The live control device, if created.
fn control_device() -> PDEVICE_OBJECT {
    CONTROL_DEVICE.load(Ordering::Acquire)
}

/// Creates the control device and its DOS symlink. Idempotent: the second
/// and later calls (only possible if several PDOs ever arrive) are no-ops.
/// On failure the audio endpoints still work — the mic just stays silent.
pub fn create(driver: PDRIVER_OBJECT) -> NTSTATUS {
    if !control_device().is_null() {
        return STATUS_SUCCESS;
    }
    // SAFETY: name/dos are static null-terminated UTF-16 buffers; the
    // UNICODE_STRINGs point at them for the duration of the calls.
    unsafe {
        let mut name: UNICODE_STRING = core::mem::zeroed();
        RtlInitUnicodeString(&raw mut name, NT_NAME.as_ptr());
        let mut device: PDEVICE_OBJECT = ptr::null_mut();
        let status = IoCreateDevice(
            driver,
            0,
            &raw mut name,
            FILE_DEVICE_UNKNOWN,
            0,
            0, // not exclusive: several agent processes may push audio
            &raw mut device,
        );
        if status != STATUS_SUCCESS || device.is_null() {
            dlog!("IoCreateDevice failed 0x%I64x", status as u32 as u64);
            return status;
        }
        // METHOD_BUFFERED: the I/O manager copies through SystemBuffer.
        (*device).Flags |= DO_BUFFERED_IO;
        // Clear DO_DEVICE_INITIALIZING so CreateFile can reach us.
        (*device).Flags &= !DO_DEVICE_INITIALIZING;

        let mut link: UNICODE_STRING = core::mem::zeroed();
        RtlInitUnicodeString(&raw mut link, DOS_NAME.as_ptr());
        let link_status = IoCreateSymbolicLink(&raw mut link, &raw mut name);
        if link_status != STATUS_SUCCESS {
            dlog!("IoCreateSymbolicLink failed 0x%I64x", link_status as u32 as u64);
            IoDeleteDevice(device);
            return link_status;
        }
        CONTROL_DEVICE.store(device, Ordering::Release);
        dlog!("control device ready");
        STATUS_SUCCESS
    }
}

/// Deletes the control device and its symlink. Called from DriverUnload
/// before chaining to PortCls so no new IRP can arrive afterwards.
pub fn delete() {
    let device = CONTROL_DEVICE.swap(ptr::null_mut(), Ordering::AcqRel);
    if device.is_null() {
        return;
    }
    // SAFETY: the device is ours and no longer reachable; the symlink name
    // is the static we registered in `create`.
    unsafe {
        let mut link: UNICODE_STRING = core::mem::zeroed();
        RtlInitUnicodeString(&raw mut link, DOS_NAME.as_ptr());
        let _ = IoDeleteSymbolicLink(&raw mut link);
        IoDeleteDevice(device);
    }
    dlog!("control device gone");
}

/// Completes an IRP with the given status and information field.
fn complete(irp: PIRP, status: NTSTATUS, info: usize) -> NTSTATUS {
    // SAFETY: `irp` is a valid IRP owned by this dispatch routine.
    unsafe {
        (*irp).IoStatus.__bindgen_anon_1.Status = status;
        (*irp).IoStatus.Information = info as u64;
        IofCompleteRequest(irp, IO_NO_INCREMENT as i8);
    }
    status
}

/// Whether an IRP arriving at one of our dispatchers targets the control
/// device (as opposed to a PortCls device we must forward).
fn is_control_irp(device: PDEVICE_OBJECT) -> bool {
    !device.is_null() && device == control_device()
}

/// Forwards an IRP to PortCls (what the MajorFunction entry pointed at
/// before we replaced it).
fn forward_to_portcls(device: PDEVICE_OBJECT, irp: PIRP) -> NTSTATUS {
    // SAFETY: device/irp are the ones the I/O manager handed us; PortCls
    // owns the dispatch for every device object except our control one.
    unsafe { pc::PcDispatchIrp(device as pc::PDEVICE_OBJECT, irp as pc::PIRP) }
}

/// IRP_MJ_CREATE / IRP_MJ_CLOSE: the control device accepts and completes;
/// everything else goes to PortCls. wdk-sys types PDRIVER_DISPATCH as
/// `extern "C"` (x64 has a single calling convention, so this is the same
/// ABI as the kernel's `extern "system"`).
pub unsafe extern "C" fn dispatch_create_close(device: PDEVICE_OBJECT, irp: PIRP) -> NTSTATUS {
    if !is_control_irp(device) {
        return forward_to_portcls(device, irp);
    }
    if irp.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    complete(irp, STATUS_SUCCESS, 0)
}

/// IRP_MJ_DEVICE_CONTROL: handles [`logic::IOCTL_PUSH_MIC`] on the control
/// device; everything else goes to PortCls (audio endpoint property IRPs
/// take this path).
///
/// SAFETY: installed in the driver's MajorFunction table; the I/O manager
/// guarantees valid device/irp pointers.
pub unsafe extern "C" fn dispatch_device_control(device: PDEVICE_OBJECT, irp: PIRP) -> NTSTATUS {
    if !is_control_irp(device) {
        return forward_to_portcls(device, irp);
    }
    // SAFETY: `irp` is valid for the duration of the dispatch (checked for
    // null before dereference); the stack location and SystemBuffer belong
    // to this IRP (METHOD_BUFFERED).
    unsafe {
        if irp.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        // IoGetCurrentIrpStackLocation is a macro: this is its expansion.
        let stack = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
        if stack.is_null() {
            return complete(irp, STATUS_INVALID_PARAMETER, 0);
        }
        let params = (*stack).Parameters.DeviceIoControl;
        if params.IoControlCode != logic::IOCTL_PUSH_MIC {
            dlog!("IOCTL rejected 0x%I64x", u64::from(params.IoControlCode));
            return complete(irp, STATUS_INVALID_DEVICE_REQUEST, 0);
        }
        let len = params.InputBufferLength as usize;
        // 48 kHz mono s16: whole frames only.
        if len % 2 != 0 {
            return complete(irp, STATUS_INVALID_BUFFER_SIZE, 0);
        }
        let system_buffer = (*irp).AssociatedIrp.SystemBuffer;
        if len == 0 {
            return complete(irp, STATUS_SUCCESS, 0);
        }
        if system_buffer.is_null() {
            return complete(irp, STATUS_INVALID_PARAMETER, 0);
        }
        let pcm = core::slice::from_raw_parts(system_buffer as *const u8, len);
        let consumed = MIC_RING.push(pcm);
        complete(irp, STATUS_SUCCESS, consumed)
    }
}
