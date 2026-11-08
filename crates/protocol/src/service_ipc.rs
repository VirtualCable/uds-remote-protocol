// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

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
//!          u16   ladder tuning overrides ("k=v,..."; empty = defaults)
//!          u16   adaptive-fps overrides ("k=v,..."; empty = disabled)
//!          u16   stats CSV path (empty = telemetry to the log only)
//!          u16   webcam sink ("null"/"" = off, "raw:PATH" = E2E file)
//!          u16   audio-in sink ("null"/"" = off, "uds" = driver mic,
//!                "wav:PATH" = E2E file)
//!          u16   quality policy ("k=v,..."; empty = §12.5 defaults)
//!          u16   encoder backend (""/"auto", "mf", "software")
//!          u32   fixed resolution width  (0 = approach the client's mode)
//!          u32   fixed resolution height
//!          u8    flags (bit0 = allow_lock: client may lock the console,
//!                bit1 = wallpaper_off: blank the desktop wallpaper)
//!          u8    handoff flag (0 = none, 1 = handoff block follows)
//!          [u64  send_seq | u64 recv_seq |
//!           u16 hello_len + Hello bytes |
//!           u32 socket_info_len + opaque socket blob (WSAPROTOCOL_INFO)]
//! ```
//!
//! Pure format code only; both the service (Windows) and the agent use it.

/// Current frame version. Bump on any layout change; the agent rejects
/// mismatches (the service and agent ship together, so there is no compat).
pub const AGENT_CONFIG_VERSION: u8 = 13;

/// Session key length in bytes (matches `crypt`/`protocol`).
pub const KEY_LEN: usize = 32;
/// Session ticket length in bytes (matches `protocol`).
pub const TICKET_LEN: usize = 48;

/// Maximum accepted length for any string field (defensive bound; the
/// quality policy wire form carries the whole level table, §18.69).
pub const MAX_STRING_LEN: usize = 2048;

/// Maximum accepted length for the opaque socket-info blob
/// (WSAPROTOCOL_INFO is ~400 bytes; bound generously).
pub const MAX_SOCKET_INFO_LEN: usize = 4096;

/// Minimum frame size: version + key + ticket + the u16 string lengths
/// (all strings empty) + fixed resolution + flags + the handoff flag.
const HEADER_LEN: usize = 1 + KEY_LEN + TICKET_LEN + 9 * 2 + 8 + 1 + 1;

/// Offset where the length-prefixed string fields start.
const STRINGS_OFF: usize = 1 + KEY_LEN + TICKET_LEN;

/// Socket-handoff state for an agent that continues a connection the
/// service already validated (§18.32): the connected socket (as an opaque
/// `WSAPROTOCOL_INFO` blob on Windows), the stream seqs after the
/// handshake, and the peer's Hello.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffInfo {
    /// Next outgoing (server → client) stream seq.
    pub send_seq: u64,
    /// Next expected incoming (client → server) stream seq.
    pub recv_seq: u64,
    /// Encoded peer Hello (`protocol::Hello::ENCODED_LEN` bytes).
    pub hello: Vec<u8>,
    /// Opaque duplicated-socket blob (`WSAPROTOCOL_INFO` on Windows).
    pub socket_info: Vec<u8>,
}

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
    /// Codec-ladder sensitivity overrides in compact `key=value,...` form;
    /// empty keeps the agent's balanced defaults.
    pub ladder: String,
    /// Adaptive-fps overrides in compact `key=value,...` form; empty
    /// disables the adapter (the capture rate stays at the configured fps).
    pub fps_adapt: String,
    /// Path of the per-session telemetry CSV; empty disables it (telemetry
    /// then only goes to the log every STATS_LOG_INTERVAL).
    pub stats_csv: String,
    /// Webcam sink (§18.57): empty or "null" keeps webcam redirection off;
    /// "raw:PATH" records decoded frames for E2E verification.
    pub cam_sink: String,
    /// Audio-in sink (§9.2): empty or "null" keeps the client microphone
    /// off; "uds" feeds the udsaudio driver's virtual microphone, "wav:PATH"
    /// records decoded PCM for E2E verification.
    pub mic_sink: String,
    /// Quality policy in compact `key=value,...` wire form (§18.69): the
    /// `[quality]` config section serialized (ceilings, floors, weights and
    /// the level table). Empty keeps the agent's §12.5 defaults.
    pub quality: String,
    /// VP9 encoder backend selector: "" or "auto" picks Media Foundation
    /// when available and falls back to libvpx, "mf" forces MF (falls back
    /// with a warning), "software" forces libvpx.
    pub encoder: String,
    /// Server-fixed console resolution (§18.37); `(0, 0)` means the agent
    /// approaches the client's preferred mode instead.
    pub fixed_resolution: (u32, u32),
    /// `true` lets the client lock the remote console (`LockRequest`);
    /// server-side policy is mandatory — the client cannot override it.
    pub allow_lock: bool,
    /// `true` blanks the desktop wallpaper for the session (§18.54; the
    /// shell-animation reduction itself is unconditional, this is the only
    /// opt-in because it is visually invasive). Restored on session end.
    pub wallpaper_off: bool,
    /// Validated-connection handoff: when present, the agent must not bind
    /// its own listener; it rebuilds the connected socket and continues the
    /// stream from these seqs.
    pub handoff: Option<HandoffInfo>,
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
        HEADER_LEN
            + config.bind.len()
            + config.output_name.len()
            + config.preset.len()
            + config.ladder.len()
            + config.fps_adapt.len()
            + config.stats_csv.len()
            + config.cam_sink.len()
            + config.mic_sink.len()
            + config.quality.len()
            + config.encoder.len(),
    );
    out.push(AGENT_CONFIG_VERSION);
    out.extend_from_slice(&config.key);
    out.extend_from_slice(&config.ticket);
    for s in [
        &config.bind,
        &config.output_name,
        &config.preset,
        &config.ladder,
        &config.fps_adapt,
        &config.stats_csv,
        &config.cam_sink,
        &config.mic_sink,
        &config.quality,
        &config.encoder,
    ] {
        let len = u16::try_from(s.len()).expect("string fields are bounded well below u16::MAX");
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    out.extend_from_slice(&config.fixed_resolution.0.to_le_bytes());
    out.extend_from_slice(&config.fixed_resolution.1.to_le_bytes());
    let flags = u8::from(config.allow_lock) | (u8::from(config.wallpaper_off) << 1);
    out.push(flags);
    match &config.handoff {
        None => out.push(0),
        Some(h) => {
            out.push(1);
            out.extend_from_slice(&h.send_seq.to_le_bytes());
            out.extend_from_slice(&h.recv_seq.to_le_bytes());
            let hello_len = u16::try_from(h.hello.len()).expect("hello is a fixed-size struct");
            out.extend_from_slice(&hello_len.to_le_bytes());
            out.extend_from_slice(&h.hello);
            let sock_len = u32::try_from(h.socket_info.len()).expect("socket info is bounded");
            out.extend_from_slice(&sock_len.to_le_bytes());
            out.extend_from_slice(&h.socket_info);
        }
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
    let mut strings = ["", "", "", "", "", "", "", "", "", ""];
    for slot in &mut strings {
        let len = u16::from_le_bytes([rest[0], rest[1]]) as usize;
        rest = &rest[2..];
        if len > MAX_STRING_LEN || rest.len() < len {
            return Err(DecodeError::BadLength);
        }
        *slot = core::str::from_utf8(&rest[..len]).map_err(|_| DecodeError::BadUtf8)?;
        rest = &rest[len..];
    }
    if rest.len() < 10 {
        return Err(DecodeError::Truncated);
    }
    let fixed_resolution = (
        u32::from_le_bytes(rest[..4].try_into().expect("4-byte slice")),
        u32::from_le_bytes(rest[4..8].try_into().expect("4-byte slice")),
    );
    let allow_lock = rest[8] & 1 != 0;
    let wallpaper_off = rest[8] & 2 != 0;
    rest = &rest[9..];
    let handoff = match rest[0] {
        0 => {
            if rest.len() != 1 {
                return Err(DecodeError::BadLength);
            }
            None
        }
        1 => {
            rest = &rest[1..];
            if rest.len() < 16 + 2 {
                return Err(DecodeError::Truncated);
            }
            let send_seq = u64::from_le_bytes(rest[..8].try_into().expect("8-byte slice"));
            let recv_seq = u64::from_le_bytes(rest[8..16].try_into().expect("8-byte slice"));
            rest = &rest[16..];
            let hello_len = u16::from_le_bytes([rest[0], rest[1]]) as usize;
            rest = &rest[2..];
            if hello_len > MAX_STRING_LEN || rest.len() < hello_len + 4 {
                return Err(DecodeError::BadLength);
            }
            let hello = rest[..hello_len].to_vec();
            rest = &rest[hello_len..];
            let sock_len = u32::from_le_bytes(rest[..4].try_into().expect("4-byte slice")) as usize;
            rest = &rest[4..];
            if sock_len > MAX_SOCKET_INFO_LEN || rest.len() != sock_len {
                return Err(DecodeError::BadLength);
            }
            let socket_info = rest[..sock_len].to_vec();
            rest = &rest[sock_len..];
            if !rest.is_empty() {
                return Err(DecodeError::BadLength);
            }
            Some(HandoffInfo {
                send_seq,
                recv_seq,
                hello,
                socket_info,
            })
        }
        _ => return Err(DecodeError::BadLength),
    };
    let [bind, output_name, preset, ladder, fps_adapt, stats_csv, cam_sink, mic_sink, quality, encoder] =
        strings;
    Ok(AgentConfig {
        key,
        ticket,
        bind: bind.to_owned(),
        output_name: output_name.to_owned(),
        preset: preset.to_owned(),
        ladder: ladder.to_owned(),
        fps_adapt: fps_adapt.to_owned(),
        stats_csv: stats_csv.to_owned(),
        cam_sink: cam_sink.to_owned(),
        mic_sink: mic_sink.to_owned(),
        quality: quality.to_owned(),
        encoder: encoder.to_owned(),
        fixed_resolution,
        allow_lock,
        wallpaper_off,
        handoff,
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
            ladder: "up=15,hold_up_ms=1500".to_owned(),
            fps_adapt: "min_fps=8".to_owned(),
            stats_csv: String::new(),
            cam_sink: String::new(),
            mic_sink: String::new(),
            quality: "wc=0.2,wcpu=0.3,wbw=0.5,levels=1.0:30:1.0:80:8;0.5:30:0.75:75:8".to_owned(),
            encoder: "auto".to_owned(),
            fixed_resolution: (1280, 720),
            allow_lock: true,
            wallpaper_off: false,
            handoff: None,
        }
    }

    #[test]
    fn roundtrip() {
        let config = sample();
        let frame = encode_agent_config(&config);
        assert_eq!(decode_agent_config(&frame), Ok(config));
    }

    #[test]
    fn roundtrip_with_handoff() {
        let config = AgentConfig {
            handoff: Some(HandoffInfo {
                send_seq: 3,
                recv_seq: 2,
                hello: vec![0xAB; 64],
                socket_info: vec![0xCD; 400],
            }),
            ..sample()
        };
        let frame = encode_agent_config(&config);
        assert_eq!(decode_agent_config(&frame), Ok(config));
    }

    #[test]
    fn rejects_handoff_garbage() {
        // Flag byte 2 is not a valid handoff marker.
        let frame = encode_agent_config(&sample());
        let mut frame = frame;
        let n = frame.len();
        frame[n - 1] = 2;
        assert_eq!(decode_agent_config(&frame), Err(DecodeError::BadLength));
        // Flag 1 but truncated block.
        let full = encode_agent_config(&AgentConfig {
            handoff: Some(HandoffInfo {
                send_seq: 3,
                recv_seq: 2,
                hello: vec![0xAB; 64],
                socket_info: vec![0xCD; 400],
            }),
            ..sample()
        });
        let n = full.len();
        assert_eq!(
            decode_agent_config(&full[..n - 5]),
            Err(DecodeError::BadLength)
        );
        // Truncated right after the flag.
        let flag_at = encode_agent_config(&sample()).len() - 1;
        assert_eq!(
            decode_agent_config(&full[..=flag_at]),
            Err(DecodeError::Truncated)
        );
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
