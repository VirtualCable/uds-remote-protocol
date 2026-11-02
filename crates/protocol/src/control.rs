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
}

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
