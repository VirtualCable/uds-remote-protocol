//! Desktop topology helper: make sure the UDS virtual display is attached
//! to the desktop (extended) before capture starts.
//!
//! Windows does not auto-extend the desktop onto a freshly attached IddCx
//! monitor in every environment (VMs on the Basic Render driver don't), and
//! DXGI capture can only duplicate outputs that are part of the desktop.
//! The fix must run in the interactive session — display topology changes
//! from session 0 do not stick — so it lives in the agent, not the service.
//!
//! All changes here are **volatile** (no `CDS_UPDATEREGISTRY`, M6): they hold
//! until the display is detached or the session ends, and never poison the
//! persisted display configuration — when the IddCx monitor detaches, the
//! desktop bounding box simply shrinks back (no reboot to recover the
//! physical console, see doc/plan/servicio-y-sesion.md §3.2.2).

use anyhow::bail;
use std::time::Duration;
use tracing::{info, warn};
use windows::Win32::Foundation::POINTL;
use windows::Win32::Graphics::Gdi::{
    CDS_NORESET, CDS_TYPE, ChangeDisplaySettingsExW, DEVMODEW,
    DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, DM_BITSPERPEL, DM_DISPLAYFREQUENCY,
    DM_PELSHEIGHT, DM_PELSWIDTH, DM_POSITION, DISP_CHANGE, ENUM_CURRENT_SETTINGS,
    ENUM_DISPLAY_SETTINGS_MODE, EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::core::PCWSTR;

const DISP_CHANGE_OK: DISP_CHANGE = DISP_CHANGE(0);

/// DeviceString substring identifying our IddCx adapter's displays.
const UDS_DISPLAY_MARKER: &str = "UDS Virtual Display";

/// Logs every display device and the position/resolution of those attached
/// to the desktop. Diagnostic gold when a console agent's pointer mapping
/// breaks: a displaced primary or a widened desktop bounding box is visible
/// here at a glance (§18.29/§18.30 incidents).
pub fn log_desktop_topology() {
    for i in 0..32u32 {
        let mut dd = DISPLAY_DEVICEW {
            cb: u32::try_from(size_of::<DISPLAY_DEVICEW>()).expect("struct fits in u32"),
            ..Default::default()
        };
        // SAFETY: `dd` is a correctly sized out-struct.
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &raw mut dd, 0) }.as_bool() {
            break;
        }
        let name = utf16_z(&dd.DeviceName);
        let attached =
            dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP == DISPLAY_DEVICE_ATTACHED_TO_DESKTOP;
        if !attached {
            continue;
        }
        let detail = current_devmode(&name).map_or_else(
            || "no devmode".to_string(),
            |dm| {
                // SAFETY: plain field reads on an initialized struct.
                let pos = unsafe { dm.Anonymous1.Anonymous2.dmPosition };
                format!(
                    "pos=({},{}) {}x{}@{}Hz",
                    pos.x, pos.y, dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency
                )
            },
        );
        info!(device = %name, uds = is_uds_display(&dd), detail, "desktop display");
    }
}

/// Attaches every detached UDS virtual display to the desktop. Windows
/// auto-places new displays to the right of the existing ones, which widens
/// the desktop bounding box; some console agents (SPICE's vdagent, KVM
/// absolute-pointer tools) scale pointer X against that total width and the
/// physical console mouse ends up offset. To keep the console usable while a
/// remote session is active, attached UDS displays are stacked **below** the
/// desktop instead (the width — and the console's X scale — stays intact).
/// Best effort: errors are logged, never fatal — the capture startup reports
/// the definitive error if the output is still missing.
pub fn ensure_uds_displays_attached(width: u32, height: u32, vsync: u32) {
    match ensure_inner(width, height, vsync) {
        0 => info!("no detached UDS virtual display needed attaching"),
        n => info!(attached = n, "attached UDS virtual display(s) to the desktop"),
    }
    stack_uds_displays_below(height);
}

/// Waits until a UDS virtual display is attached to the desktop (the service
/// attaches the IddCx monitor on demand when a remote session starts, M6),
/// then reconciles the topology (attach stragglers + stack below). Returns
/// `false` on timeout — callers continue anyway so dev runs without the
/// driver still work.
pub fn wait_for_uds_display(timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if uds_display_attached() {
            ensure_uds_displays_attached(1920, 1080, 60);
            return true;
        }
        if std::time::Instant::now() >= deadline {
            warn!("timed out waiting for the UDS virtual display; capturing what exists");
            return false;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Whether any UDS virtual display is currently attached to the desktop.
fn uds_display_attached() -> bool {
    for i in 0..32u32 {
        let mut dd = DISPLAY_DEVICEW {
            cb: u32::try_from(size_of::<DISPLAY_DEVICEW>()).expect("struct fits in u32"),
            ..Default::default()
        };
        // SAFETY: `dd` is a correctly sized out-struct.
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &raw mut dd, 0) }.as_bool() {
            break;
        }
        if dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP == DISPLAY_DEVICE_ATTACHED_TO_DESKTOP
            && is_uds_display(&dd)
        {
            return true;
        }
    }
    false
}

/// Moves every attached UDS virtual display to `(0, desktop_bottom + i*h)`,
/// below the physical desktop, so the desktop bounding box does not grow
/// horizontally (console pointer agents keep a correct X scale). No-op when
/// there are no other displays (the UDS display is then the only one and
/// stays primary at the origin).
fn stack_uds_displays_below(height: u32) {
    // Desktop bottom edge over the non-UDS attached displays.
    let mut bottom = 0i32;
    let mut uds: Vec<String> = Vec::new();
    for i in 0..32u32 {
        let mut dd = DISPLAY_DEVICEW {
            cb: u32::try_from(size_of::<DISPLAY_DEVICEW>()).expect("struct fits in u32"),
            ..Default::default()
        };
        // SAFETY: `dd` is a correctly sized out-struct.
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &raw mut dd, 0) }.as_bool() {
            break;
        }
        if dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP != DISPLAY_DEVICE_ATTACHED_TO_DESKTOP {
            continue;
        }
        let name = utf16_z(&dd.DeviceName);
        if is_uds_display(&dd) {
            uds.push(name);
            continue;
        }
        if let Some(dm) = current_devmode(&name) {
            // SAFETY: plain field reads on an initialized struct.
            let pos = unsafe { dm.Anonymous1.Anonymous2.dmPosition };
            bottom = bottom.max(pos.y + i32::try_from(dm.dmPelsHeight).unwrap_or(i32::MAX));
        }
    }
    if uds.is_empty() || bottom == 0 {
        info!(uds_displays = uds.len(), bottom, "no UDS display stacking needed");
        return;
    }
    for (i, name) in uds.iter().enumerate() {
        let Some(mut dm) = current_devmode(name) else { continue };
        let Ok(i) = i32::try_from(i) else { break };
        let Ok(h) = i32::try_from(height) else { break };
        let Some(y) = bottom.checked_add(i.saturating_mul(h)) else { break };
        dm.Anonymous1.Anonymous2.dmPosition = POINTL { x: 0, y };
        dm.dmFields |= DM_POSITION;
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is null-terminated; `dm` is fully initialized.
        // CDS_NORESET stages the (volatile) change; the final null call
        // applies it. No CDS_UPDATEREGISTRY: nothing is persisted (M6).
        let result = unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(wide.as_ptr()),
                Some(&raw const dm),
                None,
                CDS_NORESET,
                None,
            )
        };
        if result == DISP_CHANGE_OK {
            info!(device = %name, y, "UDS display stacked below the desktop");
        } else {
            warn!(device = %name, "reposition rejected: {}", result.0);
        }
    }
    // Apply all staged changes at once (volatile).
    // SAFETY: null device/devmode applies the staged changes.
    unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None) };
}

/// Current settings of `\\.\DISPLAYn` (position included).
fn current_devmode(device_name: &str) -> Option<DEVMODEW> {
    let wide: Vec<u16> = device_name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut dm = DEVMODEW {
        dmSize: u16::try_from(size_of::<DEVMODEW>()).expect("struct fits in u16"),
        ..Default::default()
    };
    // SAFETY: `wide` is null-terminated; `dm` is a sized out-struct.
    unsafe { EnumDisplaySettingsW(PCWSTR(wide.as_ptr()), ENUM_CURRENT_SETTINGS, &raw mut dm) }
        .as_bool()
        .then_some(dm)
}

/// Whether the display device `dd` (an adapter entry from the desktop
/// enumeration) is one of our IddCx virtual displays. The adapter's own
/// `DeviceString` may read "Microsoft Basic Render Driver" on hosts without
/// a real GPU, hiding our marker, so the `DeviceID`s (the adapter's carries
/// `ROOT\udsdisp` from our INF) and the connected monitors' strings are
/// checked too — same identity rule as capture's `gdi_name`.
fn is_uds_display(dd: &DISPLAY_DEVICEW) -> bool {
    let matches = |s: &str| {
        let s = s.to_lowercase();
        s.contains(&UDS_DISPLAY_MARKER.to_lowercase()) || s.contains("udsdisp")
    };
    if matches(&utf16_z(&dd.DeviceString)) || matches(&utf16_z(&dd.DeviceID)) {
        return true;
    }
    let wide: Vec<u16> = utf16_z(&dd.DeviceName).encode_utf16().chain(std::iter::once(0)).collect();
    for dev_num in 0..4u32 {
        let mut mon = DISPLAY_DEVICEW {
            cb: u32::try_from(size_of::<DISPLAY_DEVICEW>()).expect("struct fits in u32"),
            ..Default::default()
        };
        // SAFETY: `wide` is null-terminated; `mon` is a sized out-struct.
        if !unsafe { EnumDisplayDevicesW(PCWSTR(wide.as_ptr()), dev_num, &raw mut mon, 0) }
            .as_bool()
        {
            break;
        }
        if matches(&utf16_z(&mon.DeviceString)) || matches(&utf16_z(&mon.DeviceID)) {
            return true;
        }
    }
    false
}

/// Whether the display device has a monitor connected behind it. Stale GDI
/// entries of past monitor incarnations ("ghosts") never do, so this cheaply
/// filters them out: without it every session burned ~10 s retrying the
/// attach of each ghost (6 attempts x 500 ms that could never succeed).
fn has_live_monitor(device_name: &str) -> bool {
    let wide: Vec<u16> = device_name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut mon = DISPLAY_DEVICEW {
        cb: u32::try_from(size_of::<DISPLAY_DEVICEW>()).expect("struct fits in u32"),
        ..Default::default()
    };
    // SAFETY: `wide` is null-terminated; `mon` is a correctly sized
    // out-struct. Returns false when no monitor is connected.
    unsafe { EnumDisplayDevicesW(PCWSTR(wide.as_ptr()), 0, &raw mut mon, 0) }.as_bool()
}

/// Ghost filter with a short settle window: a *real* display may need a
/// moment after the IddCx monitor arrives for PnP to link them, so probe
/// twice before concluding the entry is a ghost.
fn has_live_monitor_settled(device_name: &str) -> bool {
    if has_live_monitor(device_name) {
        return true;
    }
    std::thread::sleep(Duration::from_millis(250));
    has_live_monitor(device_name)
}

fn ensure_inner(width: u32, height: u32, vsync: u32) -> u32 {
    let mut attached = 0u32;
    for i in 0..32u32 {
        let mut dd = DISPLAY_DEVICEW {
            cb: u32::try_from(size_of::<DISPLAY_DEVICEW>()).expect("struct fits in u32"),
            ..Default::default()
        };
        // SAFETY: `dd` is a correctly sized out-struct.
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &raw mut dd, 0) }.as_bool() {
            break;
        }
        if !utf16_z(&dd.DeviceString).contains(UDS_DISPLAY_MARKER) {
            continue;
        }
        if dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP == DISPLAY_DEVICE_ATTACHED_TO_DESKTOP {
            continue;
        }
        let name = utf16_z(&dd.DeviceName);
        // Stale GDI entries of past monitor incarnations ("ghosts") share
        // the marker but have no live monitor behind them; skip them fast
        // instead of spending attach retries on them.
        if !has_live_monitor_settled(&name) {
            info!(device = name, "skipping ghost UDS display (no live monitor)");
            continue;
        }
        match attach_with_retries(&name, width, height, vsync) {
            Ok(()) => attached += 1,
            Err(err) => warn!(error = %format!("{err:#}"), "cannot attach {name}"),
        }
    }
    attached
}

/// Attach attempts per candidate display (ghosts from past monitor
/// incarnations never attach, so this also bounds the time lost on them).
const ATTACH_ATTEMPTS: u32 = 6;

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
        cb: u32::try_from(size_of::<DISPLAY_DEVICEW>()).expect("struct fits in u32"),
        ..Default::default()
    };
    // SAFETY: `wide` is null-terminated; `dd` is a correctly sized
    // out-struct. iDevNum 0 is ignored when a device name is given.
    if !unsafe { EnumDisplayDevicesW(PCWSTR(wide.as_ptr()), 0, &raw mut dd, 0) }.as_bool() {
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
/// No explicit position (Windows places the display itself) and no
/// `CDS_NORESET`, so the (volatile, M6) change applies immediately.
/// A hand-built `DEVMODE` is rejected with `DISP_CHANGE_BADMODE` by drivers
/// whose mode list does not match it verbatim (IddCx), so the mode is picked
/// from the driver's own enumerated list first; the hand-built one is only
/// a fallback.
fn attach_display(device_name: &str, width: u32, height: u32, vsync: u32) -> anyhow::Result<()> {
    let wide: Vec<u16> = device_name.encode_utf16().chain(std::iter::once(0)).collect();
    let devmode = find_driver_mode(&wide, width, height, vsync).unwrap_or_else(|| DEVMODEW {
        dmSize: u16::try_from(size_of::<DEVMODEW>()).expect("struct fits in u16"),
        dmFields: DM_BITSPERPEL | DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY,
        dmBitsPerPel: 32,
        dmPelsWidth: width,
        dmPelsHeight: height,
        dmDisplayFrequency: vsync,
        ..Default::default()
    });
    // SAFETY: `wide` is null-terminated; `devmode` is fully initialized.
    // No CDS_NORESET: the change applies right away (and stays volatile).
    let result = unsafe {
        ChangeDisplaySettingsExW(
            PCWSTR(wide.as_ptr()),
            Some(&raw const devmode),
            None,
            CDS_TYPE(0),
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
            dmSize: u16::try_from(size_of::<DEVMODEW>()).expect("struct fits in u16"),
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
