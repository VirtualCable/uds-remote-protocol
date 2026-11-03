// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

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
//! - Pointer: coordinates arrive **normalized to 0..=65535 over the whole
//!   virtual desktop** (§18.37), exactly the range
//!   `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` expects, so injection
//!   is a pass-through — no monitor-layout translation, no resize races.

use protocol::{InputMessage, PointerButton};

/// One input-event sink: platform-neutral injection contract.
pub trait InputInjector {
    /// Key press/release (Set 1 scancode + extended flag).
    fn key(&mut self, down: bool, scancode: u16, extended: bool);
    /// Absolute pointer move, normalized 0..=65535 over the virtual desktop.
    fn pointer_move(&mut self, display_id: u8, x: u16, y: u16);
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
            // LWin is rare enough to log at debug: diagnoses Win+L delivery.
            if scancode == 0x5B && extended {
                tracing::debug!(down, "LWin key event received");
            }
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

    fn pointer_move(&mut self, display_id: u8, x: u16, y: u16) {
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

    /// `XBUTTON1`/`XBUTTON2` values for `MOUSEINPUT.mouseData` (winuser.h).
    const XBUTTON1: u32 = 0x0001;
    const XBUTTON2: u32 = 0x0002;

    /// Sends input events to the session the process runs in (§10).
    ///
    /// Pointer coordinates arrive normalized to 0..=65535 over the whole
    /// virtual desktop (§18.37), the exact range
    /// `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` expects, so moves
    /// are a pass-through: no monitor-layout translation, no resize races.
    #[derive(Default)]
    pub struct SendInputInjector;

    impl SendInputInjector {
        /// Creates an injector.
        #[must_use]
        pub fn new() -> Self {
            Self
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

        fn pointer_move(&mut self, _display_id: u8, x: u16, y: u16) {
            // Normalized 0..=65535 over the virtual desktop (§18.37):
            // straight into MOUSEEVENTF_ABSOLUTE | VIRTUALDESK.
            Self::send(&[Self::mouse(
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                0,
                i32::from(x),
                i32::from(y),
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
