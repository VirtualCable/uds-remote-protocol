//! Wire format of the configuration frame the orchestrator service hands to
//! the session agent (server) it spawns.
//!
//! The frame travels over an anonymous pipe connected to the agent's stdin:
//! it carries the session secrets (key + ticket), which must never appear in
//! the agent's command line (any process in the session can read argv).
//!
//! Layout (little-endian, single frame, read until EOF):
//!
//! ```text
//! [0]      u8    AGENT_CONFIG_VERSION
//! [1..33]  [u8; 32] session key
//! [33..81] [u8; 48] session ticket
//! [81..83] u16   bind address length, then UTF-8 bytes (e.g. "0.0.0.0:14433")
//!          u16   output selector (display name substring), then UTF-8 bytes
//!          u16   codec preset name, then UTF-8 bytes
//! ```
//!
//! Pure format code only; both the service (Windows) and the agent use it.

/// Current frame version. Bump on any layout change; the agent rejects
/// mismatches (the service and agent ship together, so there is no compat).
pub const AGENT_CONFIG_VERSION: u8 = 1;

/// Session key length in bytes (matches `crypt`/`protocol`).
pub const KEY_LEN: usize = 32;
/// Session ticket length in bytes (matches `protocol`).
pub const TICKET_LEN: usize = 48;

/// Maximum accepted length for any string field (defensive bound).
pub const MAX_STRING_LEN: usize = 256;

/// Minimum frame size: version + key + ticket + three u16 string lengths
/// (all strings empty).
const HEADER_LEN: usize = 1 + KEY_LEN + TICKET_LEN + 3 * 2;

/// Offset where the length-prefixed string fields start.
const STRINGS_OFF: usize = 1 + KEY_LEN + TICKET_LEN;

/// Everything the service passes to the agent it spawns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    /// Shared session key (PSK) for the crypto layer.
    pub key: [u8; KEY_LEN],
    /// Session ticket clients must present in the handshake.
    pub ticket: [u8; TICKET_LEN],
    /// Listen address of the agent, e.g. `0.0.0.0:14433`.
    pub bind: String,
    /// Display selector: substring of the output/monitor to capture.
    pub output_name: String,
    /// Video codec preset name (see `codec::PRESETS`).
    pub preset: String,
}

/// Decode failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Fewer bytes than the fixed header.
    Truncated,
    /// Version byte does not match [`AGENT_CONFIG_VERSION`].
    VersionMismatch,
    /// A string field overruns the frame or [`MAX_STRING_LEN`].
    BadLength,
    /// A string field is not valid UTF-8.
    BadUtf8,
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let msg = match self {
            Self::Truncated => "frame shorter than the fixed header",
            Self::VersionMismatch => "agent config version mismatch",
            Self::BadLength => "string field overruns the frame",
            Self::BadUtf8 => "string field is not valid UTF-8",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for DecodeError {}

/// Encodes `config` into its wire frame.
#[must_use]
pub fn encode_agent_config(config: &AgentConfig) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        HEADER_LEN + config.bind.len() + config.output_name.len() + config.preset.len(),
    );
    out.push(AGENT_CONFIG_VERSION);
    out.extend_from_slice(&config.key);
    out.extend_from_slice(&config.ticket);
    for s in [&config.bind, &config.output_name, &config.preset] {
        let len = u16::try_from(s.len()).expect("string fields are bounded well below u16::MAX");
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    out
}

/// Decodes one frame. Trailing bytes are an error (the frame ends at EOF).
///
/// # Errors
/// [`DecodeError`] on truncation, version mismatch, bad lengths or bad UTF-8.
pub fn decode_agent_config(buf: &[u8]) -> Result<AgentConfig, DecodeError> {
    if buf.len() < HEADER_LEN {
        return Err(DecodeError::Truncated);
    }
    if buf[0] != AGENT_CONFIG_VERSION {
        return Err(DecodeError::VersionMismatch);
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&buf[1..=KEY_LEN]);
    let mut ticket = [0u8; TICKET_LEN];
    ticket.copy_from_slice(&buf[1 + KEY_LEN..1 + KEY_LEN + TICKET_LEN]);

    let mut rest = &buf[STRINGS_OFF..];
    let mut strings = ["", "", ""];
    for slot in &mut strings {
        let len = u16::from_le_bytes([rest[0], rest[1]]) as usize;
        rest = &rest[2..];
        if len > MAX_STRING_LEN || rest.len() < len {
            return Err(DecodeError::BadLength);
        }
        *slot = core::str::from_utf8(&rest[..len]).map_err(|_| DecodeError::BadUtf8)?;
        rest = &rest[len..];
    }
    if !rest.is_empty() {
        return Err(DecodeError::BadLength);
    }
    let [bind, output_name, preset] = strings;
    Ok(AgentConfig {
        key,
        ticket,
        bind: bind.to_owned(),
        output_name: output_name.to_owned(),
        preset: preset.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AgentConfig {
        AgentConfig {
            key: [7u8; KEY_LEN],
            ticket: [9u8; TICKET_LEN],
            bind: "0.0.0.0:14433".to_owned(),
            output_name: "UDS".to_owned(),
            preset: "vp9-balanced".to_owned(),
        }
    }

    #[test]
    fn roundtrip() {
        let config = sample();
        let frame = encode_agent_config(&config);
        assert_eq!(decode_agent_config(&frame), Ok(config));
    }

    #[test]
    fn rejects_truncation() {
        let frame = encode_agent_config(&sample());
        assert_eq!(decode_agent_config(&[]), Err(DecodeError::Truncated));
        assert_eq!(decode_agent_config(&frame[..HEADER_LEN - 1]), Err(DecodeError::Truncated));
    }

    #[test]
    fn rejects_version_mismatch() {
        let mut frame = encode_agent_config(&sample());
        frame[0] = AGENT_CONFIG_VERSION + 1;
        assert_eq!(decode_agent_config(&frame), Err(DecodeError::VersionMismatch));
    }

    #[test]
    fn rejects_overlong_and_trailing() {
        let mut frame = encode_agent_config(&sample());
        // Corrupt the first string length beyond the frame.
        let off = 1 + KEY_LEN + TICKET_LEN;
        frame[off] = 0xFF;
        frame[off + 1] = 0x7F;
        assert_eq!(decode_agent_config(&frame), Err(DecodeError::BadLength));
        // Trailing garbage.
        let mut frame = encode_agent_config(&sample());
        frame.push(0);
        assert_eq!(decode_agent_config(&frame), Err(DecodeError::BadLength));
    }

    #[test]
    fn rejects_bad_utf8() {
        let mut frame = encode_agent_config(&sample());
        let off = 1 + KEY_LEN + TICKET_LEN + 2; // first byte of `bind`
        frame[off] = 0xFF;
        assert_eq!(decode_agent_config(&frame), Err(DecodeError::BadUtf8));
    }
}
