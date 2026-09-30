//! Frame capture abstraction shared by all capture backends.
//!
//! Backends (DXGI Desktop Duplication on Windows, the synthetic pattern for
//! development) produce [`CapturedFrame`]s through the [`FrameSource`] trait,
//! so the session core stays portable and testable.

use std::time::Duration;

/// Pixel-space rectangle (`left`/`top` inclusive, `right`/`bottom` exclusive),
/// as used by DXGI dirty rects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// Left edge, inclusive.
    pub left: i32,
    /// Top edge, inclusive.
    pub top: i32,
    /// Right edge, exclusive.
    pub right: i32,
    /// Bottom edge, exclusive.
    pub bottom: i32,
}

impl Rect {
    /// True when the rect has zero or negative area.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    /// Smallest rect containing every rect in `rects`, or `None` when the
    /// list is empty (empty rects are ignored).
    #[must_use]
    pub fn bounding(rects: &[Rect]) -> Option<Self> {
        let mut iter = rects.iter().filter(|r| !r.is_empty());
        let first = iter.next()?;
        Some(iter.fold(*first, |acc, r| Self {
            left: acc.left.min(r.left),
            top: acc.top.min(r.top),
            right: acc.right.max(r.right),
            bottom: acc.bottom.max(r.bottom),
        }))
    }
}

/// One captured desktop frame in tightly packed BGRA8 (top-down rows).
pub struct CapturedFrame {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Bytes per row in `data` (always `width * 4` today; kept for clarity).
    pub stride: usize,
    /// Pixel data, `stride * height` bytes.
    pub data: Vec<u8>,
    /// Regions that actually changed since the previous frame, in pixel
    /// coordinates. An empty list means "nothing changed" (skip encode).
    /// A full-screen rect is used when the compositor reports changes but no
    /// dirty rects (e.g. move-only updates).
    pub dirty_rects: Vec<Rect>,
}

/// A source of desktop frames.
pub trait FrameSource {
    /// Frame geometry `(width, height)` in pixels.
    fn dimensions(&self) -> (u32, u32);

    /// Waits up to `timeout` for a new frame.
    ///
    /// Returns `Ok(None)` when the timeout elapses with no desktop changes,
    /// so callers can distinguish "no damage" from "error".
    fn next_frame(&mut self, timeout: Duration) -> anyhow::Result<Option<CapturedFrame>>;
}

/// Copies `width * 4`-byte BGRA rows from `src` (row stride `src_stride`)
/// into the tightly packed `dst` (`width * height * 4` bytes).
///
/// GPU-mapped rows come aligned (`RowPitch`), so `src_stride` is usually
/// larger than `width * 4` and must never be assumed tight.
pub fn copy_bgra_rows(
    src: &[u8],
    src_stride: usize,
    width: u32,
    height: u32,
    dst: &mut [u8],
) -> anyhow::Result<()> {
    let row = width as usize * 4;
    let rows = height as usize;
    anyhow::ensure!(
        src_stride >= row,
        "src_stride {src_stride} smaller than row {row}"
    );
    anyhow::ensure!(
        src.len() >= src_stride * rows,
        "src too small: {} < {}",
        src.len(),
        src_stride * rows
    );
    anyhow::ensure!(
        dst.len() == row * rows,
        "dst must be exactly {} bytes, got {}",
        row * rows,
        dst.len()
    );
    for y in 0..rows {
        dst[y * row..(y + 1) * row].copy_from_slice(&src[y * src_stride..y * src_stride + row]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounding_of_empty_list_is_none() {
        assert_eq!(Rect::bounding(&[]), None);
    }

    #[test]
    fn bounding_ignores_empty_rects() {
        let rects = [
            Rect { left: 5, top: 5, right: 5, bottom: 9 }, // empty
            Rect { left: 10, top: 20, right: 30, bottom: 40 },
        ];
        assert_eq!(
            Rect::bounding(&rects),
            Some(Rect { left: 10, top: 20, right: 30, bottom: 40 })
        );
    }

    #[test]
    fn bounding_unions_all_rects() {
        let rects = [
            Rect { left: 10, top: 10, right: 20, bottom: 20 },
            Rect { left: 0, top: 50, right: 5, bottom: 60 },
            Rect { left: 15, top: 5, right: 100, bottom: 15 },
        ];
        assert_eq!(
            Rect::bounding(&rects),
            Some(Rect { left: 0, top: 5, right: 100, bottom: 60 })
        );
    }

    #[test]
    fn copy_rows_from_padded_stride() {
        // 2x2 frame with a 4-pixel padded source stride (24 bytes per row).
        let width = 2u32;
        let height = 2u32;
        let src_stride = 24usize;
        let mut src = vec![0xEEu8; src_stride * height as usize];
        // Row 0 pixels: [1,2,3,4] [5,6,7,8]; row 1 pixels: [9..12] [13..16].
        src[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        src[src_stride..src_stride + 8].copy_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16]);

        let mut dst = vec![0u8; (width * height * 4) as usize];
        copy_bgra_rows(&src, src_stride, width, height, &mut dst).unwrap();
        assert_eq!(dst, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
    }

    #[test]
    fn copy_rows_rejects_tight_src_stride() {
        let src = vec![0u8; 8];
        let mut dst = vec![0u8; 16];
        assert!(copy_bgra_rows(&src, 4, 2, 2, &mut dst).is_err());
    }

    #[test]
    fn copy_rows_rejects_wrong_dst_size() {
        let src = vec![0u8; 16];
        let mut dst = vec![0u8; 15];
        assert!(copy_bgra_rows(&src, 8, 2, 2, &mut dst).is_err());
    }
}
