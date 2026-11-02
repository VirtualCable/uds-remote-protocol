// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Secure-desktop helper mode (§18.41, M9): `server.exe --secure-helper
//! <session-id>`, spawned by the service as SYSTEM **born on the Winlogon
//! desktop** (`lpDesktop = "winsta0\\Winlogon"` — lab-validated: only a
//! process born on the secure desktop can `SendInput` into it).
//!
//! Loop: GDI-`BitBlt` the secure desktop (~10 fps, raw BGRA) down the
//! frames pipe to the agent, and inject the sanitized input events the agent
//! forwards on the input pipe. Exits when the input desktop returns to
//! `Default` or either pipe breaks; the service also kills it on the
//! transition back.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::warn;

use crate::secure_pipe;

/// Capture cadence of the helper (secure desktop is an emergency mode).
const FRAME_INTERVAL: Duration = Duration::from_millis(100);
/// How long the helper waits for the agent's pipe server at startup.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// True while the console input desktop is a secure one (Winlogon,
/// Screen-saver). From a USER token `OpenInputDesktop` on the secure desktop
/// fails with access denied; from SYSTEM it succeeds with a name other than
/// `Default` — both mean "secure desktop active".
#[cfg(all(windows, feature = "windows-capture"))]
pub fn secure_desktop_active() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_READOBJECTS, GetUserObjectInformationW, OpenInputDesktop, UOI_NAME,
    };
    let hdesk = match unsafe { OpenInputDesktop(Default::default(), false, DESKTOP_READOBJECTS) } {
        Ok(h) => h,
        // Access denied (user token + secure desktop) or any transient error:
        // treat as secure — the failure mode that matters (UAC up) must not
        // leave the agent capturing a black screen silently.
        Err(_) => return true,
    };
    let mut buf = [0u16; 64];
    let name = unsafe {
        GetUserObjectInformationW(
            HANDLE(hdesk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * 2) as u32,
            None,
        )
    }
    .map(|()| {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    })
    .unwrap_or_default();
    unsafe {
        let _ = CloseDesktop(hdesk);
    }
    name != "Default"
}

/// Captures the calling process's desktop via GDI into 32-bpp BGRA pixels
/// (top-down rows). Used by the secure helper (born on the Winlogon desktop)
/// and by the lab probe (after `SetThreadDesktop`).
#[cfg(all(windows, feature = "windows-capture"))]
pub fn gdi_capture_frame() -> Result<(u32, u32, Vec<u8>)> {
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, GetDeviceCaps, HDC, HGDIOBJ,
        HORZRES, ReleaseDC, SRCCOPY, SelectObject, VERTRES,
    };
    let screen = unsafe { GetDC(None) };
    if screen.is_invalid() {
        anyhow::bail!("GetDC: {}", std::io::Error::last_os_error());
    }
    let result = (|| -> Result<(i32, i32, Vec<u8>)> {
        let width = unsafe { GetDeviceCaps(Some(screen), HORZRES) };
        let height = unsafe { GetDeviceCaps(Some(screen), VERTRES) };
        anyhow::ensure!(width > 0 && height > 0, "GetDeviceCaps returned no geometry");
        let mem: HDC = unsafe { CreateCompatibleDC(Some(screen)) };
        let bmp = unsafe { CreateCompatibleBitmap(screen, width, height) };
        let old = unsafe { SelectObject(mem, HGDIOBJ(bmp.0)) };
        let blt = unsafe { BitBlt(mem, 0, 0, width, height, Some(screen), 0, 0, SRCCOPY) };
        let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
        let lines = if blt.is_ok() {
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height, // top-down rows
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            unsafe {
                GetDIBits(
                    mem,
                    bmp,
                    0,
                    height as u32,
                    Some(pixels.as_mut_ptr().cast()),
                    &mut info,
                    DIB_RGB_COLORS,
                )
            }
        } else {
            0
        };
        unsafe {
            SelectObject(mem, old);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(mem);
        }
        blt.context("BitBlt")?;
        anyhow::ensure!(lines == height, "GetDIBits copied {lines}/{height} lines");
        Ok((width, height, pixels))
    })();
    unsafe {
        ReleaseDC(None, screen);
    }
    let (w, h, pixels) = result?;
    Ok((w as u32, h as u32, pixels))
}

/// Hidden argv entry point: `--secure-helper <session-id>` runs the helper
/// synchronously and returns its exit code; `None` otherwise.
#[cfg(all(windows, feature = "windows-capture"))]
pub fn maybe_run_from_argv() -> Option<ExitCode> {
    let args: Vec<String> = std::env::args().collect();
    let pos = args.iter().position(|a| a == "--secure-helper")?;
    let Some(session_id) = args.get(pos + 1).and_then(|v| v.parse::<u32>().ok()) else {
        eprintln!("server: --secure-helper requires a numeric session id");
        return Some(ExitCode::from(1));
    };
    Some(match run_helper(session_id) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("secure helper: {e:#}");
            ExitCode::FAILURE
        }
    })
}

/// Helper body: connect both pipes, pump sanitized input on a thread, and
/// capture the secure desktop until it goes away.
#[cfg(all(windows, feature = "windows-capture"))]
fn run_helper(session_id: u32) -> Result<()> {
    use crate::input::SendInputInjector;
    use crate::secure_sanitize::Sanitizer;
    let mut frames = secure_pipe::win::connect_pipe(
        &secure_pipe::frames_pipe_name(session_id),
        CONNECT_TIMEOUT,
        true,
    )
    .context("connecting the frames pipe")?;
    let mut input_pipe = secure_pipe::win::connect_pipe(
        &secure_pipe::input_pipe_name(session_id),
        CONNECT_TIMEOUT,
        false,
    )
    .context("connecting the input pipe")?;

    // Input pump: one thread, blocking reads; the process exits with the
    // main loop, so no join is needed.
    std::thread::Builder::new()
        .name("secure-input".into())
        .spawn(move || {
            let mut sanitizer = Sanitizer::new(SendInputInjector::new());
            loop {
                match secure_pipe::read_msg(&mut input_pipe) {
                    Ok(Some((secure_pipe::TAG_INPUT, payload))) => {
                        match protocol::InputMessage::decode(&payload) {
                            Ok(msg) => sanitizer.dispatch(msg),
                            Err(e) => warn!("secure helper: bad input message: {e}"),
                        }
                    }
                    Ok(Some((tag, _))) => warn!(tag, "secure helper: unexpected input-pipe tag"),
                    Ok(None) | Err(_) => return, // pipe closed: helper exits
                }
            }
        })
        .context("spawning the input pump")?;

    // Capture loop: frames while a secure desktop owns the console.
    while secure_desktop_active() {
        match gdi_capture_frame() {
            Ok((w, h, pixels)) => {
                let payload = secure_pipe::encode_frame(w, h, &pixels);
                if secure_pipe::write_msg(&mut frames, secure_pipe::TAG_FRAME, &payload).is_err() {
                    break; // agent gone
                }
            }
            Err(e) => {
                warn!("secure helper: capture failed: {e:#}");
                std::thread::sleep(FRAME_INTERVAL * 5);
            }
        }
        std::thread::sleep(FRAME_INTERVAL);
    }
    Ok(())
}
