// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Shared decoded framebuffers, one per display (§15.3).
//!
//! Decoded frames are written here under a `RwLock` and the consumer copies
//! out only the damaged rects it got via [`ClientEvent::FrameDirty`] — the
//! pixels never cross the event channel (the `primary_buffer` + `gdi_lock`
//! pattern of freerdp-integration, with a buffer per display).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::events::Rect;

/// Decoded framebuffer of one display: tight BGRA pixels (stride =
/// `width * 4`), matching `codec::DecodedFrame`.
#[derive(Debug, Default)]
pub struct DisplayBuffer {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// Shared handle to one display's framebuffer.
pub type SharedDisplay = Arc<RwLock<DisplayBuffer>>;

impl DisplayBuffer {
    /// Width in pixels (0 before the first frame).
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels (0 before the first frame).
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Tight BGRA pixels, `width * height * 4` bytes.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// The full-frame rectangle (today's damage granularity, §8/§15.1).
    #[must_use]
    pub fn full_rect(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: self.width,
            h: self.height,
        }
    }

    /// Replaces the contents with one decoded frame, resizing the buffer
    /// when the geometry changed.
    pub(crate) fn write_frame(&mut self, frame: &codec::DecodedFrame) {
        let len = frame.width as usize * frame.height as usize * 4;
        if self.width != frame.width || self.height != frame.height || self.pixels.len() != len {
            self.width = frame.width;
            self.height = frame.height;
            self.pixels = vec![0; len];
        }
        // Decoded frames are tight BGRA (stride = width * 4, codec crate).
        self.pixels.copy_from_slice(&frame.data[..len]);
    }

    /// Copies one rectangle, clipped to the framebuffer, as packed BGRA rows
    /// appended to `out`. Out-of-bounds or empty rects copy nothing.
    pub fn copy_rect_to(&self, rect: Rect, out: &mut Vec<u8>) {
        let (w, h) = (self.width as usize, self.height as usize);
        if w == 0 || h == 0 {
            return;
        }
        // Rect origins are signed; the clamps make every cast lossless.
        let x0 = usize::try_from(rect.x.max(0)).unwrap_or(0);
        let y0 = usize::try_from(rect.y.max(0)).unwrap_or(0);
        let rw = i32::try_from(rect.w).unwrap_or(i32::MAX);
        let rh = i32::try_from(rect.h).unwrap_or(i32::MAX);
        let x1 = usize::try_from(rect.x.saturating_add(rw).max(0)).unwrap_or(0);
        let y1 = usize::try_from(rect.y.saturating_add(rh).max(0)).unwrap_or(0);
        let (x1, y1) = (x1.min(w), y1.min(h));
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let row_bytes = (x1 - x0) * 4;
        out.reserve(row_bytes * (y1 - y0));
        for row in y0..y1 {
            let start = (row * w + x0) * 4;
            out.extend_from_slice(&self.pixels[start..start + row_bytes]);
        }
    }
}

/// Shared registry of per-display framebuffers (§8.3: one per display).
/// Cloning is cheap; all clones see the same buffers.
#[derive(Clone, Debug, Default)]
pub struct Displays {
    map: Arc<RwLock<HashMap<u8, SharedDisplay>>>,
}

impl Displays {
    /// Returns the framebuffer of one display, if it was announced or has
    /// produced frames already.
    #[must_use]
    pub fn get(&self, display_id: u8) -> Option<SharedDisplay> {
        self.map.read().ok()?.get(&display_id).cloned()
    }

    /// Known display ids.
    #[must_use]
    pub fn ids(&self) -> Vec<u8> {
        self.map
            .read()
            .map(|m| m.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Returns the framebuffer of one display, creating it on first use.
    pub(crate) fn get_or_insert(&self, display_id: u8) -> Option<SharedDisplay> {
        let mut map = self.map.write().ok()?;
        Some(map.entry(display_id).or_default().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32, fill: u8) -> codec::DecodedFrame {
        codec::DecodedFrame {
            width: w,
            height: h,
            stride: w as usize * 4,
            data: vec![fill; w as usize * h as usize * 4],
        }
    }

    #[test]
    fn write_frame_resizes_and_stores() {
        let mut buf = DisplayBuffer::default();
        buf.write_frame(&frame(2, 2, 0xAB));
        assert_eq!((buf.width(), buf.height()), (2, 2));
        assert_eq!(buf.pixels(), &[0xAB; 16]);
        buf.write_frame(&frame(1, 1, 0xCD));
        assert_eq!((buf.width(), buf.height()), (1, 1));
        assert_eq!(buf.pixels(), &[0xCD; 4]);
    }

    #[test]
    fn copy_rect_clips_to_bounds() {
        let mut buf = DisplayBuffer::default();
        // 2x1 frame: first pixel 0x01, second 0x02.
        let mut f = frame(2, 1, 0);
        f.data[0] = 0x01;
        f.data[4] = 0x02;
        buf.write_frame(&f);
        let mut out = Vec::new();
        buf.copy_rect_to(Rect { x: 1, y: 0, w: 5, h: 1 }, &mut out);
        assert_eq!(out, &[0x02, 0, 0, 0]);
        out.clear();
        buf.copy_rect_to(Rect { x: -1, y: 0, w: 2, h: 1 }, &mut out);
        assert_eq!(out, &[0x01, 0, 0, 0]);
        out.clear();
        buf.copy_rect_to(Rect { x: 0, y: 3, w: 1, h: 1 }, &mut out);
        assert!(out.is_empty(), "expected no copied samples, got {out:?}");
    }

    #[test]
    fn displays_registry_shares_buffers() {
        let displays = Displays::default();
        assert!(displays.get(0).is_none());
        let a = displays.get_or_insert(0).unwrap();
        a.write().unwrap().write_frame(&frame(1, 1, 7));
        let b = displays.get(0).unwrap();
        assert_eq!(b.read().unwrap().pixels(), &[7, 7, 7, 7]);
        assert_eq!(displays.ids(), vec![0]);
    }
}
