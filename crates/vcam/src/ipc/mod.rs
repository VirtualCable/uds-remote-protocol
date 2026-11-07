// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Shared-memory transport between the UDS RP server agent (webcam frame
//! producer) and the DirectShow virtual camera filter (`vcam` crate,
//! consumer, loaded in-process by the capturing application).
//!
//! Layout: a fixed 64-byte header followed by a single BGRA frame slot of
//! the maximum supported resolution. The writer (session agent) publishes
//! frames with a seqlock (`seq` odd while writing) and pulses a named
//! auto-reset event; readers copy the slot out under the seqlock. A webcam
//! frame is droppable by design, so latest-frame-wins is the desired
//! semantics — no queue, no backpressure towards the network path.
//!
//! Both endpoints live in the same Windows session (the console session the
//! agent controls), so all named objects use the `Local\` namespace.

// Mapping bases come from MapViewOfFile (64K-aligned); pointer alignment and
// raw-pointer casts are safe by construction.
#![allow(clippy::cast_ptr_alignment, clippy::ptr_cast_constness, clippy::must_use_candidate)]

use std::sync::atomic::AtomicU32;

/// Header magic, ASCII "URVC".
pub const MAGIC: u32 = 0x5552_5643;
/// Layout version.
pub const VERSION: u32 = 1;

/// Maximum frame width the slot can hold. 1920x1080 BGRA ≈ 8.3 MiB; the
/// mapping is allocated once at this size regardless of the negotiated
/// webcam resolution.
pub const MAX_WIDTH: u32 = 1920;
/// Maximum frame height (see [`MAX_WIDTH`]).
pub const MAX_HEIGHT: u32 = 1080;
/// Bytes of one maximum-size BGRA frame.
pub const MAX_FRAME_BYTES: usize = (MAX_WIDTH * MAX_HEIGHT * 4) as usize;
/// Total mapping size: header + slot.
pub const MAPPING_BYTES: usize = HEADER_BYTES + MAX_FRAME_BYTES;
/// Header size in bytes.
pub const HEADER_BYTES: usize = 64;

/// Mapping/object naming: `Local\UDSRP-Cam-0` and `Local\UDSRP-Cam-0-Event`.
/// A single virtual camera per session is all we expose today.
pub const MAPPING_NAME: &str = r"Local\UDSRP-Cam-0";
/// Auto-reset event pulsed by the writer after each published frame.
pub const EVENT_NAME: &str = r"Local\UDSRP-Cam-0-Event";

/// Fixed header at offset 0 of the mapping. Plain `u32` fields; `seq` is
/// accessed atomically through [`header_seq`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CamHeader {
    /// [`MAGIC`] once a producer initialized the mapping.
    pub magic: u32,
    /// [`VERSION`].
    pub version: u32,
    /// Active frame width in pixels; 0 means "no producer".
    pub width: u32,
    /// Active frame height in pixels; 0 means "no producer".
    pub height: u32,
    /// Seqlock counter: odd while a frame write is in progress.
    pub seq: u32,
    /// Producer-side capture timestamp in milliseconds (client clock).
    pub ts_ms: u32,
    /// Bit flags; none defined yet.
    pub flags: u32,
    /// Reserved, zero.
    pub reserved: [u32; 9],
}

const _: () = assert!(size_of::<CamHeader>() == HEADER_BYTES);

/// Casts a mapping base pointer to the header.
///
/// # Safety
/// `base` must point to at least [`HEADER_BYTES`] mapped bytes.
pub unsafe fn header<'a>(base: *const u8) -> &'a CamHeader {
    unsafe { &*(base.cast::<CamHeader>()) }
}

/// Atomic view of the seqlock counter.
///
/// # Safety
/// Same as [`header`].
pub unsafe fn header_seq(base: *const u8) -> &'static AtomicU32 {
    unsafe { &*base.add(4 * 4).cast::<AtomicU32>() }
}

/// Returns the frame slot slice for a mapping of at least [`MAPPING_BYTES`].
///
/// # Safety
/// `base` must point to the start of the mapping and remain valid.
pub unsafe fn frame_slot(base: *mut u8) -> *mut u8 {
    unsafe { base.add(HEADER_BYTES) }
}

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::{CamSharedReader, CamSharedWriter};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_sizes() {
        assert_eq!(HEADER_BYTES, 64);
        assert_eq!(MAX_FRAME_BYTES, 1920 * 1080 * 4);
        assert_eq!(MAPPING_BYTES, 64 + 1920 * 1080 * 4);
    }

    #[test]
    fn names_are_session_local() {
        assert!(MAPPING_NAME.starts_with("Local\\"));
        assert!(EVENT_NAME.starts_with("Local\\"));
    }
}
