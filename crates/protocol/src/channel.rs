//! Channel identifiers and ranges (design doc §5, riesgos.md r2 §4.2).

/// Logical channel identifier.
pub type ChannelId = u16;

/// Channel 0: control (post-login handshake, negotiation, keepalive, metrics).
pub const CONTROL: ChannelId = 0;
/// Channel 1: display (video frames S→C).
pub const DISPLAY: ChannelId = 1;
/// Channel 2: remote desktop audio (S→C).
pub const AUDIO_OUT: ChannelId = 2;
/// Channel 3: client microphone (C→S).
pub const AUDIO_IN: ChannelId = 3;
/// Channel 4: input (keyboard and mouse, C→S).
pub const INPUT: ChannelId = 4;

/// Last ID reserved for UDS core channels (`0x0000..=0x00FF`).
pub const CORE_MAX: ChannelId = 0x00FF;
