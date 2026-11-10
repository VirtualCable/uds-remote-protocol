// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Secure-desktop probe (§18.41, M9 step 1): a hidden lab mode of
//! `server.exe` that validates the assumptions the secure-desktop helper
//! relies on before the real ScreenSource/InputSink pipe is built.
//!
//! Spawned by the service as SYSTEM inside the console session, it watches
//! the input desktop every 250 ms and logs every transition. When the active
//! desktop stops being "Default" (UAC consent, WinLogon, lock screen) it
//! exercises the two operations the helper needs:
//!
//!   1. capture the secure desktop with plain GDI (`SetThreadDesktop` +
//!      `BitBlt` + `GetDIBits`) into `<log-path>.bmp`;
//!   2. inject a no-op `SendInput` mouse move, to prove input injection
//!      reaches the secure desktop from a SYSTEM process in the session.
//!
//! Logging is a plain file (path from argv) so the probe works with no
//! tracing environment; the service redirects nothing. Exit code: 0 after a
//! successful secure-desktop capture, 2 on timeout, 1 on bad usage.
//!
//! Usage: `server --secure-probe <log-path>`

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPFILEHEADER, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, GetDeviceCaps,
    HDC, HGDIOBJ, HORZRES, ReleaseDC, SRCCOPY, SelectObject, VERTRES,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, HDESK, OpenInputDesktop, SetThreadDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW, WaitForSingleObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT, SendInput,
};
use windows::core::{PCWSTR, PWSTR};

/// Poll cadence for the input-desktop watch.
const POLL: Duration = Duration::from_millis(250);
/// Give up after this long without seeing a secure desktop.
const TIMEOUT: Duration = Duration::from_secs(120);

/// Hidden argv entry point: when `--secure-probe <log-path>` is present, run
/// the probe synchronously and return its exit code; `None` otherwise.
/// Checked before any other argument parsing (the probe takes no `--key`).
///
/// `--secure-probe-child <log-path>` is the inner mode: the probe re-spawns
/// itself with `lpDesktop = "winsta0\\Winlogon"` when a secure desktop is
/// active, to test whether `SendInput` works for a process BORN on the
/// secure desktop (it does not from `winsta0\\default`, even as SYSTEM).
pub fn maybe_run_from_argv() -> Option<ExitCode> {
    let args: Vec<String> = std::env::args().collect();
    let (flag, child) = if args.iter().any(|a| a == "--secure-probe-child") {
        ("--secure-probe-child", true)
    } else if args.iter().any(|a| a == "--secure-probe") {
        ("--secure-probe", false)
    } else {
        return None;
    };
    let pos = args.iter().position(|a| a == flag)?;
    let Some(log_path) = args.get(pos + 1).map(PathBuf::from) else {
        eprintln!("server: {flag} requires a log path");
        return Some(ExitCode::from(1));
    };
    Some(if child {
        run_child(&log_path)
    } else {
        run(&log_path)
    })
}

/// Appends one timestamped line to the probe log.
fn log_line(log: &mut std::fs::File, msg: &str) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let _ = writeln!(log, "[{secs}] {msg}");
}

/// The probe body. Never panics on Windows API failures: every error is
/// logged (that is exactly the data the lab run is for) and polling goes on.
fn run(log_path: &Path) -> ExitCode {
    let mut log = match std::fs::File::create(log_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("server: cannot create {}: {e}", log_path.display());
            return ExitCode::from(1);
        }
    };
    log_line(&mut log, "secure probe started");
    let start = Instant::now();
    let mut last_name = String::new();
    loop {
        if start.elapsed() >= TIMEOUT {
            log_line(&mut log, "PROBE TIMEOUT: no secure desktop observed");
            return ExitCode::from(2);
        }
        match poll_input_desktop() {
            Ok((hdesk, name)) => {
                if name != last_name {
                    log_line(&mut log, &format!("input desktop -> '{name}'"));
                    last_name = name.clone();
                }
                if name != "Default" {
                    log_line(
                        &mut log,
                        "secure desktop active; exercising capture + input",
                    );
                    exercise(&mut log, log_path, hdesk);
                    log_line(&mut log, "PROBE DONE");
                    return ExitCode::SUCCESS;
                }
                let _ = unsafe { CloseDesktop(hdesk) };
            }
            Err(e) => log_line(&mut log, &format!("OpenInputDesktop failed: {e}")),
        }
        std::thread::sleep(POLL);
    }
}

/// Opens the input desktop and reads its name (`Default`, `Winlogon`,
/// `Screen-saver`, ...).
fn poll_input_desktop() -> windows::core::Result<(HDESK, String)> {
    // READOBJECTS for the name; WRITEOBJECTS so SetThreadDesktop can adopt it.
    use windows::Win32::System::StationsAndDesktops::{
        DESKTOP_ACCESS_FLAGS, DESKTOP_READOBJECTS, DESKTOP_WRITEOBJECTS,
    };
    let access = DESKTOP_ACCESS_FLAGS(DESKTOP_READOBJECTS.0 | DESKTOP_WRITEOBJECTS.0);
    let hdesk = unsafe { OpenInputDesktop(Default::default(), false, access) }?;
    let mut buf = [0u16; 64];
    let mut needed = 0u32;
    let name = match unsafe {
        GetUserObjectInformationW(
            HANDLE(hdesk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
    } {
        Ok(()) => {
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..len])
        }
        Err(e) => format!("<name query failed: {e}>"),
    };
    Ok((hdesk, name))
}

/// Runs the two secure-desktop operations against `hdesk`: a GDI capture to
/// `<log-path>.bmp` and a no-op `SendInput`. `hdesk` stays open (the caller
/// exits right after).
fn exercise(log: &mut std::fs::File, log_path: &Path, hdesk: HDESK) {
    if let Err(e) = unsafe { SetThreadDesktop(hdesk) } {
        log_line(log, &format!("SetThreadDesktop failed: {e}"));
        return;
    }
    log_line(log, "SetThreadDesktop ok");
    match capture_bmp() {
        Ok((width, height, pixels)) => {
            let bmp_path = log_path.with_extension("bmp");
            match write_bmp(&bmp_path, width, height, &pixels) {
                Ok(()) => log_line(
                    log,
                    &format!("capture ok: {width}x{height} -> {}", bmp_path.display()),
                ),
                Err(e) => log_line(log, &format!("BMP write failed: {e}")),
            }
        }
        Err(e) => log_line(log, &format!("capture failed: {e}")),
    }
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    if sent == 1 {
        log_line(log, "SendInput no-op accepted");
    } else {
        let err = std::io::Error::last_os_error();
        log_line(log, &format!("SendInput blocked: {err}"));
    }
    // SendInput from a process ANCHORED to winsta0\default is blocked on the
    // secure desktop even for SYSTEM; test the known alternative: a child
    // process born directly on winsta0\Winlogon (the VNC helper pattern).
    spawn_child_on_secure_desktop(log, log_path);
}

/// Re-spawns this executable in `--secure-probe-child` mode directly on the
/// secure desktop and waits for it (bounded).
fn spawn_child_on_secure_desktop(log: &mut std::fs::File, log_path: &Path) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            log_line(log, &format!("child spawn: current_exe failed: {e}"));
            return;
        }
    };
    let mut cmdline: Vec<u16> = format!(
        "\"{}\" --secure-probe-child \"{}\"",
        exe.display(),
        log_path.display()
    )
    .encode_utf16()
    .chain(std::iter::once(0))
    .collect();
    let mut desktop: Vec<u16> = "winsta0\\Winlogon"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).expect("struct fits in u32"),
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: `cmdline`/`desktop` are null-terminated, mutable and outlive
    // the call; `startup`/`info` are correctly sized. The token is inherited
    // (the probe already runs as SYSTEM in the console session).
    let created = unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &raw const startup,
            &raw mut info,
        )
    };
    match created {
        Ok(()) => {
            log_line(
                log,
                &format!(
                    "child spawned on winsta0\\Winlogon pid={}",
                    info.dwProcessId
                ),
            );
            // SAFETY: valid process handle; bounded wait.
            let waited = unsafe { WaitForSingleObject(info.hProcess, 15_000) };
            log_line(log, &format!("child wait result: {waited:?}"));
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(info.hThread);
                let _ = windows::Win32::Foundation::CloseHandle(info.hProcess);
            }
        }
        Err(e) => log_line(log, &format!("child spawn failed: {e}")),
    }
}

/// Child mode: born on the secure desktop (`lpDesktop = "winsta0\\Winlogon"`
/// at creation), appends its input-injection result to the probe log.
fn run_child(log_path: &Path) -> ExitCode {
    let mut log = match std::fs::OpenOptions::new().append(true).open(log_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("probe child: cannot open {}: {e}", log_path.display());
            return ExitCode::from(1);
        }
    };
    log_line(&mut log, "[child] started on the secure desktop");
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    if sent == 1 {
        log_line(&mut log, "[child] SendInput ACCEPTED on the secure desktop");
    } else {
        let err = std::io::Error::last_os_error();
        log_line(&mut log, &format!("[child] SendInput blocked too: {err}"));
    }
    ExitCode::SUCCESS
}

/// Captures the calling thread's desktop via GDI into 32-bpp BGRA pixels
/// (top-down rows). Fails on the secure desktop if GDI denies it — the very
/// question this probe answers.
fn capture_bmp() -> windows::core::Result<(u32, u32, Vec<u8>)> {
    let screen = unsafe { GetDC(None) };
    if screen.is_invalid() {
        return Err(std::io::Error::last_os_error().into());
    }
    let result = capture_from_dc(screen);
    unsafe { ReleaseDC(None, screen) };
    let (width, height, pixels) = result?;
    Ok((width as u32, height as u32, pixels))
}

fn capture_from_dc(screen: HDC) -> windows::core::Result<(i32, i32, Vec<u8>)> {
    let width = unsafe { GetDeviceCaps(Some(screen), HORZRES) };
    let height = unsafe { GetDeviceCaps(Some(screen), VERTRES) };
    if width <= 0 || height <= 0 {
        return Err(std::io::Error::other("GetDeviceCaps returned no geometry").into());
    }
    let mem = unsafe { CreateCompatibleDC(Some(screen)) };
    let bmp = unsafe { CreateCompatibleBitmap(screen, width, height) };
    let old = unsafe { SelectObject(mem, HGDIOBJ(bmp.0)) };
    unsafe { BitBlt(mem, 0, 0, width, height, Some(screen), 0, 0, SRCCOPY) }?;
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
    let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
    let lines = unsafe {
        GetDIBits(
            mem,
            bmp,
            0,
            height as u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    unsafe {
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
    }
    if lines != height {
        return Err(
            std::io::Error::other(format!("GetDIBits copied {lines}/{height} lines")).into(),
        );
    }
    Ok((width, height, pixels))
}

/// Writes a minimal 32-bpp BMP (BITMAPFILEHEADER + BITMAPINFOHEADER +
/// top-down BGRA pixels).
fn write_bmp(path: &Path, width: u32, height: u32, pixels: &[u8]) -> std::io::Result<()> {
    let pixel_bytes = u32::try_from(pixels.len()).map_err(std::io::Error::other)?;
    let header = BITMAPFILEHEADER {
        bfType: 0x4D42, // "BM"
        bfSize: 54 + pixel_bytes,
        bfOffBits: 54,
        ..Default::default()
    };
    let info = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width as i32,
        biHeight: -(height as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: pixel_bytes,
        ..Default::default()
    };
    let mut file = std::fs::File::create(path)?;
    file.write_all(as_bytes(&header))?;
    file.write_all(as_bytes(&info))?;
    file.write_all(pixels)
}

/// Reinterprets a plain-old-data header as bytes (both BMP headers are
/// packed C structs with no padding-sensitive tail).
fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(std::ptr::from_ref(value).cast(), size_of::<T>()) }
}
