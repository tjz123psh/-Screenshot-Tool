//! Canvas storage as an ordered list of row-blocks.
//!
//! The canvas is deliberately NOT one big array. Appending or prepending a
//! block is O(rows added); the single full-height copy happens once, in
//! `Canvas::flatten`. The Python original learned this the hard way: a
//! per-frame `np.vstack([canvas, new])` is O(canvas height) and made a long
//! scroll quadratic, which showed up as "the longer you scroll the more it
//! stutters" and then as dropped frames.
//!
//! The live preview thumbnail follows the same rule: each block is scaled once
//! when it is added, and the preview concatenates only the tail blocks needed
//! to fill the requested box. That keeps preview cost independent of total
//! canvas height.

use std::collections::VecDeque;

use vellum_core::image::Rgb8;

use crate::signature::{Cols, Sparse, compute_cols, matching_cols, sample_pixels};

/// Which edge a block was added to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Top,
    Bottom,
}

/// Thumbnail width in px, matching the Python `_preview_w`.
const PREVIEW_WIDTH: usize = 220;

struct MatchBlock {
    cols: Cols,
    pixels: Sparse,
}

pub struct Canvas {
    blocks: VecDeque<Rgb8>,
    thumbs: VecDeque<Rgb8>,
    /// Compact row signatures and sparse RGB rows in exactly the same block
    /// order as `blocks`. This is cheap to grow at either edge and lets the
    /// stitcher search already-captured content without flattening the canvas.
    matches: VecDeque<MatchBlock>,
    height: usize,
    width: usize,
    preview_enabled: bool,
}

impl Canvas {
    pub fn new(preview_enabled: bool) -> Self {
        Self {
            blocks: VecDeque::new(),
            thumbs: VecDeque::new(),
            matches: VecDeque::new(),
            height: 0,
            width: 0,
            preview_enabled,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn set_width(&mut self, width: usize) {
        self.width = width;
    }

    pub fn push(&mut self, block: Rgb8, side: Side) {
        if block.height == 0 {
            return;
        }

        // Index only the rows actually added to the canvas. Across a complete
        // long shot this processes one canvas-height worth of pixels, not one
        // viewport-height per captured frame.
        let pixels = sample_pixels(&block);
        let matching = MatchBlock {
            cols: compute_cols(&pixels),
            pixels,
        };

        self.height += block.height;
        if self.preview_enabled {
            let thumb = scale_block(&block, self.width.max(block.width));
            match side {
                Side::Bottom => self.thumbs.push_back(thumb),
                Side::Top => self.thumbs.push_front(thumb),
            }
        }
        match side {
            Side::Bottom => {
                self.blocks.push_back(block);
                self.matches.push_back(matching);
            }
            Side::Top => {
                self.blocks.push_front(block);
                self.matches.push_front(matching);
            }
        }
    }

    /// Flatten the row signatures plus only the sparse columns used by the
    /// coarse full-canvas ranker. Exact sparse verification extracts one
    /// viewport with `matching_pixels_window`, so a miss never copies the whole
    /// 96-column canvas index.
    pub(crate) fn matching_snapshot(
        &self,
        excluded_columns: Option<&[bool]>,
        probe_columns: &[usize],
    ) -> Option<(Cols, Sparse)> {
        let first = self.matches.front()?;
        let columns = first.pixels.columns;
        if probe_columns.is_empty() || probe_columns.iter().any(|column| *column >= columns) {
            return None;
        }
        let mut cols = Vec::with_capacity(self.height * 3);
        let mut pixels = Vec::with_capacity(self.height * probe_columns.len() * 3);

        for block in &self.matches {
            debug_assert_eq!(block.cols.height, block.pixels.height);
            debug_assert_eq!(block.pixels.columns, columns);
            if block.cols.height != block.pixels.height || block.pixels.columns != columns {
                return None;
            }
            let block_cols = matching_cols(&block.cols, &block.pixels, excluded_columns);
            cols.extend_from_slice(&block_cols.data);
            for row in 0..block.pixels.height {
                let source = block.pixels.row(row);
                for column in probe_columns {
                    let base = column * 3;
                    pixels.extend_from_slice(&source[base..base + 3]);
                }
            }
        }
        debug_assert_eq!(cols.len(), self.height * 3);
        debug_assert_eq!(pixels.len(), self.height * probe_columns.len() * 3);

        Some((
            Cols {
                height: self.height,
                data: cols,
            },
            Sparse {
                height: self.height,
                columns: probe_columns.len(),
                data: pixels,
            },
        ))
    }

    /// Copy one exact sparse-RGB viewport from the block index. The amount of
    /// data is bounded by frame height instead of accumulated canvas height.
    pub(crate) fn matching_pixels_window(&self, start: usize, height: usize) -> Option<Sparse> {
        let end = start.checked_add(height)?;
        if end > self.height {
            return None;
        }
        let columns = self.matches.front()?.pixels.columns;
        let mut data = Vec::with_capacity(height * columns * 3);
        let mut block_start = 0usize;
        for block in &self.matches {
            if block.pixels.columns != columns {
                return None;
            }
            let block_end = block_start + block.pixels.height;
            let copy_start = start.max(block_start);
            let copy_end = end.min(block_end);
            if copy_start < copy_end {
                for row in copy_start - block_start..copy_end - block_start {
                    data.extend_from_slice(block.pixels.row(row));
                }
            }
            if block_end >= end {
                break;
            }
            block_start = block_end;
        }
        (data.len() == height * columns * 3).then_some(Sparse {
            height,
            columns,
            data,
        })
    }

    /// The deferred full-height copy. Called once, when producing the result.
    pub fn flatten(&self) -> Option<Rgb8> {
        if self.blocks.is_empty() {
            return None;
        }
        if self.blocks.len() == 1 {
            return Some(self.blocks[0].clone());
        }
        let blocks: Vec<Rgb8> = self.blocks.iter().cloned().collect();
        Some(Rgb8::vstack(&blocks))
    }

    /// Down-scaled snapshot of the newest canvas content, fitted into the box.
    ///
    /// Only the tail thumbnail blocks needed to fill `max_h` are touched, so
    /// this is O(preview size) rather than O(canvas height).
    pub fn preview(&self, max_w: usize, max_h: usize) -> Option<Rgb8> {
        if self.thumbs.is_empty() || max_w == 0 || max_h == 0 {
            return None;
        }
        let target_rows = self.preview_rows_for(max_w, max_h);
        let mut picked: Vec<Rgb8> = Vec::new();
        let mut total = 0usize;
        for block in self.thumbs.iter().rev() {
            total += block.height;
            picked.push(block.clone());
            if total >= target_rows {
                break;
            }
        }
        picked.reverse();
        let mut live = if picked.len() == 1 {
            picked.remove(0)
        } else {
            Rgb8::vstack(&picked)
        };
        if total > target_rows {
            live = live.rows_slice(total - target_rows, total);
        }
        if live.is_empty() {
            return None;
        }
        let ratio = (max_w as f64 / live.width as f64)
            .min(max_h as f64 / live.height as f64)
            .min(1.0);
        if ratio < 1.0 {
            let w = ((live.width as f64 * ratio) as usize).max(1);
            let h = ((live.height as f64 * ratio) as usize).max(1);
            live = live.resize(w, h);
        }
        Some(live)
    }

    /// How many thumbnail rows fill a `max_w` x `max_h` box.
    ///
    /// Thumbnails are stored at `min(canvas_w, PREVIEW_WIDTH)`; fitting into the
    /// box scales that width to `min(thumb_w, max_w)` and the height scales the
    /// same way, so `max_h * thumb_w / min(thumb_w, max_w)` rows are needed.
    fn preview_rows_for(&self, max_w: usize, max_h: usize) -> usize {
        let tw = if self.width == 0 {
            PREVIEW_WIDTH
        } else {
            self.width.min(PREVIEW_WIDTH)
        }
        .max(1);
        let fit_w = tw.min(max_w);
        if fit_w == 0 {
            return max_h;
        }
        max_h.max(((max_h * tw) as f64 / fit_w as f64).round() as usize)
    }
}

/// Scale one block to preview width, preserving aspect ratio.
fn scale_block(block: &Rgb8, canvas_width: usize) -> Rgb8 {
    let tw = canvas_width.clamp(1, PREVIEW_WIDTH);
    let scale = canvas_width as f64 / tw as f64;
    let th = ((block.height as f64 / scale).round() as usize).max(1);
    block.resize(tw, th)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band(width: usize, height: usize, value: u8) -> Rgb8 {
        Rgb8::from_raw(width, height, vec![value; width * height * 3])
    }

    #[test]
    fn flatten_preserves_block_order_both_directions() {
        let mut canvas = Canvas::new(false);
        canvas.set_width(4);
        canvas.push(band(4, 2, 100), Side::Bottom);
        canvas.push(band(4, 3, 200), Side::Bottom);
        canvas.push(band(4, 1, 50), Side::Top);
        let flat = canvas.flatten().unwrap();
        assert_eq!(flat.height, 6);
        assert_eq!(flat.pixel(0, 0), [50, 50, 50]);
        assert_eq!(flat.pixel(0, 1), [100, 100, 100]);
        assert_eq!(flat.pixel(0, 5), [200, 200, 200]);
    }

    #[test]
    fn height_tracks_pushes_without_flattening() {
        let mut canvas = Canvas::new(false);
        canvas.set_width(2);
        for _ in 0..50 {
            canvas.push(band(2, 4, 9), Side::Bottom);
        }
        assert_eq!(canvas.height(), 200);
    }

    #[test]
    fn matching_snapshot_preserves_rows_added_at_both_edges() {
        let mut canvas = Canvas::new(false);
        canvas.set_width(4);
        canvas.push(band(4, 2, 100), Side::Bottom);
        canvas.push(band(4, 1, 200), Side::Bottom);
        canvas.push(band(4, 1, 50), Side::Top);

        let (cols, pixels) = canvas.matching_snapshot(None, &[0, 3]).unwrap();
        assert_eq!(cols.height, 4);
        assert_eq!(pixels.height, 4);
        assert_eq!(pixels.columns, 2);
        assert_eq!(pixels.row(0)[0], 50);
        assert_eq!(pixels.row(1)[0], 100);
        assert_eq!(pixels.row(3)[0], 200);

        let window = canvas.matching_pixels_window(2, 2).unwrap();
        assert_eq!(window.height, 2);
        assert_eq!(window.row(0)[0], 100);
        assert_eq!(window.row(1)[0], 200);
    }

    #[test]
    fn empty_canvas_has_no_result() {
        assert!(Canvas::new(true).flatten().is_none());
        assert!(Canvas::new(true).preview(100, 100).is_none());
    }

    #[test]
    fn preview_returns_scaled_tail_content() {
        let mut canvas = Canvas::new(true);
        canvas.set_width(400);
        canvas.push(band(400, 300, 10), Side::Bottom);
        canvas.push(band(400, 300, 20), Side::Bottom);
        let preview = canvas.preview(100, 60).unwrap();
        assert!(preview.width <= 100 && preview.height <= 60);
        // The newest (value 20) content must dominate the tail preview.
        assert_eq!(preview.pixel(preview.width / 2, preview.height - 1)[0], 20);
    }

    #[test]
    fn zero_height_block_is_ignored() {
        let mut canvas = Canvas::new(true);
        canvas.set_width(4);
        canvas.push(band(4, 0, 1), Side::Bottom);
        assert!(canvas.is_empty());
        assert_eq!(canvas.height(), 0);
    }
}
