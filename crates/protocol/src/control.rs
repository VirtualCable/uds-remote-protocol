// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Channel 0 (control) messages, serialized with postcard.

use serde::{Deserialize, Serialize};

use crate::channel::ChannelId;
use crate::error::ProtocolError;

/// Capability announced during negotiation (`SERVER_CAPS` / `CLIENT_CAPS`).
///
/// Extensible `(kind, version, params)` list: a peer that does not know a
/// `kind` ignores it, so adding channels or codecs does not break old
/// peers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    /// Capability type (channel, video codec, audio codec, ...).
    pub kind: u16,
    /// Capability version.
    pub version: u16,
    /// Opaque type-specific parameters.
    pub params: Vec<u8>,
}

/// Control channel messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlMessage {
    /// Server capabilities (channels, codecs, policy limits,
    /// UDP port/token when available).
    ServerCaps {
        /// List of offered capabilities.
        caps: Vec<Capability>,
    },
    /// Client capabilities + selection.
    ClientCaps {
        /// List of requested capabilities.
        caps: Vec<Capability>,
    },
    /// Channel open request.
    ChannelOpen {
        /// Requested channel ID.
        id: ChannelId,
        /// Channel type.
        kind: u16,
        /// Channel version.
        version: u16,
    },
    /// Channel open confirmation.
    ChannelOpenAck {
        /// Confirmed channel ID.
        id: ChannelId,
    },
    /// Channel not supported by this peer; the session continues without it.
    UnsupportedChannel {
        /// Rejected channel ID.
        id: ChannelId,
    },
    /// UDP leg offer (§4.2): server-advertised port and session token.
    /// The client answers with an encrypted `Probe` datagram; if no valid
    /// traffic arrives, lossy channels stay on TCP.
    UdpOffer {
        /// Server UDP port.
        port: u16,
        /// Session token (derived from ticket+secret in direct mode;
        /// relay-assigned in a future tunnel leg).
        token: [u8; 16],
    },
    /// Display stream announcement (§8.3/§8.4): codec and geometry of one
    /// display, sent before its first frame and whenever they change.
    /// `codec` is a `VideoCodecType` wire ID (codec crate); unknown IDs must
    /// be ignored (frames for that display are then dropped with a log).
    DisplayInfo {
        /// Display identifier (0 = primary).
        display_id: u8,
        /// Frame width in physical pixels.
        width: u32,
        /// Frame height in physical pixels.
        height: u32,
        /// `VideoCodecType` wire ID.
        codec: u16,
    },
    /// Link-quality report, client → server (§12.2): one per probe window
    /// (~1 s) on the reliable control channel. The server feeds these into
    /// its `QualityController` (§12.5).
    MetricsReport {
        /// Smoothed RTT in microseconds (EWMA over probe acks).
        rtt_micros: u64,
        /// RTT variation in microseconds (EWMA of |ΔRTT|).
        jitter_micros: u64,
        /// UDP loss rate in the window, in permille (0-1000), from seq gaps.
        loss_permille: u16,
    },
    /// Keyframe request, client → server (§12.2): sent when fragment loss
    /// makes the predictive stream undecodable. Rate-limited by both sides
    /// (max 1 per 500 ms — riesgos §2.1).
    RequestKeyframe {
        /// Display whose stream must restart from a keyframe.
        display_id: u8,
    },
    /// Control channel heartbeat.
    Keepalive,
    /// Audio stream announcement (§9), server → client: codec and format of
    /// one audio direction, sent before its first packet and whenever they
    /// change. `codec` is an `AudioCodecType` wire ID (codec crate); unknown
    /// IDs must be ignored (packets for that stream are then dropped with a
    /// log). Appended at the end so existing postcard variant indices do not
    /// shift.
    AudioConfig {
        /// `AudioCodecType` wire ID.
        codec: u16,
        /// Sample rate in Hz (e.g. 48000).
        sample_rate: u32,
        /// Channels: 1 (mono) or 2 (stereo).
        channels: u8,
        /// Target bitrate in kbps.
        bitrate_kbps: u32,
        /// Frame duration in milliseconds (one frame per datagram).
        frame_ms: u8,
        /// In-band FEC enabled on the encoder.
        fec: bool,
    },
    /// Audio-in support announcement (§9.2), server → client: the server has
    /// a real microphone sink and accepts mic audio on channel `AUDIO_IN` in
    /// exactly this format. Absence means no mic support: the client must
    /// not send `AUDIO_IN` packets then (a server without a sink discards
    /// them defensively). Appended at the end so existing postcard variant
    /// indices do not shift.
    AudioInConfig {
        /// `AudioCodecType` wire ID.
        codec: u16,
        /// Sample rate in Hz (fixed 48000 in the PoC).
        sample_rate: u32,
        /// Channels: 1 (mono; the PoC wire format).
        channels: u8,
        /// Target bitrate in kbps.
        bitrate_kbps: u32,
        /// Frame duration in milliseconds (one frame per datagram).
        frame_ms: u8,
        /// In-band FEC expected on the encoder.
        fec: bool,
    },
    /// Desktop resize request, client → server (§18.37): the client's
    /// preferred resolution changed mid-session (window resize, fullscreen
    /// toggle). The server re-applies its resolution policy (fixed config
    /// wins; otherwise closest supported mode not exceeding the request)
    /// and answers with a fresh `DisplayInfo` carrying the real geometry.
    /// Appended at the end so existing postcard variant indices do not
    /// shift.
    DesktopResize {
        /// Preferred width in pixels.
        width: u32,
        /// Preferred height in pixels.
        height: u32,
    },
    /// Secure-desktop transition, server → client (§18.41, M9): the console
    /// switched to/from a secure desktop (UAC consent, WinLogon, lock
    /// screen). While active, frames come from the low-rate GDI helper path
    /// and the client should show a "secure desktop — reduced mode" notice.
    /// Appended at the end so existing postcard variant indices do not
    /// shift.
    SecureDesktop {
        /// True while a secure desktop owns the console.
        active: bool,
    },
    /// Session lock request, client → server (§18.50): the user asked to
    /// lock the remote session (Ctrl+Alt+L substitute for Win+L, which the
    /// local OS always intercepts). The server locks its own console via the
    /// OS API (`LockWorkStation` on Windows) — injecting the Win+L keysym is
    /// unreliable by design (the shell treats the synthesized hotkey as
    /// untrusted), so the request travels as an explicit control message.
    /// Appended at the end so existing postcard variant indices do not
    /// shift.
    LockRequest,
    /// Pre-session gate, server → client (§18.53): the conflict policy is
    /// resolved and the session is about to be handed off to the agent; the
    /// next control message will be the agent's `UdpOffer`. Always sent by
    /// the orchestrator service before the handoff so the client can tell a
    /// pending console-conflict exchange apart from a ready session. The
    /// laboratory agent (no service) never sends it: the client treats an
    /// early `UdpOffer` as an implicit ready.
    /// Appended at the end so existing postcard variant indices do not
    /// shift.
    SessionReady,
    /// Pre-session refusal, server → client (§18.53): the session is
    /// rejected after a successful handshake; `reason` is one of the
    /// `SESSION_END_*` constants. The connection is dropped right after.
    /// Appended at the end so existing postcard variant indices do not
    /// shift.
    SessionEnd {
        /// `SESSION_END_*` reason code.
        reason: u8,
    },
    /// Console-conflict query, server → client (§18.53, policy `ask`):
    /// another user's session owns the console. `user` is only `Some` when
    /// the server policy allows revealing it; `state` is one of the
    /// `CONSOLE_STATE_*` constants. The client must answer with
    /// `ConsoleConflictReply` before the server's ask timeout.
    /// Appended at the end so existing postcard variant indices do not
    /// shift.
    ConsoleConflict {
        /// Console owner's name when the server policy reveals it.
        user: Option<String>,
        /// `CONSOLE_STATE_*` constant.
        state: u8,
    },
    /// Console-conflict answer, client → server (§18.53): `true` evicts the
    /// console owner and continues, `false` refuses (the server answers
    /// with `SessionEnd`). Appended at the end so existing postcard variant
    /// indices do not shift.
    ConsoleConflictReply {
        /// True to take the console over.
        take_over: bool,
    },
    /// Pre-session progress notice, server → client (§18.53): the wait has
    /// stages (evicting a previous console owner, waiting for the
    /// autologon) that can take several seconds; the client shows them so
    /// the user does not read the wait as a hang. `stage` is one of the
    /// `SESSION_STAGE_*` constants. Appended at the end so existing
    /// postcard variant indices do not shift.
    SessionProgress {
        /// `SESSION_STAGE_*` constant.
        stage: u8,
    },
    /// Webcam capabilities, client → server (§18.57): sent once after
    /// `ClientCaps` when the client offered `FLAG_WEBCAM` in the HELLO.
    /// `codecs` holds `VideoCodecType` wire IDs the client can encode,
    /// most-preferred first. Appended at the end so existing postcard
    /// variant indices do not shift.
    WebcamCaps {
        /// Maximum capture width the camera supports.
        max_width: u32,
        /// Maximum capture height the camera supports.
        max_height: u32,
        /// Maximum capture frame rate.
        max_fps: u8,
        /// Encodable `VideoCodecType` wire IDs (VP9 only in this phase).
        codecs: Vec<u16>,
    },
    /// Webcam stream configuration, server → client (§18.57): the server
    /// has a camera sink and picks the wire format (Server > Client: the
    /// choice is clamped to the client's announced `WebcamCaps`). Absence
    /// means no webcam support: the client must not send `WEBCAM` packets
    /// then. Appended at the end so existing postcard variant indices do
    /// not shift.
    WebcamConfig {
        /// `VideoCodecType` wire ID (VP9 in this phase).
        codec: u16,
        /// Capture width in pixels.
        width: u32,
        /// Capture height in pixels.
        height: u32,
        /// Capture frame rate.
        fps: u8,
        /// Target bitrate in kbps (adapted at runtime, §12).
        bitrate_kbps: u32,
    },
    /// Starts the webcam stream, server → client (§18.57): an app opened
    /// the virtual camera (or the session policy wants it always on).
    /// Appended at the end so existing postcard variant indices do not
    /// shift.
    WebcamStart,
    /// Stops the webcam stream, server → client (§18.57): no consumer left
    /// on the virtual camera; the client stops capturing immediately.
    /// Appended at the end so existing postcard variant indices do not
    /// shift.
    WebcamStop,
    /// Webcam keyframe request, server → client (§18.57): fragment loss
    /// made the VP9 stream undecodable; rate-limited like the display
    /// `RequestKeyframe` (max 1 per 500 ms — riesgos §2.1). Appended at the
    /// end so existing postcard variant indices do not shift.
    WebcamRequestKeyframe,
}

/// `SessionEnd` reason: the console is owned by another user and the server
/// policy is `deny` (§18.53).
pub const SESSION_END_CONSOLE_BUSY: u8 = 1;
/// `SessionEnd` reason: the connecting user refused the console takeover
/// (or the ask timed out) under policy `ask` (§18.53).
pub const SESSION_END_DENIED_BY_USER: u8 = 2;

/// `ConsoleConflict` state: the console session is active.
pub const CONSOLE_STATE_ACTIVE: u8 = 0;
/// `ConsoleConflict` state: the console session is locked.
pub const CONSOLE_STATE_LOCKED: u8 = 1;
/// `ConsoleConflict` state: the console session is disconnected.
pub const CONSOLE_STATE_DISCONNECTED: u8 = 2;

/// `SessionProgress` stage: evicting the previous console owner
/// (disconnect/logoff can take a few seconds).
pub const SESSION_STAGE_EVICTING: u8 = 1;
/// `SessionProgress` stage: waiting for the autologon to bring the console
/// up (SAS + credential provider + shell startup).
pub const SESSION_STAGE_AUTOLOGON: u8 = 2;

impl ControlMessage {
    /// Serializes the message (postcard).
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        Ok(postcard::to_allocvec(self)?)
    }

    /// Deserializes a message; rejects trailing bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        Ok(postcard::from_bytes(bytes)?)
    }
}
