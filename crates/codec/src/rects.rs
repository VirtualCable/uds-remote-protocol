//! Dirty-region detection and merging for rect-oriented codecs
//! (design doc §8.5).
//!
//! Pure Rust, no dependencies: 64 px tile diffing against the previous frame
//! plus the merge heuristic ported from rdphtml5's `RectSender` (dedup by
//! containment, merge on intersection or when the union area is under 1.15x
//! the sum of areas, bounding box on degenerate loads).

// Rect geometry math is full of small, provably in-range integer conversions.
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation
)]

/// Pixel-space rectangle (`left`/`top` inclusive, `right`/`bottom` exclusive).
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

    /// Area in pixels.
    #[must_use]
    pub fn area(&self) -> i64 {
        if self.is_empty() {
            0
        } else {
            i64::from(self.right - self.left) * i64::from(self.bottom - self.top)
        }
    }

    /// True when `self` fully contains `other`.
    #[must_use]
    pub fn contains(&self, other: &Self) -> bool {
        other.left >= self.left
            && other.right <= self.right
            && other.top >= self.top
            && other.bottom <= self.bottom
    }

    /// True when the rects intersect or touch.
    #[must_use]
    pub fn intersects(&self, other: &Self) -> bool {
        self.left <= other.right
            && other.left <= self.right
            && self.top <= other.bottom
            && other.top <= self.bottom
    }

    /// Smallest rect containing both.
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        Self {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }
}

/// Diff tile size in pixels. 64 keeps the scan memory-bandwidth bound and
/// the rect count low.
pub const DIFF_TILE: u32 = 64;

/// Maximum rects per frame before collapsing to the bounding box (the
/// per-rect container overhead dominates beyond this).
pub const MAX_RECTS: usize = 64;

/// Computes the dirty regions between two tight/padded BGRA frames of the
/// same geometry as merged rects. `prev == None` yields one full-screen rect.
#[must_use]
pub fn dirty_rects(
    prev: Option<&[u8]>,
    cur: &[u8],
    width: u32,
    height: u32,
    stride: usize,
) -> Vec<Rect> {
    let full = Rect {
        left: 0,
        top: 0,
        right: width as i32,
        bottom: height as i32,
    };
    let Some(prev) = prev else {
        return vec![full];
    };
    let (w, h) = (width as usize, height as usize);
    let tiles_x = w.div_ceil(DIFF_TILE as usize);
    let tiles_y = h.div_ceil(DIFF_TILE as usize);
    let mut dirty = vec![false; tiles_x * tiles_y];
    let mut any = false;
    for ty in 0..tiles_y {
        let y0 = ty * DIFF_TILE as usize;
        let rows = (DIFF_TILE as usize).min(h - y0);
        for tx in 0..tiles_x {
            let x0 = tx * DIFF_TILE as usize;
            let cols = (DIFF_TILE as usize).min(w - x0) * 4;
            let is_dirty = (0..rows).any(|r| {
                let off = (y0 + r) * stride + x0 * 4;
                prev[off..off + cols] != cur[off..off + cols]
            });
            if is_dirty {
                dirty[ty * tiles_x + tx] = true;
                any = true;
            }
        }
    }
    if !any {
        return Vec::new();
    }
    // Coalesce contiguous dirty tiles horizontally into strips, then merge.
    let mut rects = Vec::new();
    for ty in 0..tiles_y {
        let mut tx = 0;
        while tx < tiles_x {
            if !dirty[ty * tiles_x + tx] {
                tx += 1;
                continue;
            }
            let mut end = tx;
            while end + 1 < tiles_x && dirty[ty * tiles_x + end + 1] {
                end += 1;
            }
            rects.push(Rect {
                left: (tx * DIFF_TILE as usize) as i32,
                top: (ty * DIFF_TILE as usize) as i32,
                right: (((end + 1) * DIFF_TILE as usize).min(w)) as i32,
                bottom: (((ty + 1) * DIFF_TILE as usize).min(h)) as i32,
            });
            tx = end + 1;
        }
    }
    merge(rects)
}

/// Dedup by containment, merge on intersection or when the union area is
/// under 1.15x the sum of areas; degenerate loads collapse to the bounding
/// box.
#[must_use]
pub fn merge(mut rects: Vec<Rect>) -> Vec<Rect> {
    rects.retain(|r| !r.is_empty());
    if rects.len() <= 1 {
        return rects;
    }
    if rects.len() > 500 {
        return vec![bounding_all(&rects)];
    }
    let mut i = 0;
    while i < rects.len() {
        if (0..rects.len()).any(|j| i != j && rects[j].contains(&rects[i])) {
            rects.remove(i);
        } else {
            i += 1;
        }
    }
    let mut changed = true;
    while changed && rects.len() > 1 {
        changed = false;
        let mut i = 0;
        while i < rects.len() {
            let mut j = i + 1;
            while j < rects.len() {
                let merged = rects[i].union(&rects[j]);
                if rects[i].intersects(&rects[j])
                    || merged.area() < (rects[i].area() + rects[j].area()) * 115 / 100
                {
                    rects[i] = merged;
                    rects.remove(j);
                    changed = true;
                } else {
                    j += 1;
                }
            }
            i += 1;
        }
    }
    if rects.len() > MAX_RECTS {
        return vec![bounding_all(&rects)];
    }
    rects
}

fn bounding_all(rects: &[Rect]) -> Rect {
    rects.iter().skip(1).fold(rects[0], |acc, r| acc.union(r))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(l: i32, t: i32, r_: i32, b: i32) -> Rect {
        Rect {
            left: l,
            top: t,
            right: r_,
            bottom: b,
        }
    }

    #[test]
    fn no_changes_yields_no_rects() {
        let a = vec![7u8; 128 * 128 * 4];
        assert!(dirty_rects(Some(&a), &a, 128, 128, 128 * 4).is_empty());
    }

    #[test]
    fn first_frame_is_full_rect() {
        let a = vec![0u8; 64 * 64 * 4];
        assert_eq!(
            dirty_rects(None, &a, 64, 64, 64 * 4),
            vec![r(0, 0, 64, 64)]
        );
    }

    #[test]
    fn single_pixel_change_yields_one_tile() {
        let a = vec![0u8; 128 * 128 * 4];
        let mut b = a.clone();
        b[(70 * 128 + 70) * 4] = 255; // pixel (70,70) -> tile (1,1)
        let rects = dirty_rects(Some(&a), &b, 128, 128, 128 * 4);
        assert_eq!(rects, vec![r(64, 64, 128, 128)]);
    }

    #[test]
    fn distant_changes_stay_separate() {
        let a = vec![0u8; 256 * 128 * 4];
        let mut b = a.clone();
        b[0] = 1; // tile (0,0)
        b[(64 * 256 + 200) * 4] = 1; // tile (3,1)
        let rects = dirty_rects(Some(&a), &b, 256, 128, 256 * 4);
        assert_eq!(rects.len(), 2);
    }

    #[test]
    fn merge_dedups_contained() {
        let outer = r(0, 0, 100, 100);
        let inner = r(10, 10, 20, 20);
        assert_eq!(merge(vec![outer, inner]), vec![outer]);
        assert_eq!(merge(vec![inner, outer]), vec![outer]);
    }

    #[test]
    fn merge_unions_near_rects() {
        // Union area (200x100) < 1.15x sum (2x 100x100) -> merge.
        assert_eq!(
            merge(vec![r(0, 0, 100, 100), r(100, 0, 200, 100)]),
            vec![r(0, 0, 200, 100)]
        );
    }

    #[test]
    fn merge_keeps_distant_rects() {
        assert_eq!(merge(vec![r(0, 0, 10, 10), r(500, 500, 510, 510)]).len(), 2);
    }

    #[test]
    fn merge_collapses_overload_to_bounding_box() {
        let rects: Vec<Rect> = (0..20)
            .flat_map(|y| (0..20).map(move |x| r(x * 1000, y * 1000, x * 1000 + 10, y * 1000 + 10)))
            .collect();
        assert_eq!(merge(rects).len(), 1);
    }
}
