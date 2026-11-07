// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Cryptographic format constants.

/// Session ticket length (HKDF salt), in bytes.
pub const TICKET_LENGTH: usize = 48;
/// Length of the shared secret and of each derived key, in bytes.
pub const KEY_LENGTH: usize = 32;
/// AES-256-GCM tag length, in bytes.
pub const TAG_LENGTH: usize = 16;
/// Cleartext header of a stream packet: `seq (8B BE) + len (2B BE)`.
pub const STREAM_HEADER_SIZE: usize = 10;
/// Size of the `channel_id` field, encrypted inside the packet.
pub const CHANNEL_ID_SIZE: usize = 2;
/// Maximum plaintext per packet: `len` is u16 and covers `channel_id + ciphertext + tag`.
pub const MAX_PLAINTEXT: usize = u16::MAX as usize - CHANNEL_ID_SIZE - TAG_LENGTH;

/// UDP datagram session token length (demultiplexing, §6.2).
pub const TOKEN_LENGTH: usize = 16;
/// Size of a datagram's cleartext header: token + seq.
pub const DATAGRAM_HEADER_SIZE: usize = TOKEN_LENGTH + 8;
/// Maximum payload per datagram (§6.2): with IPv6 + UDP + headers the
/// datagram stays ≤ 1280B and never suffers IP fragmentation.
pub const MAX_DATAGRAM_PAYLOAD: usize = 1180;

// §6.2 budget: IPv6 (40B) + UDP (8B) + protocol headers (~50B) ≤ 1280B MTU.
const _: () = assert!(MAX_DATAGRAM_PAYLOAD <= 1180);
