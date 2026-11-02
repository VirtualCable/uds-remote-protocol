// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! IPC protocol between the UDS virtual smart card reader driver
//! (`udsscard`, UMDF) and its userland counterpart (the UDS-RP service, or
//! the `udsscard-ctl` test harness).
//!
//! Two named pipes, both created by the userland side:
//!
//! * **Command pipe** (`CMD_PIPE_NAME`): userland -> driver, fire-and-forget
//!   card lifecycle commands (inject/eject).
//! * **IO pipe** (`IO_PIPE_NAME`): driver -> userland requests with a
//!   synchronous reply per request (APDU transmit). The driver serializes
//!   requests with a mutex, so at most one is in flight.
//!
//! Wire format (both pipes): one byte tag, little-endian u32 payload length,
//! then payload bytes.

use std::io::{self, Read, Write};

/// Pipe carrying card lifecycle commands to the driver.
pub const CMD_PIPE_NAME: &str = r"\\.\pipe\uds-rp-scard-cmd";
/// Pipe carrying driver-originated IO requests (transmit) to userland.
pub const IO_PIPE_NAME: &str = r"\\.\pipe\uds-rp-scard-io";

/// Maximum frame payload (an APDU exchange never exceeds a few KiB).
pub const MAX_PAYLOAD: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Command pipe (userland -> driver)
// ---------------------------------------------------------------------------

/// `InjectCard`: payload is the card ATR.
pub const CMD_INJECT_CARD: u8 = 1;
/// `EjectCard`: empty payload.
pub const CMD_EJECT_CARD: u8 = 2;

/// Card lifecycle command decoded from the command pipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Card inserted; payload is its ATR.
    InjectCard(Vec<u8>),
    /// Card removed.
    EjectCard,
}

impl Command {
    /// Parse a command frame. Returns `None` for unknown tags.
    #[must_use]
    pub fn parse(tag: u8, payload: Vec<u8>) -> Option<Self> {
        match tag {
            CMD_INJECT_CARD => Some(Self::InjectCard(payload)),
            CMD_EJECT_CARD => Some(Self::EjectCard),
            _ => None,
        }
    }

    /// Frame tag for this command.
    #[must_use]
    pub const fn tag(&self) -> u8 {
        match self {
            Self::InjectCard(_) => CMD_INJECT_CARD,
            Self::EjectCard => CMD_EJECT_CARD,
        }
    }

    /// Frame payload for this command.
    #[must_use]
    pub fn payload(&self) -> Vec<u8> {
        match self {
            Self::InjectCard(atr) => atr.clone(),
            Self::EjectCard => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// IO pipe (driver -> userland, request/reply)
// ---------------------------------------------------------------------------

/// `Transmit` request: payload is the raw APDU as received from SCardSvr
/// (including the trailing `Le` byte if present; the `SCARD_IO_REQUEST`
/// protocol header is NOT included — the driver strips it and reports the
/// negotiated protocol separately if ever needed).
pub const IO_TRANSMIT: u8 = 1;

/// Successful transmit reply: payload is the full card response, including
/// the trailing SW1 SW2.
pub const IO_RESP_OK: u8 = 1;
/// Failed transmit reply: payload is a little-endian u32 SCARD error code
/// (e.g. `SCARD_E_NO_SMARTCARD`), propagated back to the caller of the
/// driver IOCTL as an error.
pub const IO_RESP_ERROR: u8 = 2;

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// Encode one frame (`tag` + LE length + payload).
#[must_use]
pub fn encode_frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(tag);
    let len = u32::try_from(payload.len()).expect("payload fits u32");
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Write one frame to `w`.
pub fn write_frame(w: &mut impl Write, tag: u8, payload: &[u8]) -> io::Result<()> {
    w.write_all(&encode_frame(tag, payload))?;
    w.flush()
}

/// Read one frame from `r`. `UnexpectedEof` on clean peer shutdown.
pub fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 5];
    r.read_exact(&mut hdr)?;
    let tag = hdr[0];
    let len = u32::from_le_bytes(hdr[1..5].try_into().expect("4 bytes")) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((tag, payload))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, IO_TRANSMIT, &[0x00, 0xA4, 0x04, 0x00]).unwrap();
        let mut cur = io::Cursor::new(buf);
        let (tag, payload) = read_frame(&mut cur).unwrap();
        assert_eq!(tag, IO_TRANSMIT);
        assert_eq!(payload, vec![0x00, 0xA4, 0x04, 0x00]);
    }

    #[test]
    fn empty_payload_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, CMD_EJECT_CARD, &[]).unwrap();
        let mut cur = io::Cursor::new(buf);
        let (tag, payload) = read_frame(&mut cur).unwrap();
        assert_eq!(tag, CMD_EJECT_CARD);
        assert_eq!(payload, Vec::<u8>::new());
    }

    #[test]
    fn command_parse() {
        assert_eq!(
            Command::parse(CMD_INJECT_CARD, vec![0x3B, 0x00]),
            Some(Command::InjectCard(vec![0x3B, 0x00]))
        );
        assert_eq!(Command::parse(CMD_EJECT_CARD, vec![]), Some(Command::EjectCard));
        assert_eq!(Command::parse(0xEE, vec![]), None);
    }

    #[test]
    fn oversized_frame_rejected() {
        let mut buf = vec![0u8, 0xFF, 0xFF, 0xFF, 0xFF];
        buf.extend_from_slice(&[0u8; 16]);
        let mut cur = io::Cursor::new(buf);
        assert!(read_frame(&mut cur).is_err());
    }

    #[test]
    fn truncated_frame_rejected() {
        let mut cur = io::Cursor::new(vec![1u8, 4, 0, 0, 0, 0xAA]);
        assert!(read_frame(&mut cur).is_err());
    }
}
