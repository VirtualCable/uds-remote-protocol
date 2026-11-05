// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

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
/// Channel 5: cursor shape/visibility (S→C, reliable; the client paints the
/// cursor locally and keeps the last shape as state, so loss is intolerable).
pub const CURSOR: ChannelId = 5;
/// Channel 6: clipboard (bidirectional, reliable; offer/request/data pull
/// model — payloads must not be dropped once a generation was announced).
pub const CLIPBOARD: ChannelId = 6;
/// Channel 7: storage (bidirectional, reliable; explicit file transfers —
/// chunked and potentially large, so it must never share the lossy leg).
pub const STORAGE: ChannelId = 7;
/// Channel 8: smartcard redirection (S→C requests / C→S replies, reliable;
/// APDUs tolerate neither loss nor reordering — the volume is trivial).
pub const SMARTCARD: ChannelId = 8;
/// Channel 9: webcam redirection (C→S, lossy; one self-contained VP9 frame
/// per logical element, fragmented exactly like the display channel — a
/// lost fragment drops the whole frame and the client resyncs on the next
/// keyframe request; design doc §18.57).
pub const WEBCAM: ChannelId = 9;
/// Channel 10: printer redirection (S→C jobs / C→S replies, reliable;
/// print data tolerates neither loss nor reordering — §18.57).
pub const PRINTER: ChannelId = 10;

/// Last ID reserved for UDS core channels (`0x0000..=0x00FF`).
pub const CORE_MAX: ChannelId = 0x00FF;
