// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

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
    /// RGBA pixels, `width * height * 4` bytes. Pixels whose bit is set in
    /// `xor_mask` carry their source RGB with alpha 255.
    pub rgba: Vec<u8>,
    /// 1bpp mask (MSB first, row-major, `ceil(width*height/8)` bytes): a set
    /// bit marks the pixel as XOR-with-background (DXGI masked-color /
    /// monochrome invert semantics). Empty when no pixel XORs.
    pub xor_mask: Vec<u8>,
}

/// Cursor state change observed while capturing (design doc §10): the client
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
    /// BGRA color pixels with a real alpha channel.
    Color,
    /// BGRA color pixels where the ALPHA byte is the mask (MSDN): 0x00 = the
    /// RGB replaces the screen pixel; 0xFF = the RGB is XORed with the
    /// screen pixel.
    MaskedColor,
}

/// Sets bit `i` (MSB first) in a growable 1bpp mask.
fn set_bit(mask: &mut Vec<u8>, i: usize) {
    let byte = i / 8;
    if mask.len() <= byte {
        mask.resize(byte + 1, 0);
    }
    mask[byte] |= 0x80 >> (i % 8);
}

/// Converts a DXGI pointer-shape buffer to tightly packed RGBA8.
///
/// For [`MaskShapeKind::Monochrome`], `height` is the **logical** cursor
/// height: the buffer holds the AND mask in rows `0..height` and the XOR
/// mask in rows `height..2*height` (DXGI stores both stacked). Invert
/// pixels (`and=1, xor=1`) come out white with their XOR bit set.
///
/// For [`MaskShapeKind::MaskedColor`] the alpha byte is the mask (MSDN):
/// 0x00 replaces the screen pixel (opaque), 0xFF XORs the RGB with the
/// screen (opaque RGB + XOR bit set). The client composites XOR pixels by
/// inverting the background under them (§10).
///
/// Returns `None` for degenerate geometry (zero size or a buffer too small
/// for the declared layout).
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
    let (logical_h, rgba, xor_mask) = match kind {
        MaskShapeKind::Monochrome => {
            let rows = usize::from(height);
            // Both masks stacked: the buffer must hold 2*height rows and one
            // row must hold all the pixels of a row.
            if pitch * rows * 2 > buf.len() || pitch * 8 < w_px {
                return None;
            }
            let bit = |row: &[u8], col: usize| (row[col / 8] >> (7 - (col % 8))) & 1 == 1;
            let mut rgba = Vec::with_capacity(w_px * rows * 4);
            let mut xor_mask = Vec::new();
            for row_i in 0..rows {
                let and_row = &buf[row_i * pitch..(row_i + 1) * pitch];
                let xor_row = &buf[(row_i + rows) * pitch..(row_i + rows + 1) * pitch];
                for col in 0..w_px {
                    let px: [u8; 4] = match (bit(and_row, col), bit(xor_row, col)) {
                        (false, false) => [0, 0, 0, 255],
                        (false, true) => [255, 255, 255, 255],
                        (true, false) => [0, 0, 0, 0],
                        (true, true) => {
                            // Invert: white XOR pixel.
                            set_bit(&mut xor_mask, row_i * w_px + col);
                            [255, 255, 255, 255]
                        }
                    };
                    rgba.extend_from_slice(&px);
                }
            }
            (rows, rgba, xor_mask)
        }
        MaskShapeKind::Color | MaskShapeKind::MaskedColor => {
            let rows = usize::from(height);
            let color_bytes = pitch.checked_mul(rows)?;
            if pitch < w_px * 4 || color_bytes > buf.len() {
                return None;
            }
            let mut rgba = Vec::with_capacity(w_px * rows * 4);
            let mut xor_mask = Vec::new();
            for row_i in 0..rows {
                for col in 0..w_px {
                    let pixel = &buf[row_i * pitch + col * 4..row_i * pitch + col * 4 + 4];
                    let (blue, green, red) = (pixel[0], pixel[1], pixel[2]);
                    let alpha = match kind {
                        MaskShapeKind::MaskedColor => match pixel[3] {
                            // RGB replaces the screen pixel.
                            0x00 => 255,
                            // RGB XORs the screen pixel. A BLACK XOR is a
                            // no-op (x ^ 0 == x): emit it as transparent, NOT
                            // as an XOR pixel — the client approximates XOR
                            // with a reverse-subtract blend, and 0 - dst
                            // would clamp to black instead of keeping dst.
                            // Any other XOR color ships opaque + XOR bit.
                            0xFF if red == 0 && green == 0 && blue == 0 => 0,
                            0xFF => {
                                set_bit(&mut xor_mask, row_i * w_px + col);
                                255
                            }
                            // Off-spec value: treat as replace (opaque).
                            _ => 255,
                        },
                        // Color: BGRA with a real alpha channel — honor it or
                        // the client paints an opaque black tile around the
                        // pointer.
                        MaskShapeKind::Color => pixel[3],
                        // Monochrome is handled by the dedicated arm above.
                        MaskShapeKind::Monochrome => unreachable!(),
                    };
                    rgba.extend_from_slice(&[red, green, blue, alpha]);
                }
            }
            (rows, rgba, xor_mask)
        }
    };
    // The wire format fixes the mask at ceil(w*h/8) bytes; a sparse mask
    // shorter than that (last XOR pixel far from the bottom-right corner,
    // e.g. the centered I-beam) must be zero-padded or decoders reject it.
    let mut xor_mask = xor_mask;
    if !xor_mask.is_empty() {
        xor_mask.resize((w_px * logical_h).div_ceil(8), 0);
    }
    Some(CapturedCursor {
        width,
        height: u16::try_from(logical_h).ok()?,
        hot_x,
        hot_y,
        rgba,
        xor_mask,
    })
}

/// Which desktop output a capture backend should capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputSelector {
    /// Nth output of the first adapter that has one (the historical
    /// `--output N` behavior; `0` is usually the primary monitor).
    Index(u32),
    /// First output whose adapter description or output device name
    /// contains this substring, case-insensitive (e.g. `"QXL"` selects the
    /// QXL monitor whichever display index the OS gave it). Must not be
    /// empty (an empty needle would match the first output).
    NameContains(String),
}

/// One enumerable desktop output, as the platform backend reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputCandidate {
    /// Description of the adapter owning the output (DXGI's
    /// `DXGI_ADAPTER_DESC1.Description`, e.g. `"Red Hat QXL controller"`).
    pub adapter: String,
    /// The output's device name (e.g. `\\.\DISPLAY1`).
    pub name: String,
    /// The output's GDI device string (`EnumDisplayDevicesW`'s
    /// `DeviceString`, the monitor description). DXGI can report an output
    /// under a generic adapter such as "Microsoft Basic Render Driver",
    /// hiding its own adapter description, so this GDI string is the
    /// reliable way to identify a specific monitor. Empty when the backend
    /// cannot resolve it.
    pub gdi_name: String,
    /// Output index within its adapter.
    pub index_in_adapter: u32,
}

/// Picks the position in `candidates` (adapter-major order) that matches
/// `selector`, or `None` when nothing matches. Pure, so the selection rules
/// are testable off Windows.
#[must_use]
pub fn select_output(candidates: &[OutputCandidate], selector: &OutputSelector) -> Option<usize> {
    match selector {
        OutputSelector::Index(n) => candidates.iter().position(|c| c.index_in_adapter == *n),
        OutputSelector::NameContains(needle) => {
            let needle = needle.to_lowercase();
            candidates.iter().position(|c| {
                c.adapter.to_lowercase().contains(&needle)
                    || c.name.to_lowercase().contains(&needle)
                    || c.gdi_name.to_lowercase().contains(&needle)
            })
        }
    }
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

    /// GDI device name of the captured output (e.g. `\\.\DISPLAY2`), for
    /// translating display-local input coordinates into virtual-desktop
    /// ones. Default: backend does not capture a GDI output.
    fn output_device(&self) -> Option<&str> {
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
            Rect {
                left: 5,
                top: 5,
                right: 5,
                bottom: 9,
            }, // empty
            Rect {
                left: 10,
                top: 20,
                right: 30,
                bottom: 40,
            },
        ];
        assert_eq!(
            Rect::bounding(&rects),
            Some(Rect {
                left: 10,
                top: 20,
                right: 30,
                bottom: 40
            })
        );
    }

    #[test]
    fn bounding_unions_all_rects() {
        let rects = [
            Rect {
                left: 10,
                top: 10,
                right: 20,
                bottom: 20,
            },
            Rect {
                left: 0,
                top: 50,
                right: 5,
                bottom: 60,
            },
            Rect {
                left: 15,
                top: 5,
                right: 100,
                bottom: 15,
            },
        ];
        assert_eq!(
            Rect::bounding(&rects),
            Some(Rect {
                left: 0,
                top: 5,
                right: 100,
                bottom: 60
            })
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
        // 4x1 logical cursor: AND mask in row 0, XOR mask in row 1.
        // Pixels: (and,xor) = (0,0) black, (0,1) white, (1,0) transparent,
        // (1,1) inverted → white XOR pixel (bit set in the xor mask).
        let buf = [
            0b0011_0000, // AND row (pitch 1)
            0b0101_0000, // XOR row
        ];
        let cur = mask_shape_to_rgba(MaskShapeKind::Monochrome, 4, 1, 1, 0, 0, &buf).unwrap();
        assert_eq!((cur.width, cur.height), (4, 1));
        assert_eq!(
            cur.rgba,
            vec![
                0, 0, 0, 255, // black
                255, 255, 255, 255, // white
                0, 0, 0, 0, // transparent
                255, 255, 255, 255, // inverted (white XOR)
            ]
        );
        assert_eq!(cur.xor_mask, vec![0b0001_0000]);
    }

    #[test]
    fn monochrome_shape_allows_odd_height() {
        // DXGI reports the logical height (odd for e.g. 41x39 arrows); the
        // buffer holds 2*height rows.
        let buf = vec![0xFFu8; 2 * 3]; // pitch 1, 3 AND rows + 3 XOR rows
        let cur = mask_shape_to_rgba(MaskShapeKind::Monochrome, 8, 3, 1, 0, 0, &buf).unwrap();
        assert_eq!((cur.width, cur.height), (8, 3));
        // and=1, xor=1 everywhere → all white XOR pixels; 24 px = 3 mask bytes.
        assert!(
            cur.rgba
                .as_chunks::<4>()
                .0
                .iter()
                .all(|&p| p == [255, 255, 255, 255])
        );
        assert_eq!(cur.xor_mask, vec![0xFF; 3]);
    }

    #[test]
    fn monochrome_shape_rejects_short_buffer() {
        // Needs 2*pitch*height bytes; one mask missing.
        assert!(mask_shape_to_rgba(MaskShapeKind::Monochrome, 4, 2, 1, 0, 0, &[0; 2]).is_none());
    }

    #[test]
    fn color_shape_honors_the_alpha_channel() {
        // 2x1 color cursor, pitch 8: pixel 0 transparent (alpha 0), pixel 1
        // opaque red. The wire format keeps the source alpha verbatim so the
        // client can composite the pointer shape correctly.
        let buf = vec![0, 0, 0, 0, 0, 0, 255, 255]; // BGRA: clear, red
        let cur = mask_shape_to_rgba(MaskShapeKind::Color, 2, 1, 8, 1, 0, &buf).unwrap();
        assert_eq!((cur.hot_x, cur.hot_y), (1, 0));
        assert_eq!(cur.rgba, vec![0, 0, 0, 0, 255, 0, 0, 255]);
    }

    #[test]
    fn masked_color_uses_alpha_byte_as_xor_mask() {
        // MSDN: alpha 0x00 -> RGB replaces the screen (opaque); alpha 0xFF
        // -> XOR with the screen. Black XOR is a no-op (transparent, NOT
        // xor-marked); non-black XOR ships opaque with the bit set.
        let buf = vec![255, 255, 255, 0xFF, 0, 0, 0, 0xFF]; // white-XOR, black-XOR
        let cur = mask_shape_to_rgba(MaskShapeKind::MaskedColor, 2, 1, 8, 0, 0, &buf).unwrap();
        assert_eq!(cur.rgba, vec![255, 255, 255, 255, 0, 0, 0, 0]);
        assert_eq!(cur.xor_mask, vec![0b1000_0000]);
    }

    #[test]
    fn masked_color_alpha_zero_replaces_screen() {
        // Opaque pixel per the alpha byte, no XOR: green stays green and
        // opaque.
        let buf = vec![0, 255, 0, 0x00];
        let cur = mask_shape_to_rgba(MaskShapeKind::MaskedColor, 1, 1, 4, 0, 0, &buf).unwrap();
        assert_eq!(cur.rgba, vec![0, 255, 0, 255]);
        assert!(cur.xor_mask.is_empty());
    }

    #[test]
    fn masked_color_black_xor_pixels_are_all_transparent() {
        // 3x1 row: white-XOR, black-XOR, black-XOR: both blacks are no-ops
        // (x^0 == x) and become transparent; only the white carries the bit.
        let mut buf = vec![];
        buf.extend_from_slice(&[255, 255, 255, 0xFF]);
        buf.extend_from_slice(&[0, 0, 0, 0xFF]);
        buf.extend_from_slice(&[0, 0, 0, 0xFF]);
        let cur = mask_shape_to_rgba(MaskShapeKind::MaskedColor, 3, 1, 12, 0, 0, &buf).unwrap();
        assert_eq!(cur.rgba, vec![255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(cur.xor_mask, vec![0b1000_0000]);
    }

    #[test]
    fn shape_rejects_degenerate_geometry() {
        assert!(mask_shape_to_rgba(MaskShapeKind::Color, 0, 1, 4, 0, 0, &[0; 4]).is_none());
        // Buffer too small for declared layout.
        assert!(mask_shape_to_rgba(MaskShapeKind::Color, 4, 2, 16, 0, 0, &[0; 8]).is_none());
    }

    fn candidates() -> Vec<OutputCandidate> {
        // Two adapters: a physical GPU with two outputs, then a second
        // adapter with one.
        vec![
            OutputCandidate {
                adapter: "Red Hat QXL controller".to_string(),
                name: r"\\.\DISPLAY1".to_string(),
                gdi_name: "QXL monitor".to_string(),
                index_in_adapter: 0,
            },
            OutputCandidate {
                adapter: "Red Hat QXL controller".to_string(),
                name: r"\\.\DISPLAY2".to_string(),
                gdi_name: "QXL monitor".to_string(),
                index_in_adapter: 1,
            },
            OutputCandidate {
                adapter: "Some Other Adapter".to_string(),
                name: r"\\.\DISPLAY6".to_string(),
                gdi_name: "Some Other Monitor".to_string(),
                index_in_adapter: 0,
            },
        ]
    }

    #[test]
    fn index_selects_first_adapter_having_that_output() {
        let c = candidates();
        assert_eq!(select_output(&c, &OutputSelector::Index(0)), Some(0));
        assert_eq!(select_output(&c, &OutputSelector::Index(1)), Some(1));
        assert_eq!(select_output(&c, &OutputSelector::Index(2)), None);
    }

    #[test]
    fn name_matches_adapter_description_case_insensitively() {
        let c = candidates();
        let sel = OutputSelector::NameContains("some other".to_string());
        assert_eq!(select_output(&c, &sel), Some(2));
    }

    #[test]
    fn name_matches_output_device_name() {
        let c = candidates();
        let sel = OutputSelector::NameContains("display2".to_string());
        assert_eq!(select_output(&c, &sel), Some(1));
    }

    #[test]
    fn name_with_no_match_is_none() {
        let c = candidates();
        let sel = OutputSelector::NameContains("nvidia".to_string());
        assert_eq!(select_output(&c, &sel), None);
        assert_eq!(select_output(&[], &OutputSelector::Index(0)), None);
    }

    #[test]
    fn name_matches_gdi_device_string_when_dxgi_hides_the_adapter() {
        // On hosts rendering through the Microsoft Basic Render Driver,
        // DXGI reports the outputs under that generic adapter, so only the
        // GDI device string still identifies a specific monitor.
        let c = vec![
            OutputCandidate {
                adapter: "Microsoft Basic Render Driver".to_string(),
                name: r"\\.\DISPLAY1".to_string(),
                gdi_name: "Generic PnP Monitor".to_string(),
                index_in_adapter: 0,
            },
            OutputCandidate {
                adapter: "Microsoft Basic Render Driver".to_string(),
                name: r"\\.\DISPLAY2".to_string(),
                gdi_name: "Target Monitor".to_string(),
                index_in_adapter: 1,
            },
        ];
        let sel = OutputSelector::NameContains("Target Monitor".to_string());
        assert_eq!(select_output(&c, &sel), Some(1));
    }
}
