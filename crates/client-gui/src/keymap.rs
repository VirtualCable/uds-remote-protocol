// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! winit `KeyCode` → Set 1 scancode + extended flag mapping, ported from
//! uds-client's `gui::keymap` (same BSD-3-Clause, Virtual Cable S.L.).
//!
//! Set 1 scancodes are what the protocol's `InputMessage::Key` carries;
//! "extended" keys (arrows, navigation block, right modifiers, ...) are
//! flagged so the server emits the `0xE0` prefix.

use winit::keyboard::KeyCode;

/// Maps a winit key to `(scancode, extended)`.
#[must_use]
#[expect(clippy::too_many_lines, reason = "a flat key table is clearer long")]
pub fn map_key(code: KeyCode) -> Option<(u16, bool)> {
    let raw: u16 = match code {
        KeyCode::Escape => 0x0001,
        KeyCode::Digit1 => 0x0002,
        KeyCode::Digit2 => 0x0003,
        KeyCode::Digit3 => 0x0004,
        KeyCode::Digit4 => 0x0005,
        KeyCode::Digit5 => 0x0006,
        KeyCode::Digit6 => 0x0007,
        KeyCode::Digit7 => 0x0008,
        KeyCode::Digit8 => 0x0009,
        KeyCode::Digit9 => 0x000A,
        KeyCode::Digit0 => 0x000B,
        KeyCode::Minus => 0x000C,
        KeyCode::Equal => 0x000D,
        KeyCode::Backspace => 0x000E,
        KeyCode::Tab => 0x000F,
        KeyCode::KeyQ => 0x0010,
        KeyCode::KeyW => 0x0011,
        KeyCode::KeyE => 0x0012,
        KeyCode::KeyR => 0x0013,
        KeyCode::KeyT => 0x0014,
        KeyCode::KeyY => 0x0015,
        KeyCode::KeyU => 0x0016,
        KeyCode::KeyI => 0x0017,
        KeyCode::KeyO => 0x0018,
        KeyCode::KeyP => 0x0019,
        KeyCode::BracketLeft => 0x001A,
        KeyCode::BracketRight => 0x001B,
        KeyCode::Enter => 0x001C,
        KeyCode::ControlLeft => 0x001D,
        KeyCode::KeyA => 0x001E,
        KeyCode::KeyS => 0x001F,
        KeyCode::KeyD => 0x0020,
        KeyCode::KeyF => 0x0021,
        KeyCode::KeyG => 0x0022,
        KeyCode::KeyH => 0x0023,
        KeyCode::KeyJ => 0x0024,
        KeyCode::KeyK => 0x0025,
        KeyCode::KeyL => 0x0026,
        KeyCode::Semicolon => 0x0027,
        KeyCode::Quote => 0x0028,
        KeyCode::Backquote => 0x0029,
        KeyCode::ShiftLeft => 0x002A,
        KeyCode::Backslash => 0x002B,
        KeyCode::KeyZ => 0x002C,
        KeyCode::KeyX => 0x002D,
        KeyCode::KeyC => 0x002E,
        KeyCode::KeyV => 0x002F,
        KeyCode::KeyB => 0x0030,
        KeyCode::KeyN => 0x0031,
        KeyCode::KeyM => 0x0032,
        KeyCode::Comma => 0x0033,
        KeyCode::Period => 0x0034,
        KeyCode::Slash => 0x0035,
        KeyCode::ShiftRight => 0x0036,
        KeyCode::NumpadMultiply => 0x0037,
        KeyCode::AltLeft => 0x0038,
        KeyCode::Space => 0x0039,
        KeyCode::CapsLock => 0x003A,
        KeyCode::F1 => 0x003B,
        KeyCode::F2 => 0x003C,
        KeyCode::F3 => 0x003D,
        KeyCode::F4 => 0x003E,
        KeyCode::F5 => 0x003F,
        KeyCode::F6 => 0x0040,
        KeyCode::F7 => 0x0041,
        KeyCode::F8 => 0x0042,
        KeyCode::F9 => 0x0043,
        KeyCode::F10 => 0x0044,
        KeyCode::NumLock => 0x0045,
        KeyCode::ScrollLock => 0x0046,
        KeyCode::Numpad7 => 0x0047,
        KeyCode::Numpad8 => 0x0048,
        KeyCode::Numpad9 => 0x0049,
        KeyCode::NumpadSubtract => 0x004A,
        KeyCode::Numpad4 => 0x004B,
        KeyCode::Numpad5 => 0x004C,
        KeyCode::Numpad6 => 0x004D,
        KeyCode::NumpadAdd => 0x004E,
        KeyCode::Numpad1 => 0x004F,
        KeyCode::Numpad2 => 0x0050,
        KeyCode::Numpad3 => 0x0051,
        KeyCode::Numpad0 => 0x0052,
        KeyCode::NumpadDecimal => 0x0053,
        KeyCode::IntlRo | KeyCode::IntlBackslash => 0x0056,
        KeyCode::F11 => 0x0057,
        KeyCode::F12 => 0x0058,
        KeyCode::F13 => 0x0064,
        KeyCode::F14 => 0x0065,
        KeyCode::F15 => 0x0066,
        KeyCode::F16 => 0x0067,
        KeyCode::F17 => 0x0068,
        KeyCode::F18 => 0x0069,
        KeyCode::F19 => 0x006A,
        KeyCode::F20 => 0x006B,
        // Extended keys (high byte set ⇒ the server adds the 0xE0 prefix).
        KeyCode::NumpadEnter => 0x011C,
        KeyCode::ControlRight => 0x011D,
        KeyCode::NumpadDivide => 0x0135,
        KeyCode::PrintScreen => 0x0137,
        KeyCode::AltRight => 0x0138,
        KeyCode::Pause => 0x0146,
        KeyCode::Home => 0x0147,
        KeyCode::ArrowUp => 0x0148,
        KeyCode::PageUp => 0x0149,
        KeyCode::ArrowLeft => 0x014B,
        KeyCode::ArrowRight => 0x014D,
        KeyCode::End => 0x014F,
        KeyCode::ArrowDown => 0x0150,
        KeyCode::PageDown => 0x0151,
        KeyCode::Insert => 0x0152,
        KeyCode::Delete => 0x0153,
        KeyCode::SuperLeft => 0x015B,
        KeyCode::SuperRight => 0x015C,
        KeyCode::ContextMenu => 0x015D,
        KeyCode::AudioVolumeMute => 0x0120,
        KeyCode::AudioVolumeDown => 0x012E,
        KeyCode::AudioVolumeUp => 0x0130,
        _ => return None,
    };
    let extended = raw >= 0x0100;
    Some((raw & 0x00FF, extended))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_keys_are_not_extended() {
        assert_eq!(map_key(KeyCode::KeyA), Some((0x1E, false)));
        assert_eq!(map_key(KeyCode::Enter), Some((0x1C, false)));
    }

    #[test]
    fn navigation_block_is_extended() {
        assert_eq!(map_key(KeyCode::ArrowUp), Some((0x48, true)));
        assert_eq!(map_key(KeyCode::Delete), Some((0x53, true)));
        assert_eq!(map_key(KeyCode::ControlRight), Some((0x1D, true)));
    }
}
