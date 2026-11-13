// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Wire mapping of broker ticket strings.
//!
//! Broker (pq) tickets are 48-character alphanumeric strings that travel
//! raw (UTF-8, NUL-padded) in the 48-byte ticket field of the Hello.
//! Standalone tickets are human strings mapped with SHA-384 instead (see
//! `server_config::ticket_bytes`); the two forms never mix: a pq endpoint
//! rejects binary tickets and vice versa.

use crate::TICKET_LENGTH;

/// Maps a broker ticket string to its wire form: UTF-8 bytes NUL-padded
/// to [`TICKET_LENGTH`].
pub fn wire_ticket_from_str(ticket: &str) -> Result<[u8; TICKET_LENGTH], crate::ProtocolError> {
    let bytes = ticket.as_bytes();
    if bytes.len() > TICKET_LENGTH {
        return Err(crate::ProtocolError::Malformed(
            "broker ticket too long for the wire field",
        ));
    }
    let mut out = [0u8; TICKET_LENGTH];
    out[..bytes.len()].copy_from_slice(bytes);
    Ok(out)
}

/// Inverse of [`wire_ticket_from_str`]: UTF-8 up to the first NUL.
pub fn str_from_wire_ticket(ticket: &[u8; TICKET_LENGTH]) -> Result<String, crate::ProtocolError> {
    let end = ticket.iter().position(|b| *b == 0).unwrap_or(ticket.len());
    let s = std::str::from_utf8(&ticket[..end])
        .map_err(|_| crate::ProtocolError::Malformed("wire ticket is not a UTF-8 broker ticket"))?;
    if s.is_empty() {
        return Err(crate::ProtocolError::Malformed("empty wire ticket"));
    }
    Ok(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_ticket_roundtrip() {
        let ticket = "a".repeat(TICKET_LENGTH);
        let wire = wire_ticket_from_str(&ticket).unwrap();
        assert_eq!(str_from_wire_ticket(&wire).unwrap(), ticket);
    }

    #[test]
    fn wire_ticket_rejects_overlong() {
        assert!(wire_ticket_from_str(&"x".repeat(TICKET_LENGTH + 1)).is_err());
    }

    #[test]
    fn str_from_wire_rejects_binary_and_empty() {
        assert!(str_from_wire_ticket(&[0xff; TICKET_LENGTH]).is_err());
        assert!(str_from_wire_ticket(&[0; TICKET_LENGTH]).is_err());
    }
}
