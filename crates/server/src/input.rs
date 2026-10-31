//! Input injection (design doc §10): turns [`InputMessage`]s into OS-level
//! input.
//!
//! The trait is the platform-neutral contract; `SendInputInjector` (Windows,
//! feature `windows-capture`) is the real backend and `LogInjector` the
//! development fallback (counts events, logs them) so the channel can be
//! exercised end to end without a Windows session.
//!
//! Windows mapping notes:
//! - Keyboard: `SendInput` with `KEYEVENTF_SCANCODE` (+ `KEYEVENTF_EXTENDEDKEY`
//!   when the message carries the `0xE0` flag): Set 1 scancodes straight
//!   through, layouts stay a client-side concern.
//! - Pointer: coordinates arrive display-local; the injector translates them
//!   to virtual-desktop pixels via the monitor layout and sends normalized
//!   absolute moves (`MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`).

use protocol::{InputMessage, PointerButton};

/// One input-event sink: platform-neutral injection contract.
pub trait InputInjector {
    /// Key press/release (Set 1 scancode + extended flag).
    fn key(&mut self, down: bool, scancode: u16, extended: bool);
    /// Absolute pointer move, display-local pixels.
    fn pointer_move(&mut self, display_id: u8, x: i32, y: i32);
    /// Pointer button press/release.
    fn pointer_button(&mut self, button: PointerButton, down: bool);
    /// Wheel motion in detents (positive = up/right).
    fn pointer_wheel(&mut self, delta: i16, hdelta: i16);
    /// Toggle-key state sync at session start (§10).
    fn sync_modifiers(&mut self, caps_lock: bool, num_lock: bool, scroll_lock: bool);
}

/// Dispatches one decoded message to the matching injector call.
pub fn dispatch(injector: &mut dyn InputInjector, msg: &InputMessage) {
    match *msg {
        InputMessage::Key { down, scancode, extended } => {
            injector.key(down, scancode, extended);
        }
        InputMessage::PointerMove { display_id, x, y } => {
            injector.pointer_move(display_id, x, y);
        }
        InputMessage::PointerButton { button, down } => {
            injector.pointer_button(button, down);
        }
        InputMessage::PointerWheel { delta, hdelta } => injector.pointer_wheel(delta, hdelta),
        InputMessage::SyncModifiers { caps_lock, num_lock, scroll_lock } => {
            injector.sync_modifiers(caps_lock, num_lock, scroll_lock);
        }
    }
}

/// Development injector: counts and logs events, injects nothing.
#[derive(Debug, Default)]
pub struct LogInjector {
    /// Events received so far.
    pub events: u64,
}

impl InputInjector for LogInjector {
    fn key(&mut self, down: bool, scancode: u16, extended: bool) {
        self.events += 1;
        tracing::debug!(down, scancode, extended, "input key");
    }

    fn pointer_move(&mut self, display_id: u8, x: i32, y: i32) {
        self.events += 1;
        tracing::trace!(display_id, x, y, "input move");
    }

    fn pointer_button(&mut self, button: PointerButton, down: bool) {
        self.events += 1;
        tracing::debug!(?button, down, "input button");
    }

    fn pointer_wheel(&mut self, delta: i16, hdelta: i16) {
        self.events += 1;
        tracing::debug!(delta, hdelta, "input wheel");
    }

    fn sync_modifiers(&mut self, caps_lock: bool, num_lock: bool, scroll_lock: bool) {
        self.events += 1;
        tracing::debug!(caps_lock, num_lock, scroll_lock, "input sync modifiers");
    }
}

/// Windows injector over `SendInput`.
#[cfg(all(windows, feature = "windows-capture"))]
pub use win::SendInputInjector;

#[cfg(all(windows, feature = "windows-capture"))]
mod win {
    use super::{InputInjector, PointerButton};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
        KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
        MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
        MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
        MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS,
        SendInput, VIRTUAL_KEY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
    };
    use windows::core::BOOL;

    /// `XBUTTON1`/`XBUTTON2` values for `MOUSEINPUT.mouseData` (winuser.h).
    const XBUTTON1: u32 = 0x0001;
    const XBUTTON2: u32 = 0x0002;

    /// Sends input events to the session the process runs in (§10).
    pub struct SendInputInjector {
        /// GDI device name of the captured output (e.g. `\\.\DISPLAY2`).
        /// Display-local pointer coordinates are translated through this
        /// monitor's desktop rectangle; without it they are assumed to start
        /// at the primary monitor's origin.
        output_device: Option<String>,
    }

    impl Default for SendInputInjector {
        fn default() -> Self {
            Self::new(None)
        }
    }

    /// One pass over the desktop monitors; the callback gets each monitor's
    /// device name and rectangle and may stop the enumeration with `false`.
    fn for_each_monitor(mut f: impl FnMut(&str, RECT) -> bool) {
        unsafe extern "system" fn tramp(
            monitor: HMONITOR,
            _hdc: HDC,
            _rect: *mut RECT,
            data: LPARAM,
        ) -> BOOL {
            // SAFETY: `data` is the `&mut dyn FnMut` installed by the caller;
            // `info` is a valid, correctly sized out struct.
            let f = unsafe { &mut *(data.0 as *mut &mut dyn FnMut(&str, RECT) -> bool) };
            let mut info = MONITORINFOEXW {
                monitorInfo: windows::Win32::Graphics::Gdi::MONITORINFO {
                    cbSize: u32::try_from(std::mem::size_of::<MONITORINFOEXW>()).unwrap_or(0),
                    ..Default::default()
                },
                ..Default::default()
            };
            let ok = unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast()) };
            if !ok.as_bool() {
                return true.into();
            }
            let name = {
                let len = info
                    .szDevice
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(info.szDevice.len());
                String::from_utf16_lossy(&info.szDevice[..len])
            };
            f(&name, info.monitorInfo.rcMonitor).into()
        }
        let mut f: &mut dyn FnMut(&str, RECT) -> bool = &mut f;
        // SAFETY: `f` outlives the call and is only used by `tramp` above.
        unsafe {
            let _ = EnumDisplayMonitors(None, None, Some(tramp), LPARAM((&raw mut f) as isize));
        }
    }

    impl SendInputInjector {
        /// Creates an injector bound to the captured `output_device` (GDI
        /// device name) when known.
        #[must_use]
        pub fn new(output_device: Option<&str>) -> Self {
            Self {
                output_device: output_device.map(str::to_owned),
            }
        }

        /// Desktop rectangle of the captured output, resolved on each move
        /// (monitor topology changes at runtime when the virtual display
        /// attaches/detaches). Falls back to the primary monitor at the
        /// desktop origin.
        fn output_rect(&self) -> (i32, i32, i32, i32) {
            if let Some(device) = &self.output_device {
                let mut found = None;
                for_each_monitor(|name, rect| {
                    if name == device {
                        found = Some(rect);
                        false
                    } else {
                        true
                    }
                });
                if let Some(rect) = found {
                    return (rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top);
                }
                tracing::warn!(device, "captured output not attached; using primary-origin mapping");
            }
            (0, 0, 0, 0)
        }

        fn send(inputs: &[INPUT]) {
            // SAFETY: `inputs` is a valid slice; SendInput copies from it.
            unsafe {
                let _ = SendInput(inputs, i32::try_from(std::mem::size_of::<INPUT>()).unwrap_or(0));
            }
        }

        fn mouse(flags: MOUSE_EVENT_FLAGS, data: u32, x: i32, y: i32) -> INPUT {
            INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: x,
                        dy: y,
                        mouseData: data,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            }
        }
    }

    impl InputInjector for SendInputInjector {
        fn key(&mut self, down: bool, scancode: u16, extended: bool) {
            let mut flags = KEYEVENTF_SCANCODE;
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !down {
                flags |= KEYEVENTF_KEYUP;
            }
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: scancode,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            Self::send(&[input]);
        }

        fn pointer_move(&mut self, _display_id: u8, x: i32, y: i32) {
            // Display-local → virtual-desktop: offset by the captured
            // monitor's desktop rectangle (extended layouts place it
            // anywhere, not at the origin).
            let (left, top, _, _) = self.output_rect();
            let (dx, dy) = (left + x, top + y);
            // Refresh the virtual-desktop metrics on every move: the virtual
            // display attaching/detaching changes them mid-session.
            // SAFETY: GetSystemMetrics is always safe to call.
            let (ox, oy, w, h) = unsafe {
                (
                    GetSystemMetrics(SM_XVIRTUALSCREEN),
                    GetSystemMetrics(SM_YVIRTUALSCREEN),
                    GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
                    GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
                )
            };
            // Normalize to 0..65535 over the virtual desktop.
            let nx = i32::try_from((i64::from(dx - ox) * 65_535 / i64::from(w)).clamp(0, 65_535))
                .unwrap_or(0);
            let ny = i32::try_from((i64::from(dy - oy) * 65_535 / i64::from(h)).clamp(0, 65_535))
                .unwrap_or(0);
            Self::send(&[Self::mouse(
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                0,
                nx,
                ny,
            )]);
        }

        fn pointer_button(&mut self, button: PointerButton, down: bool) {
            let (flags, data): (MOUSE_EVENT_FLAGS, u32) = match (button, down) {
                (PointerButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                (PointerButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                (PointerButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                (PointerButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                (PointerButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                (PointerButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                (PointerButton::X1, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
                (PointerButton::X1, false) => (MOUSEEVENTF_XUP, XBUTTON1),
                (PointerButton::X2, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
                (PointerButton::X2, false) => (MOUSEEVENTF_XUP, XBUTTON2),
            };
            Self::send(&[Self::mouse(flags, data, 0, 0)]);
        }

        fn pointer_wheel(&mut self, delta: i16, hdelta: i16) {
            let mut batch = Vec::with_capacity(2);
            if delta != 0 {
                batch.push(Self::mouse(MOUSEEVENTF_WHEEL, (i32::from(delta) * 120).cast_unsigned(), 0, 0));
            }
            if hdelta != 0 {
                batch.push(Self::mouse(MOUSEEVENTF_HWHEEL, (i32::from(hdelta) * 120).cast_unsigned(), 0, 0));
            }
            Self::send(&batch);
        }

        fn sync_modifiers(&mut self, caps_lock: bool, num_lock: bool, scroll_lock: bool) {
            // Phase 3 PoC: logged only; toggling keys remotely would need a
            // press/release pair per mismatched LED (§10).
            tracing::debug!(caps_lock, num_lock, scroll_lock, "modifier sync (not injected)");
        }
    }
}
