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
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE,
        MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
        MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
        MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
        MOUSE_EVENT_FLAGS, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
    };

    /// Sends input events to the session the process runs in (§10).
    pub struct SendInputInjector {
        /// Virtual-desktop origin (monitors can start at negative coords).
        origin: (i32, i32),
        /// Virtual-desktop size for absolute-coordinate normalization.
        size: (i32, i32),
    }

    impl SendInputInjector {
        /// Captures the current virtual-desktop geometry.
        #[must_use]
        pub fn new() -> Self {
            // SAFETY: GetSystemMetrics is always safe to call.
            let (ox, oy, w, h) = unsafe {
                (
                    GetSystemMetrics(SM_XVIRTUALSCREEN),
                    GetSystemMetrics(SM_YVIRTUALSCREEN),
                    GetSystemMetrics(SM_CXVIRTUALSCREEN),
                    GetSystemMetrics(SM_CYVIRTUALSCREEN),
                )
            };
            Self { origin: (ox, oy), size: (w.max(1), h.max(1)) }
        }

        fn send(inputs: &[INPUT]) {
            // SAFETY: `inputs` is a valid slice; SendInput copies from it.
            unsafe {
                let _ = SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
            }
        }

        fn mouse(flags: MOUSE_EVENT_FLAGS, data: i32, x: i32, y: i32) -> INPUT {
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
                        wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(0),
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
            // Display 0 shares its origin with the virtual desktop only when
            // the primary monitor is top-left; proper multi-monitor mapping
            // (EnumDisplayMonitors) arrives with multi-display support (§8.3).
            let (vx, vy) = (x - self.origin.0, y - self.origin.1);
            // Normalize to 0..65535 over the virtual desktop.
            let nx = (i64::from(vx) * 65_535 / i64::from(self.size.0)) as i32;
            let ny = (i64::from(vy) * 65_535 / i64::from(self.size.1)) as i32;
            Self::send(&[Self::mouse(
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                0,
                nx,
                ny,
            )]);
        }

        fn pointer_button(&mut self, button: PointerButton, down: bool) {
            let (flags, data) = match (button, down) {
                (PointerButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                (PointerButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                (PointerButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                (PointerButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                (PointerButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                (PointerButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                (PointerButton::X1, true) => (MOUSEEVENTF_XDOWN, XBUTTON1.0 as i32),
                (PointerButton::X1, false) => (MOUSEEVENTF_XUP, XBUTTON1.0 as i32),
                (PointerButton::X2, true) => (MOUSEEVENTF_XDOWN, XBUTTON2.0 as i32),
                (PointerButton::X2, false) => (MOUSEEVENTF_XUP, XBUTTON2.0 as i32),
            };
            Self::send(&[Self::mouse(flags, data, 0, 0)]);
        }

        fn pointer_wheel(&mut self, delta: i16, hdelta: i16) {
            let mut batch = Vec::with_capacity(2);
            if delta != 0 {
                batch.push(Self::mouse(MOUSEEVENTF_WHEEL, i32::from(delta) * 120, 0, 0));
            }
            if hdelta != 0 {
                batch.push(Self::mouse(MOUSEEVENTF_HWHEEL, i32::from(hdelta) * 120, 0, 0));
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
