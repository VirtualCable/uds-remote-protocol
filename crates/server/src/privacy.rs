//! Privacy screen (M7): branded fullscreen overlay over the physical
//! displays while a remote session is active.
//!
//! When the client negotiates `FLAG_PRIVACY_SCREEN`, the server paints one
//! borderless, always-on-top window per *physical* display (the IddCx
//! virtual monitor is left untouched for the remote user). The overlay is a
//! black background with the UDS logo and a lock notice; the physical
//! outputs stay attached, so restoring the console is simply closing the
//! windows.
//!
//! The overlay windows live in this process (dedicated UI thread): if the
//! server dies, the OS destroys its windows and the console is restored
//! immediately. The named mutex [`PRIVACY_MUTEX_NAME`] is held while the
//! screen is active; it arbitrates double activation and documents the
//! protocol a future supervisor process can watch (`WAIT_ABANDONED` means
//! the holder died).
//!
//! The pure part (monitor model and target selection) is platform-free and
//! unit-tested on every OS; the Win32 implementation is compiled only on
//! Windows with the `windows-capture` feature.

/// One display device as enumerated by the OS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    /// Device name, e.g. `\\.\DISPLAY1`.
    pub name: String,
    /// Human-readable device string, e.g. "UDS Virtual Display".
    pub device_string: String,
    /// Desktop coordinates of the display (meaningful when `attached`).
    pub left: i32,
    /// Top edge of the display in desktop coordinates.
    pub top: i32,
    /// Right edge of the display in desktop coordinates.
    pub right: i32,
    /// Bottom edge of the display in desktop coordinates.
    pub bottom: i32,
    /// Whether the display is attached to the desktop.
    pub attached: bool,
}

/// Substring identifying the IddCx virtual display (its device string).
pub const VIRTUAL_DISPLAY_MARKER: &str = "UDS Virtual Display";

/// Named mutex held while the privacy screen is active. `Local\` scope: the
/// screen is per-session, and a supervisor in session 0 can observe it.
pub const PRIVACY_MUTEX_NAME: &str = r"Local\UDSRemotePrivacyScreen";

/// Text shown on the privacy overlay under the logo.
pub const PRIVACY_TEXT: &str = "Locked — Remote Access in progress";

/// Rectangles (desktop coordinates) that the overlay windows must cover:
/// every attached display that is not the IddCx virtual one.
#[must_use]
pub fn overlay_targets(monitors: &[MonitorInfo]) -> Vec<(i32, i32, i32, i32)> {
    monitors
        .iter()
        .filter(|m| m.attached && !m.device_string.contains(VIRTUAL_DISPLAY_MARKER))
        .map(|m| (m.left, m.top, m.right, m.bottom))
        .collect()
}

#[cfg(all(windows, feature = "windows-capture"))]
mod imp {
    // The `windows` crate FFI methods take raw pointers; implicit borrows are
    // the idiomatic call style for that API (same convention as `dxgi`).
    #![allow(clippy::borrow_as_ptr)]

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, OnceLock};
    use std::thread::JoinHandle;

    use anyhow::{Context, Result, bail};
    use tracing::{info, warn};
    use windows::Win32::Foundation::{
        CloseHandle, COLORREF, ERROR_ALREADY_EXISTS, ERROR_CLASS_ALREADY_EXISTS, GetLastError,
        HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
    };
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLACK_BRUSH, BeginPaint, CLEARTYPE_QUALITY,
        CLIP_DEFAULT_PRECIS, CreateFontW, DEFAULT_CHARSET, DIB_RGB_COLORS, DISPLAY_DEVICE_STATE_FLAGS,
        DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, DEVMODEW, DT_CENTER, DT_SINGLELINE,
        DT_VCENTER, DeleteObject, DrawTextW, ENUM_CURRENT_SETTINGS, EndPaint, EnumDisplayDevicesW,
        EnumDisplaySettingsW, FW_BOLD, FillRect, GetStockObject, HBRUSH, HGDIOBJ, OUT_DEFAULT_PRECIS,
        PAINTSTRUCT, SRCCOPY, SelectObject, SetBkMode, SetTextColor, StretchDIBits, TRANSPARENT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
        GetMessageW, KillTimer, MSG, RegisterClassW, SW_SHOWNA, SetTimer,
        ShowWindow, TranslateMessage, UnregisterClassW, WM_ERASEBKGND, WM_PAINT, WM_TIMER,
        WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };
    use windows::core::{PCWSTR, w};

    use super::{MonitorInfo, PRIVACY_MUTEX_NAME, PRIVACY_TEXT, overlay_targets};

    /// Timer that wakes the UI thread to check the stop flag.
    const STOP_TIMER_ID: usize = 1;
    /// Stop-flag polling cadence; also the worst-case teardown latency.
    const STOP_POLL_MS: u32 = 500;
    /// Margin between the logo and the notice text, in pixels.
    const TEXT_MARGIN: i32 = 32;

    /// UDS logo decoded from the embedded PNG, pre-composed over black as
    /// 32bpp BGRX (the overlay background is always black, so alpha blending
    /// at paint time is unnecessary).
    struct Logo {
        width: i32,
        height: i32,
        bgrx: Vec<u8>,
    }

    static LOGO: OnceLock<Option<Logo>> = OnceLock::new();

    /// Decodes the logo once per process; `None` if the PNG fails to decode
    /// (the overlay then shows only the notice text over black).
    fn logo() -> Option<&'static Logo> {
        LOGO
            .get_or_init(|| {
                let png = include_bytes!("../assets/uds-logo-512.png");
                let img = match image::load_from_memory(png) {
                    Ok(img) => img.to_rgba8(),
                    Err(e) => {
                        warn!("privacy screen: cannot decode embedded logo: {e}");
                        return None;
                    }
                };
                let (w, h) = img.dimensions();
                let mut bgrx = Vec::with_capacity((w * h * 4) as usize);
                for px in img.pixels() {
                    let [red, green, blue, alpha] = px.0;
                    let blend = |channel: u8| {
                        u8::try_from(
                            (u32::from(channel) * u32::from(alpha) + 127) / 255,
                        )
                        .expect("a premultiplied channel fits in u8")
                    };
                    bgrx.extend_from_slice(&[blend(blue), blend(green), blend(red), 0]);
                }
                Some(Logo {
                    width: i32::try_from(w).ok()?,
                    height: i32::try_from(h).ok()?,
                    bgrx,
                })
            })
            .as_ref()
    }

    /// RAII handle of an active privacy screen: dropping it closes the
    /// overlay windows and releases the mutex. A process crash achieves the
    /// same through the OS (windows belong to the process; the mutex becomes
    /// abandoned).
    pub struct PrivacyGuard {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
        /// Raw `HANDLE` value (`HANDLE` is not `Send`; the guard crosses
        /// `.await` points in the session loop).
        mutex: usize,
    }

    impl Drop for PrivacyGuard {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            if self.mutex != 0 {
                unsafe {
                    let _ = CloseHandle(windows::Win32::Foundation::HANDLE(
                        self.mutex as *mut core::ffi::c_void,
                    ));
                }
            }
        }
    }

    // Raw handles wrapped in `usize` keep the guard `Send`; the mutex handle
    // is only ever used on drop, from whichever thread drops the guard.

    /// Raises the privacy screen: grabs the named mutex and covers every
    /// physical display with a branded overlay window until the returned
    /// guard is dropped.
    pub fn activate() -> Result<PrivacyGuard> {
        unsafe {
            // Defensive: the server may run embedded in a process whose DPI
            // awareness is not per-monitor; the overlay must cover exact
            // physical pixels.
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

            let name: Vec<u16> = PRIVACY_MUTEX_NAME
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mutex = CreateMutexW(None, true, PCWSTR(name.as_ptr()))
                .context("CreateMutexW failed")?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(mutex);
                bail!("privacy screen already active ({PRIVACY_MUTEX_NAME} held)");
            }

            let monitors = enumerate_monitors();
            let rects = overlay_targets(&monitors);
            let stop = Arc::new(AtomicBool::new(false));
            let mut guard = PrivacyGuard {
                stop: Arc::clone(&stop),
                thread: None,
                mutex: mutex.0 as usize,
            };
            if rects.is_empty() {
                warn!(
                    displays = monitors.len(),
                    "privacy screen: no physical display to cover"
                );
                return Ok(guard);
            }
            info!(windows = rects.len(), "privacy screen: covering physical displays");

            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let thread = match std::thread::Builder::new()
                .name("privacy-ui".into())
                .spawn(move || ui_thread_main(&rects, &stop, &ready_tx))
            {
                Ok(thread) => thread,
                Err(e) => {
                    let _ = CloseHandle(mutex);
                    return Err(e).context("spawning the privacy UI thread");
                }
            };
            match ready_rx.recv() {
                Ok(Ok(())) => {
                    guard.thread = Some(thread);
                    Ok(guard)
                }
                Ok(Err(e)) => {
                    let _ = thread.join();
                    let _ = CloseHandle(mutex);
                    Err(e)
                }
                Err(_) => {
                    let _ = thread.join();
                    let _ = CloseHandle(mutex);
                    bail!("privacy UI thread ended during startup");
                }
            }
        }
    }

    /// Enumerates the display devices with their desktop rectangles.
    fn enumerate_monitors() -> Vec<MonitorInfo> {
        let mut out = Vec::new();
        unsafe {
            for i in 0u32.. {
                let mut dd = DISPLAY_DEVICEW {
                    cb: u32::try_from(size_of::<DISPLAY_DEVICEW>())
                        .expect("DISPLAY_DEVICEW fits in u32"),
                    ..Default::default()
                };
                if !EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0).as_bool() {
                    break;
                }
                let attached =
                    dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP != DISPLAY_DEVICE_STATE_FLAGS(0);
                let name = wide_to_string(&dd.DeviceName);
                let device_string = wide_to_string(&dd.DeviceString);
                let mut rect = (0, 0, 0, 0);
                if attached {
                    let device: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
                    let mut dm = DEVMODEW {
                        dmSize: u16::try_from(size_of::<DEVMODEW>())
                            .expect("DEVMODEW fits in u16"),
                        ..Default::default()
                    };
                    if EnumDisplaySettingsW(PCWSTR(device.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm)
                        .as_bool()
                    {
                        let pos = dm.Anonymous1.Anonymous2.dmPosition;
                        rect = (
                            pos.x,
                            pos.y,
                            pos.x + i32::try_from(dm.dmPelsWidth).unwrap_or(i32::MAX),
                            pos.y + i32::try_from(dm.dmPelsHeight).unwrap_or(i32::MAX),
                        );
                    }
                }
                out.push(MonitorInfo {
                    name,
                    device_string,
                    left: rect.0,
                    top: rect.1,
                    right: rect.2,
                    bottom: rect.3,
                    attached,
                });
            }
        }
        out
    }

    /// Converts a fixed-size UTF-16 field to a Rust string.
    fn wide_to_string(field: &[u16]) -> String {
        let end = field.iter().position(|&c| c == 0).unwrap_or(field.len());
        String::from_utf16_lossy(&field[..end])
    }

    /// UI thread: registers the window class, creates one overlay window per
    /// target rectangle and runs the message loop until the stop flag is set
    /// (checked on each timer tick) or the loop is broken by `WM_QUIT`.
    fn ui_thread_main(
        rects: &[(i32, i32, i32, i32)],
        stop: &AtomicBool,
        ready: &std::sync::mpsc::Sender<Result<()>>,
    ) {
        unsafe {
            let hinstance = HINSTANCE(GetModuleHandleW(None).map_or(std::ptr::null_mut(), |m| m.0));
            let class_name = w!("UDSRemotePrivacyOverlay");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                hInstance: hinstance,
                hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
                lpszClassName: class_name,
                ..Default::default()
            };
            if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                ready
                    .send(Err(anyhow::anyhow!(
                        "RegisterClassW failed: {}",
                        windows::core::Error::from_thread()
                    )))
                    .ok();
                return;
            }

            let mut windows: Vec<HWND> = Vec::with_capacity(rects.len());
            for &(left, top, right, bottom) in rects {
                match CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    class_name,
                    w!("UDS Privacy Screen"),
                    WS_POPUP,
                    left,
                    top,
                    right - left,
                    bottom - top,
                    None,
                    None,
                    Some(hinstance),
                    None,
                ) {
                    Ok(hwnd) => {
                        let _ = ShowWindow(hwnd, SW_SHOWNA);
                        windows.push(hwnd);
                    }
                    Err(e) => {
                        for hwnd in &windows {
                            let _ = DestroyWindow(*hwnd);
                        }
                        let _ = UnregisterClassW(class_name, Some(hinstance));
                        ready
                            .send(Err(e).context("CreateWindowExW failed for a privacy overlay"))
                            .ok();
                        return;
                    }
                }
            }
            ready.send(Ok(())).ok();

            SetTimer(None, STOP_TIMER_ID, STOP_POLL_MS, None);
            let mut msg = MSG::default();
            loop {
                // GetMessageW returns 0 on WM_QUIT and -1 on error.
                let ret = GetMessageW(&mut msg, None, 0, 0);
                if ret.0 <= 0 {
                    break;
                }
                if msg.message == WM_TIMER && stop.load(Ordering::Relaxed) {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let _ = KillTimer(None, STOP_TIMER_ID);
            for hwnd in &windows {
                let _ = DestroyWindow(*hwnd);
            }
            let _ = UnregisterClassW(class_name, Some(hinstance));
        }
    }

    /// Window procedure: paints the branded overlay, erases nothing outside
    /// `WM_PAINT` (avoids flicker) and never takes input focus.
    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            match msg {
                WM_PAINT => {
                    paint_overlay(hwnd);
                    LRESULT(0)
                }
                WM_ERASEBKGND => LRESULT(1),
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    /// Paints one overlay window: black background, logo centered (scaled
    /// down if the display is smaller) and the notice text below it.
    // GDI coordinate math mixes window sizes and font metrics; values are
    // display-sized, so the casts cannot overflow in practice.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    unsafe fn paint_overlay(hwnd: HWND) {
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            if hdc.is_invalid() {
                return;
            }
            let mut rc = RECT::default();
            if GetClientRect(hwnd, &mut rc).is_err() {
                let _ = EndPaint(hwnd, &ps);
                return;
            }
            let width = rc.right - rc.left;
            let height = rc.bottom - rc.top;
            FillRect(hdc, &rc, HBRUSH(GetStockObject(BLACK_BRUSH).0));

            let mut text_top = rc.top + height / 2 + TEXT_MARGIN;
            if let Some(logo) = logo() {
                // Fit the logo into 60% of the display when it is smaller
                // than the source bitmap.
                let max_w = width * 3 / 5;
                let max_h = height * 3 / 5;
                let scale = f64::from(max_w) / f64::from(logo.width);
                let scale = scale.min(f64::from(max_h) / f64::from(logo.height)).min(1.0);
                let dw = (f64::from(logo.width) * scale) as i32;
                let dh = (f64::from(logo.height) * scale) as i32;
                let x = rc.left + (width - dw) / 2;
                let y = rc.top + (height - dh) / 2 - dh / 8;
                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: logo.width,
                        biHeight: -logo.height, // top-down
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                StretchDIBits(
                    hdc,
                    x,
                    y,
                    dw,
                    dh,
                    0,
                    0,
                    logo.width,
                    logo.height,
                    Some(logo.bgrx.as_ptr().cast()),
                    &bmi,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                );
                text_top = y + dh + TEXT_MARGIN;
            }

            let line = (height / 24).clamp(24, 64);
            let font = CreateFontW(
                -line,
                0,
                0,
                0,
                FW_BOLD.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                0,
                w!("Segoe UI"),
            );
            if !font.is_invalid() {
                let prev = SelectObject(hdc, HGDIOBJ(font.0));
                SetTextColor(hdc, COLORREF(0x00FF_FFFF));
                SetBkMode(hdc, TRANSPARENT);
                let mut text: Vec<u16> = PRIVACY_TEXT.encode_utf16().collect();
                let mut text_rc = RECT {
                    left: rc.left,
                    top: text_top.min(rc.bottom - line),
                    right: rc.right,
                    bottom: rc.bottom,
                };
                DrawTextW(hdc, &mut text, &mut text_rc, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
                SelectObject(hdc, prev);
                let _ = DeleteObject(HGDIOBJ(font.0));
            }
            let _ = EndPaint(hwnd, &ps);
        }
    }
}

#[cfg(all(windows, feature = "windows-capture"))]
pub use imp::{PrivacyGuard, activate};

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: &str, device_string: &str, rect: (i32, i32, i32, i32), attached: bool) -> MonitorInfo {
        MonitorInfo {
            name: name.to_string(),
            device_string: device_string.to_string(),
            left: rect.0,
            top: rect.1,
            right: rect.2,
            bottom: rect.3,
            attached,
        }
    }

    #[test]
    fn overlay_covers_attached_physical_displays_only() {
        let monitors = vec![
            monitor("\\\\.\\DISPLAY1", "QXL GPU", (0, 0, 1920, 1080), true),
            monitor("\\\\.\\DISPLAY2", "UDS Virtual Display", (1920, 0, 3840, 1080), true),
            monitor("\\\\.\\DISPLAY3", "Generic PnP Monitor", (-1920, 0, 0, 1080), false),
        ];
        assert_eq!(overlay_targets(&monitors), vec![(0, 0, 1920, 1080)]);
    }

    #[test]
    fn overlay_supports_multiple_physical_displays_with_negative_origins() {
        let monitors = vec![
            monitor("\\\\.\\DISPLAY1", "A", (-1920, -100, 0, 980), true),
            monitor("\\\\.\\DISPLAY2", "B", (0, 0, 2560, 1440), true),
        ];
        assert_eq!(
            overlay_targets(&monitors),
            vec![(-1920, -100, 0, 980), (0, 0, 2560, 1440)]
        );
    }

    #[test]
    fn overlay_targets_is_empty_without_displays() {
        assert_eq!(overlay_targets(&[]), Vec::new());
        let only_virtual = vec![monitor(
            "\\\\.\\DISPLAY9",
            "UDS Virtual Display",
            (0, 0, 1920, 1080),
            true,
        )];
        assert_eq!(overlay_targets(&only_virtual), Vec::new());
    }

    #[test]
    fn mutex_name_is_local_scoped() {
        assert!(PRIVACY_MUTEX_NAME.starts_with("Local\\"));
    }
}
