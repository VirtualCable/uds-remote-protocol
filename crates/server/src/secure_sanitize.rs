// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Secure-desktop input allowlist (§18.41, riesgos r2 §3.3): while the
//! helper injects on the Winlogon desktop, Windows keys and accessibility
//! toggle sequences are blocked before injection — the helper is the
//! enforcement point (the client is untrusted). Kept platform-neutral (no
//! Win32) so the policy is unit-testable everywhere.

use crate::input::InputInjector;

/// Set 1 scancode allowlist wrapper around an [`InputInjector`].
#[cfg_attr(
    not(all(windows, feature = "windows-capture")),
    allow(dead_code, reason = "only the Windows helper constructs it")
)]
pub(crate) struct Sanitizer<I> {
    inner: I,
    /// Consecutive Shift-only presses (StickyKeys needs 5).
    shift_streak: u8,
    /// Modifier state for the Alt+Shift+PrtScr (high contrast) rule.
    alt_down: bool,
    shift_down: bool,
}

#[cfg_attr(
    not(all(windows, feature = "windows-capture")),
    allow(dead_code, reason = "only the Windows helper constructs it")
)]
impl<I: InputInjector> Sanitizer<I> {
    pub(crate) fn new(inner: I) -> Self {
        Self {
            inner,
            alt_down: false,
            shift_down: false,
            shift_streak: 0,
        }
    }

    pub(crate) fn dispatch(&mut self, msg: protocol::InputMessage) {
        if let protocol::InputMessage::Key {
            down,
            scancode,
            extended,
        } = msg
            && !self.key_allowed(down, scancode, extended)
        {
            return;
        }
        crate::input::dispatch(&mut self.inner, &msg);
    }

    /// Set 1 scancode policy. Returns false for blocked keys.
    fn key_allowed(&mut self, down: bool, scancode: u16, extended: bool) -> bool {
        // Windows keys and the menu key: never (logon accessibility surface).
        if extended && matches!(scancode, 0x5B..=0x5D) {
            return false;
        }
        match (scancode, extended) {
            // Track modifier state (used by the high-contrast rule below).
            (0x38, _) => self.alt_down = down,
            (0x2A | 0x36, false) => {
                self.shift_down = down;
                // StickyKeys: five Shift-only presses in a row toggle it.
                if down {
                    self.shift_streak += 1;
                    if self.shift_streak >= 5 {
                        self.shift_streak = 0;
                        return false;
                    }
                }
            }
            // PrintScreen (0x37 extended, or 0x54 = Alt+PrtScr) with Alt and
            // Shift held toggles high contrast on the logon desktop.
            (0x37 | 0x54, _) if down && self.alt_down && self.shift_down => return false,
            // Any non-shift key breaks the StickyKeys streak.
            _ if down => self.shift_streak = 0,
            _ => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::LogInjector;
    use protocol::InputMessage;

    fn key(down: bool, scancode: u16, extended: bool) -> InputMessage {
        InputMessage::Key {
            down,
            scancode,
            extended,
        }
    }

    #[test]
    fn plain_keys_pass() {
        let mut s = Sanitizer::new(LogInjector::default());
        s.dispatch(key(true, 0x1E, false)); // 'a'
        s.dispatch(key(false, 0x1E, false));
        s.dispatch(key(true, 0x1C, false)); // Enter
        assert_eq!(s.inner.events, 3);
    }

    #[test]
    fn windows_and_menu_keys_are_blocked() {
        let mut s = Sanitizer::new(LogInjector::default());
        for scancode in [0x5B, 0x5C, 0x5D] {
            s.dispatch(key(true, scancode, true));
            s.dispatch(key(false, scancode, true));
        }
        assert_eq!(s.inner.events, 0);
    }

    #[test]
    fn sticky_keys_sequence_is_blocked_at_five() {
        let mut s = Sanitizer::new(LogInjector::default());
        // Four full Shift presses pass; the fifth press is swallowed.
        for _ in 0..4 {
            s.dispatch(key(true, 0x2A, false));
            s.dispatch(key(false, 0x2A, false));
        }
        s.dispatch(key(true, 0x2A, false));
        assert_eq!(s.inner.events, 8);
        // The streak reset: Shift works again right away.
        s.dispatch(key(false, 0x2A, false));
        s.dispatch(key(true, 0x2A, false));
        assert_eq!(s.inner.events, 10);
    }

    #[test]
    fn sticky_keys_streak_breaks_on_other_keys() {
        let mut s = Sanitizer::new(LogInjector::default());
        for _ in 0..4 {
            s.dispatch(key(true, 0x2A, false));
            s.dispatch(key(false, 0x2A, false));
            s.dispatch(key(true, 0x1E, false)); // breaks the streak
        }
        s.dispatch(key(true, 0x2A, false));
        assert_eq!(s.inner.events, 13); // nothing was blocked
    }

    #[test]
    fn high_contrast_hotkey_is_blocked() {
        let mut s = Sanitizer::new(LogInjector::default());
        s.dispatch(key(true, 0x38, false)); // Alt down
        s.dispatch(key(true, 0x2A, false)); // Shift down
        s.dispatch(key(true, 0x37, true)); // PrtScr — blocked
        assert_eq!(s.inner.events, 2);
        // Without both modifiers the same key passes.
        s.dispatch(key(false, 0x38, false));
        s.dispatch(key(true, 0x37, true));
        assert_eq!(s.inner.events, 4);
    }

    #[test]
    fn pointer_and_other_messages_pass_untouched() {
        let mut s = Sanitizer::new(LogInjector::default());
        s.dispatch(InputMessage::PointerMove {
            display_id: 0,
            x: 100,
            y: 200,
        });
        s.dispatch(InputMessage::PointerWheel {
            delta: 1,
            hdelta: 0,
        });
        s.dispatch(InputMessage::SyncModifiers {
            caps_lock: false,
            num_lock: true,
            scroll_lock: false,
        });
        assert_eq!(s.inner.events, 3);
    }
}
