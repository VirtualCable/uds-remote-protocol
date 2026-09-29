//! Synthetic moving test pattern (development frames before real capture).
//!
//! Generates a BGRA gradient with a moving vertical bar, so both static
//! correctness (geometry, colors) and motion (frame-to-frame change) can be
//! verified at the far end of the channel.

use codec::VideoFrame;

/// Synthetic BGRA frame source: a gradient plus a moving white bar.
pub struct SyntheticSource {
    width: u32,
    height: u32,
    frame: u32,
    buf: Vec<u8>,
}

impl SyntheticSource {
    /// Creates a source for `width`x`height` frames (tight stride, `width*4`).
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            frame: 0,
            buf: vec![0u8; width as usize * height as usize * 4],
        }
    }

    /// Frame geometry `(width, height)`.
    #[must_use]
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Renders the next frame and returns a view of it.
    pub fn next_frame(&mut self) -> VideoFrame<'_> {
        const BAR_WIDTH: usize = 8;
        let w = self.width as usize;
        let h = self.height as usize;
        let bar_x = (self.frame as usize) % w;

        for y in 0..h {
            for x in 0..w {
                let px = &mut self.buf[(y * w + x) * 4..][..4];
                if x >= bar_x && x < bar_x + BAR_WIDTH {
                    px.copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]); // white bar
                } else {
                    let r = u8::try_from(x * 255 / w.max(1)).unwrap_or(0xFF);
                    let g = u8::try_from(y * 255 / h.max(1)).unwrap_or(0xFF);
                    px.copy_from_slice(&[0x80, g, r, 0xFF]); // BGRA gradient
                }
            }
        }
        self.frame = self.frame.wrapping_add(1);
        VideoFrame {
            width: self.width,
            height: self.height,
            stride: w * 4,
            data: &self.buf,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_has_tight_bgra_geometry() {
        let mut src = SyntheticSource::new(320, 240);
        let f = src.next_frame();
        assert_eq!((f.width, f.height), (320, 240));
        assert_eq!(f.stride, 320 * 4);
        assert_eq!(f.data.len(), 320 * 240 * 4);
    }

    #[test]
    fn same_frame_number_is_deterministic() {
        let mut a = SyntheticSource::new(64, 64);
        let mut b = SyntheticSource::new(64, 64);
        let fa = a.next_frame().data.to_vec();
        let fb = b.next_frame().data.to_vec();
        assert_eq!(fa, fb);
    }

    #[test]
    fn consecutive_frames_differ() {
        let mut src = SyntheticSource::new(64, 64);
        let f0 = src.next_frame().data.to_vec();
        let f1 = src.next_frame().data.to_vec();
        assert_ne!(f0, f1);
    }

    #[test]
    fn alpha_is_opaque() {
        let mut src = SyntheticSource::new(16, 16);
        let f = src.next_frame();
        assert!(f.data.as_chunks::<4>().0.iter().all(|px| px[3] == 0xFF));
    }
}
