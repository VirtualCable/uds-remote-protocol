// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Hex encoding/decoding helpers for CLI tooling (tickets, keys).
//!
//! Lowercase, no separators, fixed output length (`2 * N` chars for `N`
//! bytes). Decoding rejects odd lengths, non-hex characters and wrong
//! lengths.

use crate::error::ProtocolError;

/// Encodes bytes as lowercase hex.
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0F) as usize] as char);
    }
    out
}

/// Decodes a hex string into exactly `N` bytes.
///
/// # Errors
/// Returns [`ProtocolError::Malformed`] on odd length, non-hex characters
/// or a decoded length different from `N`.
pub fn decode<const N: usize>(s: &str) -> Result<[u8; N], ProtocolError> {
    if s.len() != N * 2 {
        return Err(ProtocolError::Malformed("wrong hex length"));
    }
    let mut out = [0u8; N];
    for (i, &[hi_c, lo_c]) in s.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        out[i] = (hex_digit(hi_c)? << 4) | hex_digit(lo_c)?;
    }
    Ok(out)
}

fn hex_digit(c: u8) -> Result<u8, ProtocolError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(ProtocolError::Malformed("non-hex character")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_is_lowercase_and_doubles_length() {
        assert_eq!(encode(&[0x00, 0xAB, 0xff]), "00abff");
        assert_eq!(encode(&[]), "");
    }

    #[test]
    fn decode_roundtrips() {
        let original = [0x42u8; 48];
        let s = encode(&original);
        assert_eq!(s.len(), 96);
        assert_eq!(decode::<48>(&s).expect("valid hex"), original);
    }

    #[test]
    fn decode_accepts_uppercase() {
        assert_eq!(decode::<2>("ABff").expect("valid hex"), [0xAB, 0xff]);
    }

    #[test]
    fn decode_rejects_bad_input() {
        assert!(decode::<2>("abc").is_err()); // odd length
        assert!(decode::<2>("zzzz").is_err()); // non-hex
        assert!(decode::<3>("aabb").is_err()); // wrong length
        assert!(decode::<1>("").is_err()); // empty
    }
}
