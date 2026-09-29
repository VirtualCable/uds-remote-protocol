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
    /// Control channel heartbeat.
    Keepalive,
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
