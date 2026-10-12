//! Adapter wiring: `DriverEntry` glue, `AddDevice`, `StartDevice` and the
//! subdevice installation sequence (sysvad `InstallSubdevice` pattern:
//! `PcNewPort` -> create miniport -> `IPort::Init` -> `PcRegisterSubdevice`).
//!
//! Four subdevices are registered: topology + wave for the virtual speaker
//! and for the virtual microphone, with the physical connections between
//! each topology/wave pair so the audio endpoint builder can match them up.

use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::common::{STATUS_INSUFFICIENT_RESOURCES, STATUS_SUCCESS, STATUS_UNSUCCESSFUL};
use crate::control;
use crate::dbg::dlog;
use crate::logic::{self, FilterKind};
use crate::miniport::Miniport;
use crate::portcls as pc;
use crate::tables;

/// Subdevices: topo+wave for speaker, topo+wave for mic.
const MAX_SUBDEVICES: u32 = 4;

/// Builds a null-terminated UTF-16 literal from an ASCII byte string.
const fn wide<const N: usize>(s: &[u8]) -> [u16; N] {
    let mut out = [0u16; N];
    let mut i = 0;
    while i < s.len() {
        out[i] = s[i] as u16;
        i += 1;
    }
    out
}

// Reference strings — must match the KSNAME_* strings in udsaudio.inx.
static NAME_TOPO_SPEAKER: [u16; 15] = wide(b"UDSTopoSpeaker");
static NAME_WAVE_SPEAKER: [u16; 14] = wide(b"UDSWaveSpeaker");
static NAME_TOPO_MIC: [u16; 12] = wide(b"UDSTopoMic");
static NAME_WAVE_MIC: [u16; 11] = wide(b"UDSWaveMic");

/// The unload routine PortCls installed (we chain to it, like sysvad).
static PREVIOUS_UNLOAD: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

type DriverUnloadFn = unsafe extern "C" fn(wdk_sys::PDRIVER_OBJECT);

/// DriverEntry body: hand the driver over to PortCls, then hook unload.
///
/// SAFETY: called once by the kernel with valid driver object and registry
/// path pointers.
pub unsafe fn driver_entry(
    driver: wdk_sys::PDRIVER_OBJECT,
    registry_path: wdk_sys::PCUNICODE_STRING,
) -> wdk_sys::NTSTATUS {
    dlog!("DriverEntry");
    // SAFETY: `driver`/`registry_path` are valid for the duration of the
    // call; PcInitializeAdapterDriver installs the PnP dispatch.
    let status = unsafe {
        pc::PcInitializeAdapterDriver(
            driver as pc::PDRIVER_OBJECT,
            registry_path as pc::PUNICODE_STRING,
            Some(add_device),
        )
    };
    if status != STATUS_SUCCESS {
        dlog!("PcInitializeAdapterDriver failed 0x%I64x", status as u32 as u64);
        return status;
    }
    // Chain our unload hook after PortCls's (it just installed its own).
    // SAFETY: `driver` is valid; reading/replacing DriverUnload is the
    // documented chaining pattern.
    let previous = unsafe { (*driver).DriverUnload };
    PREVIOUS_UNLOAD.store(
        previous.map_or(ptr::null_mut(), |f| f as *mut c_void),
        Ordering::Relaxed,
    );
    unsafe {
        (*driver).DriverUnload = Some(driver_unload);
    }
    // PcInitializeAdapterDriver pointed every MajorFunction at
    // PcDispatchIrp. Take over the three entries the control device needs;
    // our dispatchers forward anything not aimed at the control device
    // straight back to PortCls.
    // SAFETY: `driver` is valid; replacing MajorFunction entries is the
    // documented way to intercept IRPs.
    unsafe {
        let major = &mut (*driver).MajorFunction;
        major[wdk_sys::IRP_MJ_CREATE as usize] = Some(control::dispatch_create_close);
        major[wdk_sys::IRP_MJ_CLOSE as usize] = Some(control::dispatch_create_close);
        major[wdk_sys::IRP_MJ_DEVICE_CONTROL as usize] = Some(control::dispatch_device_control);
    }
    dlog!("DriverEntry ok");
    STATUS_SUCCESS
}

/// Unload hook: chains to the routine PortCls registered.
///
/// wdk-sys types `PDRIVER_UNLOAD` as `extern "C"` (x64 has a single calling
/// convention, so this matches the kernel's `extern "system"` at the ABI
/// level).
unsafe extern "C" fn driver_unload(driver: wdk_sys::PDRIVER_OBJECT) {
    dlog!("DriverUnload");
    // Delete the control device first so no new IOCTL can arrive while
    // PortCls unwinds the audio devices.
    control::delete();
    let previous = PREVIOUS_UNLOAD.load(Ordering::Relaxed);
    if !previous.is_null() {
        // SAFETY: the pointer came from the DriverUnload field PortCls set
        // during PcInitializeAdapterDriver.
        let previous: DriverUnloadFn = unsafe { core::mem::transmute(previous) };
        unsafe { previous(driver) };
    }
}

/// PnP AddDevice: PortCls creates the functional device object and takes
/// over PnP/power dispatch for it. Also creates the user-mode control
/// device (mic feed IOCTL channel); a failure there is logged but does not
/// fail audio device creation.
unsafe extern "C" fn add_device(
    driver: *mut pc::_DRIVER_OBJECT,
    physical_device: *mut pc::_DEVICE_OBJECT,
) -> i32 {
    dlog!("AddDevice");
    // SAFETY: pointers come from the PnP manager via PortCls.
    let status = unsafe {
        pc::PcAddAdapterDevice(driver, physical_device, Some(start_device), MAX_SUBDEVICES, 0)
    };
    if status != STATUS_SUCCESS {
        dlog!("PcAddAdapterDevice failed 0x%I64x", status as u32 as u64);
        return status;
    }
    // The driver object is the same one DriverEntry got; wdk-sys and our
    // bindgen bindings type it differently but it is the same pointer.
    control::create(driver as wdk_sys::PDRIVER_OBJECT);
    status
}

/// Releases a port reference obtained from `PcNewPort`.
///
/// SAFETY: `port` must be a live port with a reference owned by the caller.
unsafe fn port_release(port: pc::PPORT) {
    if port.is_null() {
        return;
    }
    // SAFETY: the port vtable is owned by PortCls; Release with our
    // reference is always safe.
    unsafe {
        if let Some(release) = (*(*port).lpVtbl).Release {
            release(port as *mut pc::INTERFACE);
        }
    }
}

/// Installs one subdevice (port + miniport) and registers it. On success
/// returns the port with one reference still held by the caller (for
/// physical connection registration); the caller must [`port_release`] it.
fn install_subdevice(
    device: pc::PDEVICE_OBJECT,
    irp: pc::PIRP,
    resources: pc::PRESOURCELIST,
    name: &'static [u16],
    port_clsid: &'static pc::GUID,
    kind: FilterKind,
) -> Result<pc::PPORT, i32> {
    let mut port: pc::PPORT = ptr::null_mut();
    // SAFETY: `port` is a valid out param; `port_clsid` is a static GUID.
    let mut status = unsafe { pc::PcNewPort(&mut port, port_clsid) };
    if status != STATUS_SUCCESS || port.is_null() {
        dlog!("PcNewPort failed 0x%I64x", status as u32 as u64);
        return Err(if status == STATUS_SUCCESS {
            STATUS_UNSUCCESSFUL
        } else {
            status
        });
    }

    let is_wave = matches!(kind, FilterKind::RenderWave | FilterKind::CaptureWave);
    let miniport = if is_wave {
        Miniport::create_wave(kind)
    } else {
        Miniport::create_topology(kind)
    };
    if miniport.is_null() {
        // SAFETY: port is ours.
        unsafe { port_release(port) };
        return Err(STATUS_INSUFFICIENT_RESOURCES);
    }

    // SAFETY: port and miniport are live objects with one reference each;
    // the port takes its own reference to the miniport in Init.
    status = unsafe {
        let vtbl = (*port).lpVtbl;
        match (*vtbl).Init {
            Some(init) => init(
                port as *mut pc::INTERFACE,
                device,
                irp,
                miniport as pc::PUNKNOWN,
                ptr::null_mut(),
                resources,
            ),
            None => STATUS_UNSUCCESSFUL,
        }
    };
    // Our creation reference to the miniport is released either way: on
    // success the port holds the long-term reference, on failure the
    // miniport is destroyed.
    // SAFETY: `miniport` was created with exactly one reference, ours.
    unsafe { Miniport::release_reference(miniport) };

    if status != STATUS_SUCCESS {
        dlog!("port Init failed 0x%I64x", status as u32 as u64);
        // SAFETY: port is ours.
        unsafe { port_release(port) };
        return Err(status);
    }

    // SAFETY: `name` is a null-terminated static UTF-16 string.
    status = unsafe { pc::PcRegisterSubdevice(device, name.as_ptr() as pc::PWSTR, port as pc::PUNKNOWN) };
    if status != STATUS_SUCCESS {
        dlog!("PcRegisterSubdevice failed 0x%I64x", status as u32 as u64);
        // SAFETY: port is ours.
        unsafe { port_release(port) };
        return Err(status);
    }
    dlog!("subdevice registered");
    Ok(port)
}

/// Registers the physical connection between a wave filter bridge pin and
/// the matching topology filter pin, so Windows presents one endpoint.
fn connect_filters(
    device: pc::PDEVICE_OBJECT,
    from_port: pc::PPORT,
    from_pin: u32,
    to_port: pc::PPORT,
    to_pin: u32,
) {
    // SAFETY: both ports are registered subdevices of `device`.
    let status = unsafe {
        pc::PcRegisterPhysicalConnection(
            device,
            from_port as pc::PUNKNOWN,
            from_pin,
            to_port as pc::PUNKNOWN,
            to_pin,
        )
    };
    if status != STATUS_SUCCESS {
        dlog!("PcRegisterPhysicalConnection failed 0x%I64x", status as u32 as u64);
    }
}

/// IRP_MN_START_DEVICE handler called by PortCls: creates and registers the
/// four subdevices.
unsafe extern "C" fn start_device(
    device: pc::PDEVICE_OBJECT,
    irp: pc::PIRP,
    resources: pc::PRESOURCELIST,
) -> i32 {
    dlog!("StartDevice");

    // Speaker pair.
    let topo_speaker = match install_subdevice(
        device,
        irp,
        resources,
        &NAME_TOPO_SPEAKER,
        &tables::CLSID_PORT_TOPOLOGY,
        FilterKind::RenderTopo,
    ) {
        Ok(port) => port,
        Err(status) => return status,
    };
    let wave_speaker = match install_subdevice(
        device,
        irp,
        resources,
        &NAME_WAVE_SPEAKER,
        &tables::CLSID_PORT_WAVE_RT,
        FilterKind::RenderWave,
    ) {
        Ok(port) => port,
        Err(status) => {
            // SAFETY: topo_speaker is ours.
            unsafe { port_release(topo_speaker) };
            return status;
        }
    };
    connect_filters(
        device,
        wave_speaker,
        logic::RENDER_WAVE_PIN_BRIDGE,
        topo_speaker,
        logic::RENDER_TOPO_PIN_WAVE,
    );
    // SAFETY: both ports are ours to release.
    unsafe {
        port_release(topo_speaker);
        port_release(wave_speaker);
    }

    // Microphone pair.
    let topo_mic = match install_subdevice(
        device,
        irp,
        resources,
        &NAME_TOPO_MIC,
        &tables::CLSID_PORT_TOPOLOGY,
        FilterKind::CaptureTopo,
    ) {
        Ok(port) => port,
        Err(status) => return status,
    };
    let wave_mic = match install_subdevice(
        device,
        irp,
        resources,
        &NAME_WAVE_MIC,
        &tables::CLSID_PORT_WAVE_RT,
        FilterKind::CaptureWave,
    ) {
        Ok(port) => port,
        Err(status) => {
            // SAFETY: topo_mic is ours.
            unsafe { port_release(topo_mic) };
            return status;
        }
    };
    connect_filters(
        device,
        topo_mic,
        logic::CAPTURE_TOPO_PIN_WAVE,
        wave_mic,
        logic::CAPTURE_WAVE_PIN_BRIDGE,
    );
    // SAFETY: both ports are ours to release.
    unsafe {
        port_release(topo_mic);
        port_release(wave_mic);
    }

    dlog!("StartDevice ok");
    STATUS_SUCCESS
}
