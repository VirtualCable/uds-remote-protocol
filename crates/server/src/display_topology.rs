//! Desktop topology helper: make sure the UDS virtual display is attached
//! to the desktop (extended) before capture starts.
//!
//! Windows does not auto-extend the desktop onto a freshly attached IddCx
//! monitor in every environment (VMs on the Basic Render driver don't), and
//! DXGI capture can only duplicate outputs that are part of the desktop.
//! The fix must run in the interactive session — display topology changes
//! from session 0 do not stick — so it lives in the agent, not the service.

use anyhow::{Context as _, bail};
use tracing::{info, warn};
use windows::Win32::Graphics::Gdi::{
    CDS_NORESET, CDS_TYPE, CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW,
    DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, DM_BITSPERPEL, DM_DISPLAYFREQUENCY,
    DM_PELSHEIGHT, DM_PELSWIDTH, DM_POSITION, DISP_CHANGE, ENUM_CURRENT_SETTINGS,
    EnumDisplayDevicesW, EnumDisplaySettingsW,
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
        attach_display(&name, width, height, vsync)
            .with_context(|| format!("attaching {name} to the desktop"))?;
        attached += 1;
    }
    Ok(attached)
}

/// Attaches `\\.\DISPLAYn` to the desktop at the right of the current
/// virtual screen extent with the given mode.
fn attach_display(device_name: &str, width: u32, height: u32, vsync: u32) -> anyhow::Result<()> {
    let origin_x = desktop_right_edge();
    let wide: Vec<u16> = device_name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut devmode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        dmFields: DM_BITSPERPEL | DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY | DM_POSITION,
        dmBitsPerPel: 32,
        dmPelsWidth: width,
        dmPelsHeight: height,
        dmDisplayFrequency: vsync,
        ..Default::default()
    };
    devmode.Anonymous1.Anonymous2.dmPosition.x = origin_x;
    devmode.Anonymous1.Anonymous2.dmPosition.y = 0;
    // SAFETY: `wide` is null-terminated; `devmode` is fully initialized.
    // CDS_UPDATEREGISTRY writes the topology change without applying it;
    // the trailing null call applies it.
    let result = unsafe {
        ChangeDisplaySettingsExW(
            PCWSTR(wide.as_ptr()),
            Some(&raw const devmode),
            None,
            CDS_UPDATEREGISTRY | CDS_NORESET,
            None,
        )
    };
    if result != DISP_CHANGE_OK {
        bail!("ChangeDisplaySettingsExW({device_name}) -> {} ({result:?})", result.0);
    }
    // SAFETY: null device name applies the pending topology changes.
    let apply = unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None) };
    if apply != DISP_CHANGE_OK {
        bail!("applying the topology change -> {} ({apply:?})", apply.0);
    }
    info!(device = device_name, origin_x, width, height, "display attached to the desktop");
    Ok(())
}

/// Right edge of the current desktop: max over attached displays of
/// (position.x + width). Falls back to 0 (place right after nothing).
fn desktop_right_edge() -> i32 {
    let mut edge = 0;
    for i in 0..32u32 {
        let mut dd = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        // SAFETY: `dd` is a correctly sized out-struct.
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) }.as_bool() {
            break;
        }
        if dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP != DISPLAY_DEVICE_ATTACHED_TO_DESKTOP {
            continue;
        }
        let name = utf16_z(&dd.DeviceName);
        let mut devmode = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // SAFETY: `wide` is null-terminated; `devmode` is a sized out-struct.
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        if unsafe { EnumDisplaySettingsW(PCWSTR(wide.as_ptr()), ENUM_CURRENT_SETTINGS, &raw mut devmode) }
            .as_bool()
        {
            // SAFETY: EnumDisplaySettingsW succeeded, so the devmode union
            // holds the printer/display variant with a valid dmPosition.
            let pos = unsafe { devmode.Anonymous1.Anonymous2.dmPosition };
            let right = pos.x + devmode.dmPelsWidth as i32;
            edge = edge.max(right);
        }
    }
    edge
}

/// Lossy string from a null-terminated UTF-16 fixed buffer.
fn utf16_z(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}
