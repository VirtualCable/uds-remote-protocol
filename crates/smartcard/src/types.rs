// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Opaque handles, auxiliary types and small helpers.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::consts::{SCARD_PROTOCOL_T0, SCARD_PROTOCOL_T1};

// ---------------------------------------------------------------------------
// Handle generation
// ---------------------------------------------------------------------------

static CONTEXT_COUNTER: AtomicU64 = AtomicU64::new(1);
static HANDLE_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_nonzero(counter: &AtomicU64) -> u64 {
    loop {
        let id = counter.fetch_add(1, Ordering::Relaxed);
        if id != 0 {
            return id;
        }
        // 0 is reserved (SCARD_E_INVALID_HANDLE in some implementations)
    }
}

// ---------------------------------------------------------------------------
// Opaque handles
// ---------------------------------------------------------------------------

/// Opaque handle for an established smartcard context (`SCARDCONTEXT`).
///
/// Handles are generated on the side that owns the backend (the client)
/// and echoed by the peer in later requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScardContext(u64);

impl Default for ScardContext {
    fn default() -> Self {
        Self::new()
    }
}

impl ScardContext {
    /// Create a context with a unique, non-zero id.
    #[must_use]
    pub fn new() -> Self {
        Self(next_nonzero(&CONTEXT_COUNTER))
    }

    /// Wrap a raw id received on the wire.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Raw id.
    #[must_use]
    pub const fn raw(&self) -> u64 {
        self.0
    }
}

/// Opaque handle for a connection to a specific card (`SCARDHANDLE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScardHandle {
    handle: u64,
    active_protocol: u32,
}

impl ScardHandle {
    /// Create a handle with a unique id for the negotiated protocol.
    #[must_use]
    pub fn new(active_protocol: u32) -> Self {
        Self {
            handle: next_nonzero(&HANDLE_COUNTER),
            active_protocol,
        }
    }

    /// Wrap raw values received on the wire.
    #[must_use]
    pub const fn from_raw(handle: u64, active_protocol: u32) -> Self {
        Self {
            handle,
            active_protocol,
        }
    }

    /// Raw handle id.
    #[must_use]
    pub const fn raw(&self) -> u64 {
        self.handle
    }

    /// Protocol negotiated during `connect`/`reconnect`.
    #[must_use]
    pub const fn active_protocol(&self) -> u32 {
        self.active_protocol
    }
}

// ---------------------------------------------------------------------------
// Auxiliary types
// ---------------------------------------------------------------------------

/// I/O request parameters for `transmit` (`SCARD_IO_REQUEST`).
#[derive(Debug, Clone)]
pub struct ScardIORequest {
    /// `SCARD_PROTOCOL_*` value.
    pub protocol: u32,
    /// Extra PCI bytes (normally empty).
    pub extra_bytes: Vec<u8>,
}

impl ScardIORequest {
    /// T=0 (byte-oriented).
    #[must_use]
    pub fn t0() -> Self {
        Self {
            protocol: SCARD_PROTOCOL_T0,
            extra_bytes: Vec::new(),
        }
    }

    /// T=1 (block-oriented).
    #[must_use]
    pub fn t1() -> Self {
        Self {
            protocol: SCARD_PROTOCOL_T1,
            extra_bytes: Vec::new(),
        }
    }
}

/// Result of a `transmit`.
#[derive(Debug, Clone)]
pub struct TransmitResult {
    /// I/O request returned by the card (may differ from the send PCI).
    pub recv_pci: Option<ScardIORequest>,
    /// Card response bytes, including the trailing SW1 SW2.
    pub recv_buffer: Vec<u8>,
}

/// Status of a connected card.
#[derive(Debug, Clone)]
pub struct ScardStatus {
    /// Reader names associated with the card.
    pub reader_names: Vec<String>,
    /// `SCARD_STATE_*` flags.
    pub state: u32,
    /// Active `SCARD_PROTOCOL_*` value.
    pub protocol: u32,
    /// Answer To Reset of the card.
    pub atr: Vec<u8>,
}

/// Input state for `get_status_change`: the reader state the caller last
/// saw.
#[derive(Debug, Clone)]
pub struct ReaderStateIn {
    /// Reader to watch.
    pub reader_name: String,
    /// State known by the caller (`SCARD_STATE_*`).
    pub current_state: u32,
}

/// Output state of `get_status_change`.
#[derive(Debug, Clone)]
pub struct ReaderStateOut {
    /// Reader name.
    pub reader_name: String,
    /// State known by the caller (echoed from the input).
    pub current_state: u32,
    /// State that triggered the return (`SCARD_STATE_*`).
    pub event_state: u32,
    /// Card ATR when present.
    pub atr: Vec<u8>,
}

/// Result of `connect` / `reconnect`.
#[derive(Debug, Clone)]
pub struct ConnectResult {
    /// Handle to the connected card.
    pub handle: ScardHandle,
    /// Negotiated protocol.
    pub active_protocol: u32,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compare a card ATR against a pattern + mask:
/// `(atr[i] & mask[i]) == (pattern[i] & mask[i])` for every byte.
#[must_use]
pub fn match_atr(atr: &[u8], pattern: &[u8], mask: &[u8]) -> bool {
    if atr.len() < pattern.len() || pattern.len() != mask.len() {
        return false;
    }
    atr.iter()
        .zip(pattern.iter())
        .zip(mask.iter())
        .all(|((a, p), m)| (a & m) == (p & m))
}

/// Substring-based reader name filter (case-insensitive).
#[must_use]
pub fn filter_reader(filter: Option<&str>, reader: &str) -> bool {
    match filter {
        None | Some("") => true,
        Some(f) => reader.to_lowercase().contains(&f.to_lowercase()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_are_unique_and_nonzero() {
        let a = ScardContext::new();
        let b = ScardContext::new();
        assert_ne!(a, b);
        assert_ne!(a.raw(), 0);
        let h = ScardHandle::new(SCARD_PROTOCOL_T1);
        assert_ne!(h.raw(), 0);
        assert_eq!(h.active_protocol(), SCARD_PROTOCOL_T1);
    }

    #[test]
    fn atr_matching() {
        let atr = [0x3B, 0x89, 0x01, 0x45];
        assert!(match_atr(&atr, &[0x3B, 0x89], &[0xFF, 0xFF]));
        assert!(!match_atr(&atr, &[0x3B, 0x88], &[0xFF, 0xFF]));
        assert!(match_atr(&atr, &[0x00, 0x89], &[0x00, 0xFF]));
        assert!(!match_atr(&atr, &[0x3B], &[0xFF, 0xFF])); // len mismatch
        assert!(!match_atr(&atr[..1], &[0x3B, 0x89], &[0xFF, 0xFF]));
    }

    #[test]
    fn reader_filter() {
        assert!(filter_reader(None, "ACS ACR39U 00 00"));
        assert!(filter_reader(Some(""), "ACS ACR39U 00 00"));
        assert!(filter_reader(Some("acr39"), "ACS ACR39U 00 00"));
        assert!(!filter_reader(Some("omnikey"), "ACS ACR39U 00 00"));
    }
}
