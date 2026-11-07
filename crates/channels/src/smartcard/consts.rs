// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! PC/SC protocol and error constants (SCARD_*).

// ---------------------------------------------------------------------------
// Protocol constants
// ---------------------------------------------------------------------------

/// Scope: operations apply to the terminal (`SCARD_SCOPE_TERMINAL`).
pub const SCARD_SCOPE_TERMINAL: u32 = 1;
/// Scope: operations apply to the system (`SCARD_SCOPE_SYSTEM`).
pub const SCARD_SCOPE_SYSTEM: u32 = 2;

/// Protocol T=0 (byte-oriented half-duplex).
pub const SCARD_PROTOCOL_T0: u32 = 0x0000_0001;
/// Protocol T=1 (block-oriented half-duplex).
pub const SCARD_PROTOCOL_T1: u32 = 0x0000_0002;
/// Raw protocol (vendor specific).
pub const SCARD_PROTOCOL_RAW: u32 = 0x0001_0000;

/// Share mode: exclusive access.
pub const SCARD_SHARE_EXCLUSIVE: u32 = 1;
/// Share mode: shared access.
pub const SCARD_SHARE_SHARED: u32 = 2;
/// Share mode: direct access (no card required).
pub const SCARD_SHARE_DIRECT: u32 = 3;

/// Card disposition: leave the card as is.
pub const SCARD_LEAVE_CARD: u32 = 0;
/// Card disposition: reset the card.
pub const SCARD_RESET_CARD: u32 = 1;
/// Card disposition: power down the card.
pub const SCARD_UNPOWER_CARD: u32 = 2;
/// Card disposition: eject the card.
pub const SCARD_EJECT_CARD: u32 = 3;

// Reader state flags (SCARD_STATE_*)
/// No prior knowledge of the reader state.
pub const SCARD_STATE_UNAWARE: u32 = 0x0000_0000;
/// Ignore this reader.
pub const SCARD_STATE_IGNORE: u32 = 0x0000_0001;
/// State changed since last reported.
pub const SCARD_STATE_CHANGED: u32 = 0x0000_0002;
/// Reader unknown to the system.
pub const SCARD_STATE_UNKNOWN: u32 = 0x0000_0004;
/// Reader unavailable.
pub const SCARD_STATE_UNAVAILABLE: u32 = 0x0000_0008;
/// Reader has no card.
pub const SCARD_STATE_EMPTY: u32 = 0x0000_0010;
/// Reader has a card.
pub const SCARD_STATE_PRESENT: u32 = 0x0000_0020;
/// Card ATR matches a watched pattern.
pub const SCARD_STATE_ATRMATCH: u32 = 0x0000_0040;
/// Card is allocated to another context exclusively.
pub const SCARD_STATE_EXCLUSIVE: u32 = 0x0000_0080;
/// Card is in use by other connections.
pub const SCARD_STATE_INUSE: u32 = 0x0000_0100;
/// Card is unresponsive.
pub const SCARD_STATE_MUTE: u32 = 0x0000_0200;
/// Card is not powered.
pub const SCARD_STATE_UNPOWERED: u32 = 0x0000_0400;

// ---------------------------------------------------------------------------
// SCARD error codes (PC/SC)
// ---------------------------------------------------------------------------

/// Success.
pub const SCARD_S_SUCCESS: u32 = 0x0000_0000;
/// Internal error.
pub const SCARD_F_INTERNAL_ERROR: u32 = 0x8010_0001;
/// Operation cancelled (`SCardCancel`).
pub const SCARD_E_CANCELLED: u32 = 0x8010_0002;
/// Invalid handle.
pub const SCARD_E_INVALID_HANDLE: u32 = 0x8010_0003;
/// Invalid parameter.
pub const SCARD_E_INVALID_PARAMETER: u32 = 0x8010_0004;
/// Invalid target.
pub const SCARD_E_INVALID_TARGET: u32 = 0x8010_0005;
/// Out of memory.
pub const SCARD_E_NO_MEMORY: u32 = 0x8010_0006;
/// Buffer too small.
pub const SCARD_E_INSUFFICIENT_BUFFER: u32 = 0x8010_0008;
/// Unknown reader.
pub const SCARD_E_UNKNOWN_READER: u32 = 0x8010_0009;
/// Timeout.
pub const SCARD_E_TIMEOUT: u32 = 0x8010_000A;
/// Sharing violation.
pub const SCARD_E_SHARING_VIOLATION: u32 = 0x8010_000B;
/// No smartcard in the reader.
pub const SCARD_E_NO_SMARTCARD: u32 = 0x8010_000C;
/// Unknown card.
pub const SCARD_E_UNKNOWN_CARD: u32 = 0x8010_000D;
/// Protocol mismatch.
pub const SCARD_E_PROTO_MISMATCH: u32 = 0x8010_000F;
/// Reader/card not ready.
pub const SCARD_E_NOT_READY: u32 = 0x8010_0010;
/// Invalid value.
pub const SCARD_E_INVALID_VALUE: u32 = 0x8010_0011;
/// Cancelled by the system.
pub const SCARD_E_SYSTEM_CANCELLED: u32 = 0x8010_0012;
/// Communication error.
pub const SCARD_E_COMM_ERROR: u32 = 0x8010_0013;
/// Unknown error.
pub const SCARD_F_UNKNOWN_ERROR: u32 = 0x8010_0014;
/// No transaction in progress.
pub const SCARD_E_NOT_TRANSACTED: u32 = 0x8010_0016;
/// Reader unavailable.
pub const SCARD_E_READER_UNAVAILABLE: u32 = 0x8010_0017;
/// No smartcard service.
pub const SCARD_E_NO_SERVICE: u32 = 0x8010_001D;
/// Smartcard service stopped.
pub const SCARD_E_SERVICE_STOPPED: u32 = 0x8010_001E;
/// Unsupported feature.
pub const SCARD_E_UNSUPPORTED_FEATURE: u32 = 0x8010_0022;
/// No readers available.
pub const SCARD_E_NO_READERS_AVAILABLE: u32 = 0x8010_002E;
/// Unsupported card.
pub const SCARD_W_UNSUPPORTED_CARD: u32 = 0x8010_0065;
/// Unresponsive card.
pub const SCARD_W_UNRESPONSIVE_CARD: u32 = 0x8010_0066;
/// Unpowered card.
pub const SCARD_W_UNPOWERED_CARD: u32 = 0x8010_0067;
/// Card was reset by another application.
pub const SCARD_W_RESET_CARD: u32 = 0x8010_0068;
/// Card was removed.
pub const SCARD_W_REMOVED_CARD: u32 = 0x8010_0069;
/// Card requires authentication.
pub const SCARD_W_CARD_NOT_AUTHENTICATED: u32 = 0x8010_006F;

/// Maximum buffer size for SCardTransmit (66560 = 64.5 KiB).
pub const SCARD_TRANSMIT_MAX: usize = 66_560;
