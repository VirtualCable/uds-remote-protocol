// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Input messages, client → server (design doc §10): fixed-size wire
//! format, no serde (hot path; the protocol crate stays clock- and
//! platform-free).
//!
//! Keyboard events carry **Set 1 scancodes** (not virtual keys) plus an
//! `extended` flag (`0xE0` prefix), like RDP/SPICE: the server maps them
//! with `SendInput`/`MapVirtualKey`, so client keyboard layouts never leak
//! into the protocol. Pointer coordinates are absolute pixels local to a
//! display (multi-monitor ready: `display_id`, §8.3); the server knows the
//! monitor layout and translates to virtual-desktop coordinates.
//!
//! Wire format: tag byte + big-endian fields.

/// Wire tag for [`InputMessage::Key`].
const TAG_KEY: u8 = 1;
/// Wire tag for [`InputMessage::PointerMove`].
const TAG_POINTER_MOVE: u8 = 2;
/// Wire tag for [`InputMessage::PointerButton`].
const TAG_POINTER_BUTTON: u8 = 3;
/// Wire tag for [`InputMessage::PointerWheel`].
const TAG_POINTER_WHEEL: u8 = 4;
/// Wire tag for [`InputMessage::SyncModifiers`].
const TAG_SYNC_MODIFIERS: u8 = 5;

/// Errors decoding input messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InputError {
    /// Buffer shorter than the message needs.
    #[error("truncated input message")]
    Truncated,
    /// Unknown message tag byte.
    #[error("unknown input tag: {0}")]
    UnknownTag(u8),
}

/// Mouse buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    /// Left button.
    Left,
    /// Right button.
    Right,
    /// Middle button.
    Middle,
    /// X1 (back).
    X1,
    /// X2 (forward).
    X2,
}

impl PointerButton {
    fn to_wire(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Right => 2,
            Self::Middle => 3,
            Self::X1 => 4,
            Self::X2 => 5,
        }
    }

    fn from_wire(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Left),
            2 => Some(Self::Right),
            3 => Some(Self::Middle),
            4 => Some(Self::X1),
            5 => Some(Self::X2),
            _ => None,
        }
    }
}

/// One input event (design doc §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMessage {
    /// Key press/release: Set 1 scancode + extended (`0xE0`) flag.
    Key {
        /// True for press, false for release.
        down: bool,
        /// Set 1 scancode (without the `0xE0` prefix).
        scancode: u16,
        /// True for extended keys (arrows, Ins/Del/Home/End/PgUp/PgDn...).
        extended: bool,
    },
    /// Absolute pointer move, in display-local pixels.
    PointerMove {
        /// Display the pointer is on (0 = primary).
        display_id: u8,
        /// X coordinate inside the display (may be negative when crossing
        /// between displays with negative virtual-desktop origins).
        x: i32,
        /// Y coordinate inside the display.
        y: i32,
    },
    /// Pointer button press/release.
    PointerButton {
        /// Which button.
        button: PointerButton,
        /// True for press, false for release.
        down: bool,
    },
    /// Wheel motion, in wheel detents (positive = up/right).
    PointerWheel {
        /// Vertical wheel delta.
        delta: i16,
        /// Horizontal wheel delta.
        hdelta: i16,
    },
    /// Modifier LED state sync, sent on session start so the remote
    /// toggle keys match the client's.
    SyncModifiers {
        /// Caps Lock on.
        caps_lock: bool,
        /// Num Lock on.
        num_lock: bool,
        /// Scroll Lock on.
        scroll_lock: bool,
    },
}

const KEY_LEN: usize = 4;
const MOVE_LEN: usize = 10;
const BUTTON_LEN: usize = 3;
const WHEEL_LEN: usize = 5;
const SYNC_LEN: usize = 2;

impl InputMessage {
    /// Largest encoded size ([`InputMessage::PointerMove`]).
    pub const MAX_ENCODED_LEN: usize = MOVE_LEN;

    /// Encodes into `out`, returning the written length.
    pub fn encode_into(&self, out: &mut [u8]) -> Result<usize, InputError> {
        let needed = match self {
            Self::Key { .. } => KEY_LEN,
            Self::PointerMove { .. } => MOVE_LEN,
            Self::PointerButton { .. } => BUTTON_LEN,
            Self::PointerWheel { .. } => WHEEL_LEN,
            Self::SyncModifiers { .. } => SYNC_LEN,
        };
        if out.len() < needed {
            return Err(InputError::Truncated);
        }
        match *self {
            Self::Key { down, scancode, extended } => {
                out[0] = TAG_KEY;
                out[1] = u8::from(down) | (u8::from(extended) << 1);
                out[2..4].copy_from_slice(&scancode.to_be_bytes());
            }
            Self::PointerMove { display_id, x, y } => {
                out[0] = TAG_POINTER_MOVE;
                out[1] = display_id;
                out[2..6].copy_from_slice(&x.to_be_bytes());
                out[6..10].copy_from_slice(&y.to_be_bytes());
            }
            Self::PointerButton { button, down } => {
                out[0] = TAG_POINTER_BUTTON;
                out[1] = button.to_wire();
                out[2] = u8::from(down);
            }
            Self::PointerWheel { delta, hdelta } => {
                out[0] = TAG_POINTER_WHEEL;
                out[1..3].copy_from_slice(&delta.to_be_bytes());
                out[3..5].copy_from_slice(&hdelta.to_be_bytes());
            }
            Self::SyncModifiers { caps_lock, num_lock, scroll_lock } => {
                out[0] = TAG_SYNC_MODIFIERS;
                out[1] = u8::from(caps_lock)
                    | (u8::from(num_lock) << 1)
                    | (u8::from(scroll_lock) << 2);
            }
        }
        Ok(needed)
    }

    /// Encodes to a fixed-size buffer (convenience wrapper).
    #[must_use]
    pub fn encode(&self) -> ([u8; Self::MAX_ENCODED_LEN], usize) {
        let mut out = [0u8; Self::MAX_ENCODED_LEN];
        let n = self
            .encode_into(&mut out)
            .expect("MAX_ENCODED_LEN fits every message");
        (out, n)
    }

    /// Decodes one message from the wire format.
    pub fn decode(bytes: &[u8]) -> Result<Self, InputError> {
        let (&tag, rest) = bytes.split_first().ok_or(InputError::Truncated)?;
        match tag {
            TAG_KEY => {
                if rest.len() < 3 {
                    return Err(InputError::Truncated);
                }
                Ok(Self::Key {
                    down: rest[0] & 1 != 0,
                    extended: rest[0] & 2 != 0,
                    scancode: u16::from_be_bytes(rest[1..3].try_into().expect("2-byte slice")),
                })
            }
            TAG_POINTER_MOVE => {
                if rest.len() < 9 {
                    return Err(InputError::Truncated);
                }
                Ok(Self::PointerMove {
                    display_id: rest[0],
                    x: i32::from_be_bytes(rest[1..5].try_into().expect("4-byte slice")),
                    y: i32::from_be_bytes(rest[5..9].try_into().expect("4-byte slice")),
                })
            }
            TAG_POINTER_BUTTON => {
                if rest.len() < 2 {
                    return Err(InputError::Truncated);
                }
                let button = PointerButton::from_wire(rest[0])
                    .ok_or(InputError::UnknownTag(rest[0]))?;
                Ok(Self::PointerButton { button, down: rest[1] != 0 })
            }
            TAG_POINTER_WHEEL => {
                if rest.len() < 4 {
                    return Err(InputError::Truncated);
                }
                Ok(Self::PointerWheel {
                    delta: i16::from_be_bytes(rest[0..2].try_into().expect("2-byte slice")),
                    hdelta: i16::from_be_bytes(rest[2..4].try_into().expect("2-byte slice")),
                })
            }
            TAG_SYNC_MODIFIERS => {
                if rest.is_empty() {
                    return Err(InputError::Truncated);
                }
                Ok(Self::SyncModifiers {
                    caps_lock: rest[0] & 1 != 0,
                    num_lock: rest[0] & 2 != 0,
                    scroll_lock: rest[0] & 4 != 0,
                })
            }
            other => Err(InputError::UnknownTag(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: InputMessage) {
        let (buf, n) = msg.encode();
        assert_eq!(InputMessage::decode(&buf[..n]), Ok(msg));
    }

    #[test]
    fn all_variants_roundtrip() {
        for msg in [
            InputMessage::Key { down: true, scancode: 0x1E, extended: false },
            InputMessage::Key { down: false, scancode: 0x4D, extended: true },
            InputMessage::PointerMove { display_id: 2, x: -500, y: 3840 },
            InputMessage::PointerButton { button: PointerButton::X2, down: true },
            InputMessage::PointerButton { button: PointerButton::Left, down: false },
            InputMessage::PointerWheel { delta: -3, hdelta: 2 },
            InputMessage::SyncModifiers { caps_lock: true, num_lock: false, scroll_lock: true },
        ] {
            roundtrip(msg);
        }
    }

    #[test]
    fn truncated_is_rejected() {
        assert_eq!(InputMessage::decode(&[]), Err(InputError::Truncated));
        let (buf, n) = InputMessage::PointerMove { display_id: 0, x: 1, y: 2 }.encode();
        assert_eq!(InputMessage::decode(&buf[..n - 1]), Err(InputError::Truncated));
    }

    #[test]
    fn unknown_tag_is_rejected() {
        assert_eq!(InputMessage::decode(&[0xEE]), Err(InputError::UnknownTag(0xEE)));
    }

    #[test]
    fn unknown_button_is_rejected() {
        assert!(matches!(
            InputMessage::decode(&[TAG_POINTER_BUTTON, 99, 1]),
            Err(InputError::UnknownTag(99))
        ));
    }

    #[test]
    fn key_flags_pack_down_and_extended() {
        let (buf, n) = InputMessage::Key { down: true, scancode: 0x38, extended: true }.encode();
        assert_eq!((&buf[..n], n), (&[TAG_KEY, 0b11, 0, 0x38][..], KEY_LEN));
    }
}
