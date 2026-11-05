// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Windows implementation of the shared-memory webcam slot: the writer is
//! the server agent (per session), the reader is the `vcam` DirectShow
//! filter inside the consuming application process.

// MAPPING_BYTES is a small constant (~8 MiB); u32 casts cannot truncate.
#![allow(clippy::cast_possible_truncation)]

use std::sync::atomic::Ordering;

use anyhow::{Context as _, bail};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    PAGE_READWRITE, UnmapViewOfFile,
};
use windows::Win32::System::Threading::{CreateEventW, OpenEventW, SetEvent, SYNCHRONIZATION_ACCESS_RIGHTS};
use windows::core::HSTRING;

use super::{
    CamHeader, EVENT_NAME, MAGIC, MAPPING_BYTES, MAPPING_NAME, MAX_FRAME_BYTES, VERSION, frame_slot,
    header_seq,
};

/// `EVENT_MODIFY_STATE`, needed to signal the frame-ready event.
const EVENT_MODIFY_STATE: SYNCHRONIZATION_ACCESS_RIGHTS = SYNCHRONIZATION_ACCESS_RIGHTS(0x0002);

fn wide(name: &str) -> HSTRING {
    HSTRING::from(name)
}

/// Writer side of the shared camera slot (one per session, owned by the
/// server agent's [`crate`]-consuming sink).
pub struct CamSharedWriter {
    mapping: HANDLE,
    event: HANDLE,
    base: MEMORY_MAPPED_VIEW_ADDRESS,
}

// The mapping view is process-global memory; the handles are owned and only
// used through &mut self / Drop.
unsafe impl Send for CamSharedWriter {}

impl CamSharedWriter {
    /// Creates (or opens) the shared slot and its frame-ready event.
    pub fn create() -> anyhow::Result<Self> {
        unsafe {
            let mapping = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                MAPPING_BYTES as u32,
                &wide(MAPPING_NAME),
            )
            .context("CreateFileMappingW(UDSRP-Cam)")?;
            let base = MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, MAPPING_BYTES);
            if base.Value.is_null() {
                let _ = CloseHandle(mapping);
                bail!("MapViewOfFile(UDSRP-Cam) failed");
            }
            let event = CreateEventW(None, false, false, &wide(EVENT_NAME))
                .context("CreateEventW(UDSRP-Cam)")?;
            let base_ptr = base.Value.cast::<u8>();
            // Idempotent init: a reader (filter) may have created the
            // mapping first; only stamp magic/version once.
            let hdr = base_ptr_header(base_ptr);
            if hdr.magic != MAGIC {
                hdr.magic = MAGIC;
                hdr.version = VERSION;
                hdr.width = 0;
                hdr.height = 0;
                hdr.ts_ms = 0;
                hdr.flags = 0;
                hdr.reserved = [0; 9];
                header_seq(base_ptr).store(0, Ordering::Release);
            }
            Ok(Self { mapping, event, base })
        }
    }

    /// Publishes one BGRA frame (tight, `width*height*4` bytes).
    pub fn write_frame(&mut self, bgra: &[u8], width: u32, height: u32, ts_ms: u32) {
        debug_assert!(width > 0 && height > 0);
        let need = (width as usize) * (height as usize) * 4;
        if need > MAX_FRAME_BYTES || bgra.len() < need {
            return;
        }
        unsafe {
            let base_ptr = self.base.Value.cast::<u8>();
            let seq = header_seq(base_ptr);
            // Seqlock: odd while writing, even (incremented) when stable.
            seq.fetch_add(1, Ordering::AcqRel);
            let hdr = base_ptr_header(base_ptr);
            hdr.width = width;
            hdr.height = height;
            hdr.ts_ms = ts_ms;
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), frame_slot(base_ptr), need);
            seq.fetch_add(1, Ordering::AcqRel);
            let _ = SetEvent(self.event);
        }
    }

    /// Marks the slot as producer-less; readers render black frames.
    pub fn clear(&mut self) {
        unsafe {
            let base_ptr = self.base.Value.cast::<u8>();
            let seq = header_seq(base_ptr);
            seq.fetch_add(1, Ordering::AcqRel);
            let hdr = base_ptr_header(base_ptr);
            hdr.width = 0;
            hdr.height = 0;
            seq.fetch_add(1, Ordering::AcqRel);
        }
    }
}

impl Drop for CamSharedWriter {
    fn drop(&mut self) {
        self.clear();
        unsafe {
            let _ = UnmapViewOfFile(self.base);
            let _ = CloseHandle(self.mapping);
            let _ = CloseHandle(self.event);
        }
    }
}

/// Reader side (vcam filter): maps the slot read-only and copies frames out
/// under the seqlock.
pub struct CamSharedReader {
    mapping: HANDLE,
    event: HANDLE,
    base: MEMORY_MAPPED_VIEW_ADDRESS,
}

unsafe impl Send for CamSharedReader {}

impl CamSharedReader {
    /// Opens (creating if needed) the slot for reading.
    pub fn open() -> anyhow::Result<Self> {
        unsafe {
            // Create-or-open semantics via CreateFileMappingW so the filter
            // works before any session produced a frame.
            let mapping = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                MAPPING_BYTES as u32,
                &wide(MAPPING_NAME),
            )
            .context("CreateFileMappingW(UDSRP-Cam)")?;
            let base = MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, MAPPING_BYTES);
            if base.Value.is_null() {
                let _ = CloseHandle(mapping);
                bail!("MapViewOfFile(UDSRP-Cam) failed");
            }
            // SYNCHRONIZE (0x0010_0000) so the filter thread can wait on it.
            let access = SYNCHRONIZATION_ACCESS_RIGHTS(EVENT_MODIFY_STATE.0 | 0x0010_0000);
            let event = match OpenEventW(access, false, &wide(EVENT_NAME)) {
                Ok(ev) => ev,
                Err(_) => CreateEventW(None, false, false, &wide(EVENT_NAME))
                    .context("CreateEventW(UDSRP-Cam)")?,
            };
            let base_ptr = base.Value.cast::<u8>();
            let hdr = base_ptr_header(base_ptr);
            if hdr.magic != MAGIC {
                hdr.magic = MAGIC;
                hdr.version = VERSION;
                hdr.width = 0;
                hdr.height = 0;
                header_seq(base_ptr).store(0, Ordering::Release);
            }
            Ok(Self { mapping, event, base })
        }
    }

    /// Event handle to wait on for new frames (borrowed; valid while self lives).
    #[must_use]
    pub fn event_handle(&self) -> HANDLE {
        self.event
    }

    /// Current producer geometry, if a producer is publishing.
    #[must_use]
    pub fn current_dims(&self) -> Option<(u32, u32)> {
        unsafe {
            let hdr = base_ptr_header(self.base.Value.cast::<u8>());
            (hdr.magic == MAGIC && hdr.width > 0 && hdr.height > 0).then_some((hdr.width, hdr.height))
        }
    }

    /// Copies the latest frame into `out` (BGRA). Returns
    /// `Some((width, height, ts_ms))`, or `None` when no producer is
    /// attached (caller should render black).
    pub fn read_frame(&self, out: &mut Vec<u8>) -> Option<(u32, u32, u32)> {
        unsafe {
            let base_ptr = self.base.Value.cast::<u8>();
            let seq = header_seq(base_ptr);
            for _ in 0..8 {
                let s0 = seq.load(Ordering::Acquire);
                if s0 & 1 != 0 {
                    std::hint::spin_loop();
                    continue;
                }
                let hdr = base_ptr_header(base_ptr);
                let (w, h, ts) = (hdr.width, hdr.height, hdr.ts_ms);
                if w == 0 || h == 0 {
                    return None;
                }
                let need = (w as usize) * (h as usize) * 4;
                if need > MAX_FRAME_BYTES {
                    return None;
                }
                out.resize(need, 0);
                std::ptr::copy_nonoverlapping(frame_slot(base_ptr), out.as_mut_ptr(), need);
                if seq.load(Ordering::Acquire) == s0 {
                    return Some((w, h, ts));
                }
            }
            None
        }
    }
}

impl Drop for CamSharedReader {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.base);
            let _ = CloseHandle(self.mapping);
            let _ = CloseHandle(self.event);
        }
    }
}

/// Mutable header view over the mapping base.
///
/// # Safety
/// `base` must point to the mapped header.
unsafe fn base_ptr_header(base: *const u8) -> &'static mut CamHeader {
    unsafe { &mut *(base as *mut CamHeader) }
}
