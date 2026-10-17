//! Desktop topology helper: make sure the UDS virtual display is attached
//! to the desktop (extended) before capture starts.
//!
//! Windows does not auto-extend the desktop onto a freshly attached IddCx
//! monitor in every environment (VMs on the Basic Render driver don't), and
//! DXGI capture can only duplicate outputs that are part of the desktop.
//! The fix must run in the interactive session — display topology changes
//! from session 0 do not stick — so it lives in the agent, not the service.

use anyhow::{Context as _, bail};
use std::time::Duration;
use tracing::{info, warn};
use windows::Win32::Graphics::Gdi::{
    CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP,
    DISPLAY_DEVICEW, DM_BITSPERPEL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, DISP_CHANGE,
    ENUM_DISPLAY_SETTINGS_MODE, EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::core::PCWSTR;

const DISP_CHANGE_OK: DISP_CHANGE = DISP_CHANGE(0);

/// DeviceString substring identifying our IddCx adapter's displays.
const UDS_DISPLAY_MARKER: &str = "UDS Virtual Display";

/// Attaches every detached UDS virtual display to the desktop, extending to
/// the right of the current desktop. Best effort: errors are logged, never
/// fatal — the capture startup reports the definitive error if the output
/// is still missing.
pub fn ensure_uds_displays_attached(width: u32, height: u32, vsync: u32) {
    match ensure_inner(width, height, vsync) {
        Ok(0) => info!("no detached UDS virtual display needed attaching"),
        Ok(n) => info!(attached = n, "attached UDS virtual display(s) to the desktop"),
        Err(err) => warn!(error = %format!("{err:#}"), "cannot fix the display topology"),
    }
}

fn ensure_inner(width: u32, height: u32, vsync: u32) -> anyhow::Result<u32> {
    let mut attached = 0u32;
    for i in 0..32u32 {
        let mut dd = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        // SAFETY: `dd` is a correctly sized out-struct.
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) }.as_bool() {
            break;
        }
        if !utf16_z(&dd.DeviceString).contains(UDS_DISPLAY_MARKER) {
            continue;
        }
        if dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP == DISPLAY_DEVICE_ATTACHED_TO_DESKTOP {
            continue;
        }
        let name = utf16_z(&dd.DeviceName);
        attach_with_retries(&name, width, height, vsync)
            .with_context(|| format!("attaching {name} to the desktop"))?;
        attached += 1;
    }
    Ok(attached)
}

/// Attach attempts before giving up (the IddCx monitor can take seconds to
/// become fully ready after arrival; a detached display rejects mode sets
/// with `DISP_CHANGE_BADMODE` until then).
const ATTACH_ATTEMPTS: u32 = 20;

/// Attaches `\\.\DISPLAYn`, retrying while the monitor settles. Each round
/// first re-pushes the mode through the driver (`IddCxMonitorUpdateModes`
/// forces the OS to re-evaluate the monitor, which attaches it on its own)
/// and then tries the GDI attach as a fallback.
fn attach_with_retries(
    device_name: &str,
    width: u32,
    height: u32,
    vsync: u32,
) -> anyhow::Result<()> {
    let mut last_err = anyhow::anyhow!("no attach attempt run");
    for attempt in 1..=ATTACH_ATTEMPTS {
        nudge_driver_modes(width, height, vsync);
        std::thread::sleep(Duration::from_millis(500));
        if is_attached(device_name) {
            info!(device = device_name, attempt, "display attached after the driver mode update");
            return Ok(());
        }
        match attach_display(device_name, width, height, vsync) {
            Ok(()) => return Ok(()),
            Err(err) => last_err = err,
        }
    }
    Err(last_err.context(format!("giving up after {ATTACH_ATTEMPTS} attempts")))
}

/// Whether `\\.\DISPLAYn` is currently part of the desktop.
fn is_attached(device_name: &str) -> bool {
    let wide: Vec<u16> = device_name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut dd = DISPLAY_DEVICEW {
        cb: size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };
    // SAFETY: `wide` is null-terminated; `dd` is a correctly sized
    // out-struct. iDevNum 0 is ignored when a device name is given.
    if !unsafe { EnumDisplayDevicesW(PCWSTR(wide.as_ptr()), 0, &mut dd, 0) }.as_bool() {
        return false;
    }
    dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP == DISPLAY_DEVICE_ATTACHED_TO_DESKTOP
}

/// Re-pushes the requested mode to every connected monitor of our IddCx
/// driver. `IddCxMonitorUpdateModes` forces the OS to re-evaluate the
/// monitor, which attaches it to the desktop when it was left detached.
/// Best effort: a missing driver is not an error here.
fn nudge_driver_modes(width: u32, height: u32, vsync: u32) {
    let nudge = || -> anyhow::Result<()> {
        let client = udsdisp_client::client::Client::open()?;
        let mode = udsdisp_client::client::Mode { width, height, vsync };
        for mon in client.list()? {
            if mon.connected {
                client.set_mode(mon.connector_index, mode)?;
            }
        }
        Ok(())
    };
    if let Err(err) = nudge() {
        warn!(error = %format!("{err:#}"), "driver mode nudge failed");
    }
}

/// Attaches `\\.\DISPLAYn` to the desktop with the given mode.
///
/// Uses the recipe proven with `udsdisp-ctl set-mode`: no explicit position
/// (Windows places the display itself) and `CDS_UPDATEREGISTRY` without
/// `CDS_NORESET`, so the change applies immediately. A hand-built `DEVMODE`
/// is rejected with `DISP_CHANGE_BADMODE` by drivers whose mode list does
/// not match it verbatim (IddCx), so the mode is picked from the driver's
/// own enumerated list first; the hand-built one is only a fallback.
fn attach_display(device_name: &str, width: u32, height: u32, vsync: u32) -> anyhow::Result<()> {
    let wide: Vec<u16> = device_name.encode_utf16().chain(std::iter::once(0)).collect();
    let devmode = find_driver_mode(&wide, width, height, vsync).unwrap_or_else(|| DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        dmFields: DM_BITSPERPEL | DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY,
        dmBitsPerPel: 32,
        dmPelsWidth: width,
        dmPelsHeight: height,
        dmDisplayFrequency: vsync,
        ..Default::default()
    });
    // SAFETY: `wide` is null-terminated; `devmode` is fully initialized.
    // CDS_UPDATEREGISTRY applies the change right away (no CDS_NORESET).
    let result = unsafe {
        ChangeDisplaySettingsExW(
            PCWSTR(wide.as_ptr()),
            Some(&raw const devmode),
            None,
            CDS_UPDATEREGISTRY,
            None,
        )
    };
    if result != DISP_CHANGE_OK {
        bail!("ChangeDisplaySettingsExW({device_name}) -> {} ({result:?})", result.0);
    }
    info!(device = device_name, width, height, "display attached to the desktop");
    Ok(())
}

/// Picks a driver-reported mode matching `width`x`height`@`vsync` from the
/// display's own mode list, so the attach uses a `DEVMODE` the driver
/// accepts verbatim.
fn find_driver_mode(device_wide: &[u16], width: u32, height: u32, vsync: u32) -> Option<DEVMODEW> {
    let mut listed: Vec<String> = Vec::new();
    let mut found = None;
    for i in 0.. {
        let mut devmode = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // SAFETY: `device_wide` is null-terminated; `devmode` is a sized
        // out-struct.
        if !unsafe {
            EnumDisplaySettingsW(
                PCWSTR(device_wide.as_ptr()),
                ENUM_DISPLAY_SETTINGS_MODE(i),
                &raw mut devmode,
            )
        }
        .as_bool()
        {
            break;
        }
        if listed.len() < 8 {
            listed.push(format!(
                "{}x{}@{} bpp={}",
                devmode.dmPelsWidth,
                devmode.dmPelsHeight,
                devmode.dmDisplayFrequency,
                devmode.dmBitsPerPel
            ));
        }
        if found.is_none()
            && devmode.dmPelsWidth == width
            && devmode.dmPelsHeight == height
            && devmode.dmDisplayFrequency == vsync
        {
            found = Some(devmode);
        }
    }
    info!(
        modes = ?listed,
        matched = found.is_some(),
        "driver mode list for the display"
    );
    found
}

/// Lossy string from a null-terminated UTF-16 fixed buffer.
fn utf16_z(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}
