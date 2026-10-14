//! Initial handshake (pre-encryption): signature + version + command + ticket.

use crate::error::ProtocolError;

/// Signature at the start of the handshake ("UDSRP/01").
pub const HANDSHAKE_SIGNATURE: [u8; 8] = *b"UDSRP/01";

/// Current protocol version. v3: the HELLO carries a flags byte (bit 0 =
/// privacy screen, M7); v2: the display fragment header carries
/// `capture_ts_ms` (§9.1 A/V lip-sync); v1 peers misparse it, so the
/// handshake rejects any version mismatch.
pub const PROTOCOL_VERSION: u16 = 3;

/// Session ticket length.
pub const TICKET_LENGTH: usize = 48;

/// HELLO flag: the client asks the server to hide the physical console
/// behind a privacy screen for the duration of the session (M7).
pub const FLAG_PRIVACY_SCREEN: u8 = 0x01;

/// Handshake commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Command {
    /// Reachability/validity probe (without opening a session).
    Test = 0,
    /// New session open.
    Open = 1,
    /// Session recovery after a drop (same ticket and seqs).
    Recover = 2,
}

impl Command {
    fn from_byte(b: u8) -> Result<Self, ProtocolError> {
        match b {
            0 => Ok(Self::Test),
            1 => Ok(Self::Open),
            2 => Ok(Self::Recover),
            other => Err(ProtocolError::UnknownCommand(other)),
        }
    }
}

/// HELLO message of the handshake (8B signature + 2B version + 1B command +
/// 1B flags + 48B ticket).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// Protocol version of the sender.
    pub version: u16,
    /// Requested command.
    pub command: Command,
    /// Request flags (bit field; see `FLAG_PRIVACY_SCREEN`).
    pub flags: u8,
    /// Session ticket issued by the broker (or pre-shared in the PoC).
    pub ticket: [u8; TICKET_LENGTH],
}

impl Hello {
    /// Wire size of the encoded message.
    pub const ENCODED_LEN: usize = 8 + 2 + 1 + 1 + TICKET_LENGTH;

    /// Encodes the message to its binary form.
    #[must_use]
    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[..8].copy_from_slice(&HANDSHAKE_SIGNATURE);
        out[8..10].copy_from_slice(&self.version.to_be_bytes());
        out[10] = self.command as u8;
        out[11] = self.flags;
        out[12..].copy_from_slice(&self.ticket);
        out
    }

    /// Decodes and validates a HELLO message.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        if bytes.len() < Self::ENCODED_LEN {
            return Err(ProtocolError::Truncated);
        }
        if bytes[..8] != HANDSHAKE_SIGNATURE {
            return Err(ProtocolError::BadSignature);
        }
        Ok(Self {
            version: u16::from_be_bytes(bytes[8..10].try_into().expect("2-byte slice")),
            command: Command::from_byte(bytes[10])?,
            flags: bytes[11],
            ticket: bytes[12..12 + TICKET_LENGTH]
                .try_into()
                .expect("48-byte slice"),
        })
    }
}
