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

/// A captured cursor shape, converted to tightly packed RGBA8 (row-major).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedCursor {
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
    /// Hotspot X offset inside the image.
    pub hot_x: u16,
    /// Hotspot Y offset inside the image.
    pub hot_y: u16,
    /// RGBA pixels, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

/// Cursor state change observed while capturing (design doc §11): the client
/// paints the cursor locally, so only shape and visibility cross the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CursorUpdate {
    /// New visibility, when it changed.
    pub visible: Option<bool>,
    /// New shape, when it changed.
    pub shape: Option<CapturedCursor>,
}

/// Kind of masked cursor shape, as DXGI reports it
/// (`DXGI_OUTDUPL_POINTER_SHAPE_TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskShapeKind {
    /// 1-bpp AND + XOR masks stacked vertically (buffer height is 2× the
    /// logical height).
    Monochrome,
    /// BGRA color pixels + trailing 1-bpp AND mask.
    Color,
    /// BGRA color pixels + trailing 1-bpp mask (1 = transparent).
    MaskedColor,
}

/// Converts a DXGI pointer-shape buffer to tightly packed RGBA8.
///
/// `height`/`pitch` describe the raw buffer (for [`MaskShapeKind::Monochrome`]
/// the buffer holds both masks, so its height is twice the logical one).
/// XOR-inverted pixels (monochrome `and=1, xor=1`) are approximated with a
/// semi-transparent gray — a PoC simplification, real inversion needs the
/// pixel under the cursor which the client may not have yet (§11).
///
/// Returns `None` for degenerate geometry (zero size, odd monochrome
/// height, or a buffer too small for the declared layout).
#[must_use]
pub fn mask_shape_to_rgba(
    kind: MaskShapeKind,
    width: u16,
    height: u16,
    pitch: usize,
    hot_x: u16,
    hot_y: u16,
    buf: &[u8],
) -> Option<CapturedCursor> {
    let w_px = usize::from(width);
    if w_px == 0 || height == 0 {
        return None;
    }
    let (logical_h, rgba) = match kind {
        MaskShapeKind::Monochrome => {
            if !height.is_multiple_of(2) {
                return None;
            }
            let rows = usize::from(height) / 2;
            if pitch * usize::from(height) > buf.len() || pitch * 8 < w_px {
                return None;
            }
            let bit = |row: &[u8], col: usize| (row[col / 8] >> (7 - (col % 8))) & 1 == 1;
            let mut rgba = Vec::with_capacity(w_px * rows * 4);
            for row_i in 0..rows {
                let and_row = &buf[row_i * pitch..(row_i + 1) * pitch];
                let xor_row = &buf[(row_i + rows) * pitch..(row_i + rows + 1) * pitch];
                for col in 0..w_px {
                    let px: [u8; 4] = match (bit(and_row, col), bit(xor_row, col)) {
                        (false, false) => [0, 0, 0, 255],
                        (false, true) => [255, 255, 255, 255],
                        (true, false) => [0, 0, 0, 0],
                        (true, true) => [128, 128, 128, 128],
                    };
                    rgba.extend_from_slice(&px);
                }
            }
            (rows, rgba)
        }
        MaskShapeKind::Color | MaskShapeKind::MaskedColor => {
            let rows = usize::from(height);
            let color_bytes = pitch.checked_mul(rows)?;
            if pitch < w_px * 4 || color_bytes > buf.len() {
                return None;
            }
            let mask_pitch = w_px.div_ceil(8);
            let has_mask = buf.len() >= color_bytes + mask_pitch * rows;
            let mask_bit = |col: usize, row_i: usize| -> bool {
                has_mask
                    && (buf[color_bytes + row_i * mask_pitch + col / 8] >> (7 - (col % 8))) & 1 == 1
            };
            let mut rgba = Vec::with_capacity(w_px * rows * 4);
            for row_i in 0..rows {
                for col in 0..w_px {
                    let pixel = &buf[row_i * pitch + col * 4..row_i * pitch + col * 4 + 4];
                    let (blue, green, red) = (pixel[0], pixel[1], pixel[2]);
                    let alpha = match (kind, mask_bit(col, row_i)) {
                        // MaskedColor: mask bit means transparent.
                        (MaskShapeKind::MaskedColor, true) => 0,
                        // Color: mask bit means XOR with background; a black
                        // pixel is the classic transparent color key, anything
                        // else stays opaque (XOR ignored, PoC).
                        (MaskShapeKind::Color, true) if blue == 0 && green == 0 && red == 0 => 0,
                        _ => 255,
                    };
                    rgba.extend_from_slice(&[red, green, blue, alpha]);
                }
            }
            (rows, rgba)
        }
    };
    Some(CapturedCursor {
        width,
        height: u16::try_from(logical_h).ok()?,
        hot_x,
        hot_y,
        rgba,
    })
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

    /// Takes the cursor update accumulated since the last call, if any.
    /// Default: backend has no cursor information.
    fn take_cursor_update(&mut self) -> Option<CursorUpdate> {
        None
    }
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

    #[test]
    fn monochrome_shape_converts() {
        // 4x1 logical cursor: and mask top row, xor mask bottom row.
        // Pixels: (and,xor) = (0,0) black, (0,1) white, (1,0) transparent,
        // (1,1) inverted → semi-gray.
        let buf = [
            0b0011_0000, // AND row (pitch 1)
            0b0101_0000, // XOR row
        ];
        let cur =
            mask_shape_to_rgba(MaskShapeKind::Monochrome, 4, 2, 1, 0, 0, &buf).unwrap();
        assert_eq!((cur.width, cur.height), (4, 1));
        assert_eq!(
            cur.rgba,
            vec![
                0, 0, 0, 255, // black
                255, 255, 255, 255, // white
                0, 0, 0, 0, // transparent
                128, 128, 128, 128, // inverted
            ]
        );
    }

    #[test]
    fn monochrome_shape_rejects_odd_height() {
        assert!(mask_shape_to_rgba(MaskShapeKind::Monochrome, 4, 3, 1, 0, 0, &[0; 3]).is_none());
    }

    #[test]
    fn color_shape_uses_black_mask_as_transparent() {
        // 2x1 color cursor, pitch 8, AND mask (2 bits in 1 byte): pixel 0
        // masked+black → transparent; pixel 1 unmasked red → opaque.
        let mut buf = vec![0, 0, 0, 0, 0, 0, 255, 0]; // BGRA: black, red
        buf.push(0b1000_0000); // AND mask: pixel 0 set
        let cur = mask_shape_to_rgba(MaskShapeKind::Color, 2, 1, 8, 1, 0, &buf).unwrap();
        assert_eq!((cur.hot_x, cur.hot_y), (1, 0));
        assert_eq!(cur.rgba, vec![0, 0, 0, 0, 255, 0, 0, 255]);
    }

    #[test]
    fn masked_color_shape_mask_means_transparent() {
        let mut buf = vec![0, 255, 0, 0]; // one green pixel (BGRA)
        buf.push(0b1000_0000);
        let cur = mask_shape_to_rgba(MaskShapeKind::MaskedColor, 1, 1, 4, 0, 0, &buf).unwrap();
        assert_eq!(cur.rgba, vec![0, 255, 0, 0]);
    }

    #[test]
    fn shape_rejects_degenerate_geometry() {
        assert!(mask_shape_to_rgba(MaskShapeKind::Color, 0, 1, 4, 0, 0, &[0; 4]).is_none());
        // Buffer too small for declared layout.
        assert!(mask_shape_to_rgba(MaskShapeKind::Color, 4, 2, 16, 0, 0, &[0; 8]).is_none());
    }
}
