//! Windows implementation of the UDS Virtual Display driver (UMDF).

mod iddcx;

use std::{fs::OpenOptions, io::Write as _, sync::Once};

use wdk::println;
use wdk_sys::{
    NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, ULONG, WDFDEVICE, WDFDEVICE_INIT, WDFDRIVER,
    WDF_DRIVER_CONFIG, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES, call_unsafe_wdf_function_binding,
};

/// Appends one line to the driver's log file and to the debug output.
///
/// UMDF host processes run in Session 0 with no console; a plain file under
/// ProgramData (writable by the WUDFHost service account) is the most
/// reliable channel in the lab, verifiable through labexec.
fn log_line(msg: &str) {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = std::fs::create_dir_all(r"C:\ProgramData\UdsDisp");
    });
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(r"C:\ProgramData\UdsDisp\udsdisp.log")
    {
        let _ = writeln!(file, "{msg}");
    }
    println!("udsdisp: {msg}");
}

/// `DriverEntry` required by WDF.
///
/// # Safety
/// Called by the WDF loader with valid pointers.
// SAFETY: "DriverEntry" is the required symbol name for Windows driver entry
// points. No other function in this compilation unit exports this name.
#[cfg_attr(not(test), unsafe(export_name = "DriverEntry"))]
pub unsafe extern "system" fn driver_entry(
    driver: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    log_line("DriverEntry");

    let mut driver_config = WDF_DRIVER_CONFIG {
        Size: {
            const SIZE: usize = core::mem::size_of::<WDF_DRIVER_CONFIG>();
            const { assert!(SIZE <= ULONG::MAX as usize) }
            #[allow(clippy::cast_possible_truncation)]
            {
                SIZE as ULONG
            }
        },
        EvtDriverDeviceAdd: Some(evt_driver_device_add),
        EvtDriverUnload: Some(evt_driver_unload),
        ..WDF_DRIVER_CONFIG::default()
    };

    // SAFETY: `driver` and `registry_path` are provided by the loader and are
    // never null; the attributes and handle output may be null.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverCreate,
            driver,
            registry_path,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut driver_config,
            WDF_NO_HANDLE.cast::<WDFDRIVER>(),
        )
    };
    log_line(&format!("WdfDriverCreate -> {status:#010x}"));
    status
}

extern "C" fn evt_driver_device_add(
    _driver: WDFDRIVER,
    mut device_init: *mut WDFDEVICE_INIT,
) -> NTSTATUS {
    log_line("EvtDriverDeviceAdd");

    // Register the IddCx callbacks for this device before creating it.
    // Zeroed + Size is exactly what IDD_CX_CLIENT_CONFIG_INIT does.
    // SAFETY: IDD_CX_CLIENT_CONFIG is a POD of a size field and function
    // pointers; all-zero is the documented empty state.
    let mut idd_config: iddcx::bindings::IDD_CX_CLIENT_CONFIG = unsafe { core::mem::zeroed() };
    idd_config.Size = {
        const SIZE: usize = core::mem::size_of::<iddcx::bindings::IDD_CX_CLIENT_CONFIG>();
        const { assert!(SIZE <= u32::MAX as usize) }
        #[allow(clippy::cast_possible_truncation)]
        {
            SIZE as u32
        }
    };
    // SAFETY: `device_init` comes from the framework and is valid; the config
    // is zeroed except for its size, exactly like IDD_CX_CLIENT_CONFIG_INIT.
    let status = unsafe { iddcx::device_init_config(device_init, &mut idd_config) };
    log_line(&format!("IddCxDeviceInitConfig -> {status:#010x}"));
    if status < 0 {
        return status;
    }

    let mut device: WDFDEVICE = WDF_NO_HANDLE.cast();
    // SAFETY: `device_init` is valid and `device` is a valid out-pointer.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            &mut device_init,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut device,
        )
    };
    log_line(&format!("WdfDeviceCreate -> {status:#010x}"));
    if status < 0 {
        return status;
    }

    // SAFETY: `device` was created from a device-init that went through
    // IddCxDeviceInitConfig.
    let status = unsafe { iddcx::device_initialize(device) };
    log_line(&format!("IddCxDeviceInitialize -> {status:#010x}"));

    // SAFETY: no preconditions beyond the class extension being loaded.
    let (version_status, version) = unsafe { iddcx::get_version() };
    log_line(&format!(
        "IddCxGetVersion -> {version_status:#010x}, version {version:#06x}"
    ));

    status
}

extern "C" fn evt_driver_unload(_driver: WDFDRIVER) {
    log_line("DriverUnload");
}

#[cfg(test)]
mod tests {
    /// The crate must stay testable on any host; the real coverage of the
    /// driver logic happens on the Windows VM.
    #[test]
    fn driver_unload_is_callable() {
        super::evt_driver_unload(core::ptr::null_mut());
    }
}
