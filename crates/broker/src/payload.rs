// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! The ticket payload schema (protocol of ours, version 1).
//!
//! The payload travels in `remotes[0].extra` of the broker's ticket
//! response (the `connection` kind of the `actor/v3` `ticket` action,
//! which mirrors the tunnel ticket contract). The per-session PSK does NOT
//! live here: it comes from the response's `shared_secret` field, which
//! the broker generates when it creates the ticket and delivers both to us
//! (actor channel) and to the client (its own ticket path).

use serde::{Deserialize, Serialize};

/// Payload schema version. Unknown versions are rejected, never parsed
/// leniently.
pub const PAYLOAD_VERSION: u32 = 1;

/// What the broker answers to `start` inside `remotes[0].extra`
/// (the session definition; the PSK arrives separately in the
/// response's `shared_secret` field).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketPayload {
    /// Schema version ([`PAYLOAD_VERSION`]).
    pub version: u32,
    /// Auto-logon credentials for the credential provider; `None` means the
    /// machine must already have an interactive session (no auto-logon).
    #[serde(default)]
    pub credentials: Option<SessionCredentials>,
    /// Display policy the session must honour; `None` = capture as-is.
    #[serde(default)]
    pub display: Option<DisplayPolicy>,
    /// Video pipeline selection; `None` = agent defaults.
    #[serde(default)]
    pub video: Option<VideoPolicy>,
    /// Which optional channels the session offers (server is mandatory:
    /// the client cannot enable what this disables).
    #[serde(default)]
    pub channels: ChannelPolicy,
    /// Blank the desktop wallpaper during the session (§18.54; the shell
    /// animation reduction is unconditional — this is the only opt-in).
    #[serde(default)]
    pub wallpaper_off: bool,
}

/// Auto-logon credentials delivered per session by the broker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCredentials {
    /// Username.
    pub user: String,
    /// Domain; empty/"." pins the logon to the local accounts.
    #[serde(default)]
    pub domain: String,
    /// Password (only ever inside the authenticated actor channel).
    pub password: String,
}

/// Display policy: requested geometry and whether the client may override
/// it (plan §18.37).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DisplayPolicy {
    /// Horizontal resolution.
    pub width: u32,
    /// Vertical resolution.
    pub height: u32,
    /// Capture frames per second.
    pub fps: u32,
    /// `true` = the server resolution is fixed; the client scales locally.
    #[serde(default)]
    pub fixed: bool,
}

/// Video pipeline selection (codec preset and tuning overrides).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoPolicy {
    /// Codec preset name (from `codec::PRESETS`), e.g. `"ladder"`.
    pub preset: Option<String>,
    /// Codec-ladder overrides in wire form (`k=v,...`).
    pub ladder: Option<String>,
    /// Adaptive-fps overrides in wire form (`k=v,...`, §12.7).
    pub adaptive_fps: Option<String>,
}

/// Which optional channels the session offers. Defaults mirror the
/// conservative standalone defaults: audio and clipboard on, the rest off.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)] // a switch bag
pub struct ChannelPolicy {
    /// Audio out (and in, when the endpoint exists).
    pub audio: bool,
    /// Clipboard bridge.
    pub clipboard: bool,
    /// File transfer spool.
    pub storage: bool,
    /// Virtual smart card reader bridge.
    pub smartcard: bool,
    /// Webcam redirection (future).
    pub webcam: bool,
    /// Printer redirection (future, §21).
    pub printer: bool,
    /// The client may ask to lock the remote console (`LockRequest`).
    pub lock: bool,
}

impl Default for ChannelPolicy {
    fn default() -> Self {
        Self {
            audio: true,
            clipboard: true,
            storage: false,
            smartcard: false,
            webcam: false,
            printer: false,
            lock: false,
        }
    }
}


impl TicketPayload {
    /// Parses a payload from its JSON form, rejecting unknown versions.
    pub fn from_json(text: &str) -> anyhow::Result<Self> {
        Self::from_value(serde_json::from_str(text)?)
    }

    /// Parses a payload from an already-decoded JSON value (the
    /// `remotes[0].extra` field of the broker response).
    pub fn from_value(value: serde_json::Value) -> anyhow::Result<Self> {
        let payload: Self = serde_json::from_value(value)?;
        anyhow::ensure!(
            payload.version == PAYLOAD_VERSION,
            "unsupported ticket payload version {} (expected {PAYLOAD_VERSION})",
            payload.version
        );
        Ok(payload)
    }
}

/// A validated session: the PSK ready to use plus the parsed payload.
#[derive(Debug, Clone)]
pub struct SessionGrant {
    /// Per-session pre-shared key (the broker response's `shared_secret`,
    /// or the configured `shared_key` in standalone mode).
    pub key: [u8; 32],
    /// Auto-logon credentials, when the session defines them.
    pub credentials: Option<SessionCredentials>,
    /// Display policy (`None` = capture as-is).
    pub display: Option<DisplayPolicy>,
    /// Video pipeline selection (`None` = agent defaults).
    pub video: Option<VideoPolicy>,
    /// Channel switches.
    pub channels: ChannelPolicy,
    /// Blank the desktop wallpaper during the session (§18.54).
    pub wallpaper_off: bool,
    /// Stop-notification ticket (`None` = no stop notification).
    pub notify: Option<String>,
}

impl SessionGrant {
    /// Builds a grant from a payload plus the session PSK.
    #[must_use]
    pub fn from_payload(payload: TicketPayload, key: [u8; 32], notify: Option<String>) -> Self {
        Self {
            key,
            credentials: payload.credentials,
            display: payload.display,
            video: payload.video,
            channels: payload.channels,
            wallpaper_off: payload.wallpaper_off,
            notify,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_roundtrip() {
        let json = r#"{"version": 1,
                "credentials": {"user": "u", "domain": "d", "password": "p"},
                "channels": {"audio": true, "clipboard": false, "storage": true,
                              "smartcard": false, "webcam": true, "printer": false}}"#;
        let payload = TicketPayload::from_json(json).unwrap();
        assert!(payload.credentials.is_some());
        assert!(payload.channels.storage);
        let grant = SessionGrant::from_payload(payload, [0x73; 32], Some("stop-ticket".into()));
        assert_eq!(grant.key[0], 0x73);
        assert_eq!(grant.notify.as_deref(), Some("stop-ticket"));
    }

    #[test]
    fn payload_rejects_unknown_version() {
        assert!(TicketPayload::from_json(r#"{"version": 99}"#).is_err());
    }

    #[test]
    fn payload_defaults_are_conservative() {
        let payload = TicketPayload::from_json(r#"{"version": 1}"#).unwrap();
        assert!(payload.credentials.is_none());
        assert!(payload.channels.audio);
        assert!(!payload.channels.smartcard);
    }
}
