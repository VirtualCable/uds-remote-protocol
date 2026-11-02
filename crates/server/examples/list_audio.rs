// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Lists every active MMDevice audio endpoint (render and capture) with its
//! friendly name, endpoint ID and state, to locate the driver-provided
//! loopback capture endpoint of the UDS virtual audio device.
//!
//! Usage: list_audio

#[cfg(all(windows, feature = "windows-capture"))]
fn main() -> anyhow::Result<()> {
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Media::Audio::{
        DEVICE_STATE_ACTIVE, DEVICE_STATE_DISABLED, DEVICE_STATE_NOTPRESENT,
        DEVICE_STATE_UNPLUGGED, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eRender,
    };
    use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize, STGM_READ,
    };

    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;

    for (flow, label) in [(eRender, "RENDER "), (eCapture, "CAPTURE")] {
        // All states, so a present-but-unplugged endpoint still shows up.
        let mask = windows::Win32::Media::Audio::DEVICE_STATE(
            DEVICE_STATE_ACTIVE.0
                | DEVICE_STATE_DISABLED.0
                | DEVICE_STATE_NOTPRESENT.0
                | DEVICE_STATE_UNPLUGGED.0,
        );
        let collection = unsafe { enumerator.EnumAudioEndpoints(flow, mask) }?;
        let count = unsafe { collection.GetCount() }?;
        for i in 0..count {
            let device = unsafe { collection.Item(i) }?;
            let id = unsafe { device.GetId() }?;
            let id = unsafe { id.to_string() }.unwrap_or_default();
            let state = unsafe { device.GetState() }?.0;
            let name = unsafe {
                let store = device.OpenPropertyStore(STGM_READ)?;
                let value = store.GetValue(&PKEY_Device_FriendlyName)?;
                let text = PropVariantToStringAlloc(&value).unwrap_or_default();
                let out = text.to_string().unwrap_or_default();
                CoTaskMemFree(Some(text.0.cast()));
                out
            };
            println!("{label} state=0x{state:08x} {name} [{id}]");
        }
    }
    unsafe { CoUninitialize() };
    Ok(())
}

#[cfg(not(all(windows, feature = "windows-capture")))]
fn main() {
    eprintln!("list_audio needs Windows with the windows-capture feature");
    std::process::exit(1);
}
