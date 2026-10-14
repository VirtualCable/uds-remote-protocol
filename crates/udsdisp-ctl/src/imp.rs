//! Windows implementation of udsdisp-ctl. See `main.rs` for the command
//! list. The driver is UMDF, so the control channel is a device interface
//! (GUID in `udsdisp::logic`) opened through SetupAPI + CreateFile, with the
//! wire protocol encoded/decoded by `udsdisp::logic` (unit-tested on Linux).

// The `windows` crate FFI methods take raw pointers; implicit borrows are
// the idiomatic call style for that API (same policy as udsaudio-ctl).
#![allow(clippy::borrow_as_ptr)]

use anyhow::{Context as _, bail};
use udsdisp::logic;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Graphics::Gdi::{
    CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW, DISPLAY_DEVICEW, DM_BITSPERPEL,
    DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, DISP_CHANGE, EnumDisplayDevicesW,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::{GUID, PCWSTR};

/// `DISP_CHANGE_SUCCESSFUL`.
const DISP_CHANGE_OK: DISP_CHANGE = DISP_CHANGE(0);

/// CLI entry.
pub fn run() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map_or("help", String::as_str);
    match cmd {
        "list" => list(),
        "create" => {
            let (connector, mode) = parse_connector_mode(&args[1..])?;
            create(connector, mode)
        }
        "set-mode" => {
            let (connector, mode) = parse_connector_mode(&args[1..])?;
            set_mode(connector, mode)
        }
        "remove" => {
            let connector = parse_connector(&args[1..])?;
            remove(connector)
        }
        _ => {
            eprintln!(
                "usage: udsdisp-ctl <list | create | set-mode | remove> [connector] [WxH[@HZ]]"
            );
            std::process::exit(64);
        }
    }
}

/// Parses `<connector> <WxH[@HZ]>`.
fn parse_connector_mode(args: &[String]) -> anyhow::Result<(u32, logic::Mode)> {
    let connector = parse_connector(args)?;
    let mode_text = args.get(1).context("missing mode (WxH[@HZ])")?;
    let mode = logic::parse_mode(mode_text)
        .with_context(|| format!("invalid mode '{mode_text}' (expected WxH[@HZ])"))?;
    anyhow::ensure!(mode.is_supported(), "mode {mode:?} is not in the driver target modes");
    Ok((connector, mode))
}

/// Parses `<connector>`.
fn parse_connector(args: &[String]) -> anyhow::Result<u32> {
    let connector: u32 = args
        .first()
        .context("missing connector index")?
        .parse()
        .context("connector must be a number (0-3)")?;
    anyhow::ensure!(connector < logic::MAX_MONITORS, "connector must be 0-{}", logic::MAX_MONITORS - 1);
    Ok(connector)
}

// --- control device ---------------------------------------------------------

/// RAII wrapper for the control-device handle.
struct ControlDevice(HANDLE);

impl ControlDevice {
    /// Opens the first present device exposing our device interface GUID.
    fn open() -> anyhow::Result<Self> {
        let path = find_device_path()?;
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a valid null-terminated path; no template, no
        // security attributes.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .context("cannot open the udsdisp device interface (is the driver installed?)")?;
        Ok(Self(handle))
    }

    /// Sends an IOCTL with optional input/output buffers; returns the number
    /// of output bytes the driver reported.
    fn ioctl(&self, code: u32, input: Option<&[u8]>, output: Option<&mut [u8]>) -> anyhow::Result<u32> {
        let (in_ptr, in_len) = input.map_or((None, 0), |b| (Some(b.as_ptr().cast()), b.len() as u32));
        let (out_ptr, out_len) = match output {
            Some(b) => (Some(b.as_mut_ptr().cast()), b.len() as u32),
            None => (None, 0),
        };
        let mut returned = 0u32;
        // SAFETY: the buffers are valid for the given lengths and outlive the
        // call; `returned` is a valid out-pointer; no overlapped.
        unsafe {
            DeviceIoControl(
                self.0,
                code,
                in_ptr,
                in_len,
                out_ptr,
                out_len,
                Some(&mut returned),
                None,
            )
        }
        .with_context(|| format!("DeviceIoControl({code:#010x}) failed"))?;
        Ok(returned)
    }
}

impl Drop for ControlDevice {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and open.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// RAII guard for a SetupAPI device-info set.
struct DevInfoSet(HDEVINFO);

impl Drop for DevInfoSet {
    fn drop(&mut self) {
        // SAFETY: the set is ours.
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

/// The driver's device interface GUID as a `windows::core::GUID` (the bytes
/// in `logic::DEVICE_INTERFACE_GUID_BYTES` are the little-endian field
/// layout).
fn interface_guid() -> GUID {
    let b = logic::DEVICE_INTERFACE_GUID_BYTES;
    GUID::from_values(
        u32::from_le_bytes(b[0..4].try_into().expect("16-byte GUID")),
        u16::from_le_bytes(b[4..6].try_into().expect("16-byte GUID")),
        u16::from_le_bytes(b[6..8].try_into().expect("16-byte GUID")),
        b[8..16].try_into().expect("16-byte GUID"),
    )
}

/// Enumerates our device interface GUID and returns the device path of the
/// first present instance (there is exactly one udsdisp device per machine).
fn find_device_path() -> anyhow::Result<String> {
    let guid = interface_guid();
    // SAFETY: read-only enumeration; the GUID is a plain value.
    let set = unsafe {
        SetupDiGetClassDevsW(
            Some(&guid),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    }
    .context("SetupDiGetClassDevsW failed")?;
    let set = DevInfoSet(set);

    let mut data = SP_DEVICE_INTERFACE_DATA {
        cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
        ..Default::default()
    };
    // SAFETY: `set` is live; `data` is a correctly sized out-struct.
    unsafe { SetupDiEnumDeviceInterfaces(set.0, None, &guid, 0, &mut data) }
        .context("udsdisp device interface not found (is the driver installed?)")?;

    // Two-call sizing for the variable-length detail structure.
    let mut required = 0u32;
    // SAFETY: expected to fail with ERROR_INSUFFICIENT_BUFFER and report the
    // required size; all pointers are valid.
    let _ = unsafe {
        SetupDiGetDeviceInterfaceDetailW(set.0, &data, None, 0, Some(&mut required), None)
    };
    anyhow::ensure!(required > 0, "SetupDiGetDeviceInterfaceDetailW reported no size");
    let mut buffer = vec![0u16; required as usize / 2 + 1];
    // SAFETY: `buffer` is `required` bytes; cbSize is the fixed part size, as
    // documented.
    unsafe {
        let detail = buffer.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        SetupDiGetDeviceInterfaceDetailW(set.0, &data, Some(detail), required, None, None)
            .context("SetupDiGetDeviceInterfaceDetailW failed")?;
        let path_ptr = (*detail).DevicePath.as_ptr();
        let len = (0..).take_while(|&i| *path_ptr.add(i) != 0).count();
        Ok(String::from_utf16_lossy(core::slice::from_raw_parts(path_ptr, len)))
    }
}

// --- subcommands --------------------------------------------------------------

/// `list`: dump every connector slot.
fn list() -> anyhow::Result<()> {
    let device = ControlDevice::open()?;
    let mut buf = vec![0u8; logic::LIST_RESPONSE_LEN];
    device.ioctl(logic::IOCTL_LIST, None, Some(&mut buf))?;
    let statuses = logic::decode_list(&buf).context("driver returned a malformed list")?;
    for status in &statuses {
        if status.connected {
            let m = &status.mode;
            println!(
                "connector {}: CONNECTED {}x{}@{}Hz",
                status.connector_index, m.width, m.height, m.vsync
            );
        } else {
            println!("connector {}: -", status.connector_index);
        }
    }
    Ok(())
}

/// `create`: hot-plug a monitor with the given preferred mode.
fn create(connector: u32, mode: logic::Mode) -> anyhow::Result<()> {
    let device = ControlDevice::open()?;
    let body = logic::encode_mode_request(connector, mode);
    device.ioctl(logic::IOCTL_CREATE, Some(&body), None)?;
    println!(
        "monitor created on connector {connector} ({}x{}@{}Hz preferred)",
        mode.width, mode.height, mode.vsync
    );
    println!(
        "note: if the desktop does not extend onto it automatically, run \
         `displayswitch /extend` from the interactive session"
    );
    Ok(())
}

/// `remove`: unplug and destroy a monitor.
fn remove(connector: u32) -> anyhow::Result<()> {
    let device = ControlDevice::open()?;
    let body = logic::encode_connector(connector);
    device.ioctl(logic::IOCTL_REMOVE, Some(&body), None)?;
    println!("monitor on connector {connector} removed");
    Ok(())
}

/// `set-mode`: push the new preferred mode into the driver
/// (`IddCxMonitorUpdateModes`) and then best-effort apply it to the desktop
/// with `ChangeDisplaySettingsEx`. The driver side works from any session;
/// the desktop switch only succeeds in the interactive console session.
fn set_mode(connector: u32, mode: logic::Mode) -> anyhow::Result<()> {
    let device = ControlDevice::open()?;
    let body = logic::encode_mode_request(connector, mode);
    device.ioctl(logic::IOCTL_SET_MODE, Some(&body), None)?;
    println!(
        "driver accepted mode {}x{}@{}Hz for connector {connector}",
        mode.width, mode.height, mode.vsync
    );
    match display_device_for_connector(connector) {
        Ok(name) => match apply_mode(&name, mode) {
            Ok(()) => println!("desktop switched {name} to the new mode"),
            Err(e) => println!("note: desktop switch on {name} failed ({e:#}); the new mode is still offered to the OS"),
        },
        Err(e) => println!("note: cannot map connector {connector} to a display device ({e:#})"),
    }
    Ok(())
}

// --- desktop-side mode application -------------------------------------------

/// Maps a connector index to its GDI display device (`\\.\DISPLAYx`): the OS
/// creates one display device per connector in order, so the UDS display
/// devices sorted by number line up with connector indices.
fn display_device_for_connector(connector: u32) -> anyhow::Result<String> {
    let mut uds_displays: Vec<String> = Vec::new();
    for i in 0..32u32 {
        let mut dd = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        // SAFETY: `dd` is a correctly sized out-struct.
        let ok = unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) };
        if !ok.as_bool() {
            break;
        }
        let device_string = utf16_z(&dd.DeviceString);
        if device_string.to_lowercase().contains("uds") {
            uds_displays.push(utf16_z(&dd.DeviceName));
        }
    }
    uds_displays.sort();
    uds_displays
        .get(connector as usize)
        .cloned()
        .with_context(|| format!("only {} UDS display device(s) found", uds_displays.len()))
}

/// Applies `mode` to the given display device with `ChangeDisplaySettingsEx`
/// (same recipe verified in the M2 spike: bits-per-pixel included in
/// `dmFields` or the call returns `DISP_CHANGE_BADMODE`).
fn apply_mode(device_name: &str, mode: logic::Mode) -> anyhow::Result<()> {
    let wide: Vec<u16> = device_name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut devmode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        dmFields: DM_BITSPERPEL | DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY,
        dmBitsPerPel: 32,
        dmPelsWidth: mode.width,
        dmPelsHeight: mode.height,
        dmDisplayFrequency: mode.vsync,
        ..Default::default()
    };
    // SAFETY: `wide` is null-terminated; `devmode` is fully initialized.
    let result = unsafe {
        ChangeDisplaySettingsExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut devmode),
            None,
            CDS_UPDATEREGISTRY,
            None,
        )
    };
    if result == DISP_CHANGE_OK {
        Ok(())
    } else {
        bail!("ChangeDisplaySettingsExW -> {} ({result:?})", result.0)
    }
}

/// Lossy string from a null-terminated UTF-16 fixed buffer.
fn utf16_z(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}
