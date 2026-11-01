//! Manual probe for the `reposition rejected: -4` (DISP_CHANGE_BADFLAGS)
//! seen by `display_topology::stack_uds_displays_below` (doc/plan
//! estudio-inicial.md §18.29).
//!
//! Run in the interactive console session (topology changes do not stick
//! from session 0):
//!
//! ```text
//! cargo test -p server --features windows-capture --test reposition_probe -- --ignored --nocapture
//! ```
//!
//! Tries several `ChangeDisplaySettingsExW` strategies to move a display
//! and reports the `DISP_CHANGE_*` code of each, on both the UDS virtual
//! display and the primary one (to tell IddCx-specific rejections from a
//! broken reposition path in general).

#![cfg(all(windows, feature = "windows-capture"))]
#![allow(clippy::missing_docs_in_private_items)]

use windows::Win32::Foundation::POINTL;
use windows::Win32::Graphics::Gdi::{
    CDS_NORESET, CDS_TYPE, CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW,
    DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, DM_POSITION, DISP_CHANGE,
    ENUM_CURRENT_SETTINGS, EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::core::PCWSTR;

const DISP_CHANGE_OK: DISP_CHANGE = DISP_CHANGE(0);

fn utf16_z(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn attached_displays() -> Vec<String> {
    let mut out = Vec::new();
    for i in 0..32u32 {
        let mut dd = DISPLAY_DEVICEW {
            cb: u32::try_from(size_of::<DISPLAY_DEVICEW>()).unwrap(),
            ..Default::default()
        };
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &raw mut dd, 0) }.as_bool() {
            break;
        }
        if dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP
            == DISPLAY_DEVICE_ATTACHED_TO_DESKTOP
        {
            out.push(utf16_z(&dd.DeviceName));
        }
    }
    out
}

fn current_devmode(name: &str) -> Option<DEVMODEW> {
    let w = wide(name);
    let mut dm = DEVMODEW {
        dmSize: u16::try_from(size_of::<DEVMODEW>()).unwrap(),
        ..Default::default()
    };
    unsafe { EnumDisplaySettingsW(PCWSTR(w.as_ptr()), ENUM_CURRENT_SETTINGS, &raw mut dm) }
        .as_bool()
        .then_some(dm)
}

fn pos(dm: &DEVMODEW) -> POINTL {
    unsafe { dm.Anonymous1.Anonymous2.dmPosition }
}

fn cds(name: &str, dm: Option<&DEVMODEW>, flags: CDS_TYPE) -> i32 {
    let w = wide(name);
    let ptr = dm.map_or(std::ptr::null(), |d| d as *const DEVMODEW);
    unsafe { ChangeDisplaySettingsExW(PCWSTR(w.as_ptr()), ptr, None, flags, None) }.0
}

fn code(rc: DISP_CHANGE) -> String {
    let n = rc.0;
    let label = match n {
        0 => "OK",
        1 => "RESTART",
        -1 => "FAILED",
        -2 => "BADMODE",
        -3 => "NOTUPDATED",
        -4 => "BADFLAGS",
        -5 => "BADPARAM",
        -6 => "BADDUALVIEW",
        _ => "?",
    };
    format!("{n} ({label})")
}

/// Position-only devmode: keep the current mode, expose only DM_POSITION.
fn position_only(dm: &DEVMODEW, x: i32, y: i32) -> DEVMODEW {
    let mut out = *dm;
    out.dmFields = DM_POSITION;
    out.Anonymous1.Anonymous2.dmPosition = POINTL { x, y };
    out
}

#[test]
#[ignore = "manual probe: run in the interactive console session"]
fn reposition_strategies() {
    let displays = attached_displays();
    println!("attached displays: {displays:?}");
    assert!(displays.len() >= 2, "need the UDS display attached next to another one");

    for name in &displays {
        let Some(dm) = current_devmode(name) else { continue };
        let p = pos(&dm);
        println!("\n== {name} current pos=({},{}) fields=0x{:X}", p.x, p.y, dm.dmFields.0);

        // Strategy A: what stack_uds_displays_below does today — stage with
        // CDS_NORESET keeping the full field set, then apply with null/null/0.
        let mut staged = dm;
        staged.dmFields |= DM_POSITION;
        staged.Anonymous1.Anonymous2.dmPosition = p; // same position: harmless
        let a1 = cds(name, Some(&staged), CDS_NORESET);
        let a2 = unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None) }.0;
        println!("A stage-full+noreset: {} | apply: {}", code(DISP_CHANGE(a1)), code(DISP_CHANGE(a2)));

        // Strategy B: direct single call, position-only fields, no NORESET.
        let b = cds(name, Some(&position_only(&dm, p.x, p.y)), CDS_TYPE(0));
        println!("B direct position-only: {}", code(DISP_CHANGE(b)));

        // Strategy C: stage position-only with NORESET, apply null/null/0.
        let c1 = cds(name, Some(&position_only(&dm, p.x, p.y)), CDS_NORESET);
        let c2 = unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None) }.0;
        println!("C stage-posonly+noreset: {} | apply: {}", code(DISP_CHANGE(c1)), code(DISP_CHANGE(c2)));

        // Strategy D: stage position-only with NORESET|UPDATEREGISTRY, apply.
        let d1 = cds(name, Some(&position_only(&dm, p.x, p.y)), CDS_NORESET | CDS_UPDATEREGISTRY);
        let d2 = unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None) }.0;
        println!("D stage-posonly+noreset+updatereg: {} | apply: {}", code(DISP_CHANGE(d1)), code(DISP_CHANGE(d2)));

        // Restore the original position directly (in case anything moved).
        let r = cds(name, Some(&position_only(&dm, p.x, p.y)), CDS_TYPE(0));
        println!("restore direct: {}", code(DISP_CHANGE(r)));
    }
}
