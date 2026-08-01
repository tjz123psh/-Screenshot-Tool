//! Row-signature core: the primitives every match decision is built on.
//!
//! Ported 1:1 from `vellum/longshot/stitcher.py`'s numpy helpers. Numeric
//! behaviour must stay identical, so the accumulation order and the f32 width
//! of the signature values are deliberate, not incidental.
//!
//! Two representations exist per frame:
//! * `Cols`   - [H, 3] row signatures (mean*2, contrast, bright+edges)
//! * `Sparse` - up to 96 equally spaced RGB columns, kept for the absolute
//!   pixel check that row statistics alone cannot provide.

use vellum_core::image::Rgb8;

/// Max sparse columns retained per frame; enough to characterise a row.
pub const SAMPLE_COLUMNS: usize = 96;

/// Row signatures for one frame: 3 f32 per row.
#[derive(Clone, Debug, PartialEq)]
pub struct Cols {
    pub height: usize,
    /// Flat `height * 3` values.
    pub data: Vec<f32>,
}

impl Cols {
    #[inline]
    pub fn row(&self, y: usize) -> &[f32] {
        &self.data[y * 3..y * 3 + 3]
    }
}

/// Sparse RGB samples: `height` rows x `columns` columns x 3 channels.
#[derive(Clone, Debug, PartialEq)]
pub struct Sparse {
    pub height: usize,
    pub columns: usize,
    pub data: Vec<u8>,
}

impl Sparse {
    #[inline]
    pub fn row(&self, y: usize) -> &[u8] {
        let stride = self.columns * 3;
        &self.data[y * stride..(y + 1) * stride]
    }

    /// Keep only the columns where `keep` is true (used to drop fixed sidebars).
    pub fn select_columns(&self, keep: &[bool]) -> Sparse {
        let kept: Vec<usize> = (0..self.columns).filter(|i| keep[*i]).collect();
        let mut data = Vec::with_capacity(self.height * kept.len() * 3);
        for y in 0..self.height {
            let row = self.row(y);
            for &c in &kept {
                data.extend_from_slice(&row[c * 3..c * 3 + 3]);
            }
        }
        Sparse {
            height: self.height,
            columns: kept.len(),
            data,
        }
    }
}

/// Equally spaced column indices, identical to numpy's integer arithmetic:
/// `arange(count) * (width - 1) // (count - 1)`.
pub fn sample_column_indices(width: usize) -> Vec<usize> {
    let count = width.clamp(1, SAMPLE_COLUMNS);
    if count == 1 {
        return vec![0];
    }
    (0..count).map(|i| i * (width - 1) / (count - 1)).collect()
}

pub fn sample_pixels(frame: &Rgb8) -> Sparse {
    let indices = sample_column_indices(frame.width);
    let mut data = Vec::with_capacity(frame.height * indices.len() * 3);
    for y in 0..frame.height {
        let row = frame.row(y);
        for &x in &indices {
            data.extend_from_slice(&row[x * 3..x * 3 + 3]);
        }
    }
    Sparse {
        height: frame.height,
        columns: indices.len(),
        data,
    }
}

/// Reduce each row to `[mean*2, contrast, bright + edges]`.
///
/// Mirrors `_compute_cols_from_pixels`: luma via BT.601 weights, mean over the
/// sampled columns, mean absolute deviation as contrast, the positive-only
/// excess above 8 as `bright`, and the mean absolute horizontal gradient as
/// `edges`.
pub fn compute_cols(pixels: &Sparse) -> Cols {
    let count = pixels.columns;
    let mut data = Vec::with_capacity(pixels.height * 3);
    let mut luma = vec![0f32; count];

    for y in 0..pixels.height {
        let row = pixels.row(y);
        let mut sum = 0f32;
        for (x, value) in luma.iter_mut().enumerate() {
            let base = x * 3;
            let g = 0.299 * f32::from(row[base])
                + 0.587 * f32::from(row[base + 1])
                + 0.114 * f32::from(row[base + 2]);
            *value = g;
            sum += g;
        }
        let mean = sum / count as f32;

        let mut contrast_sum = 0f32;
        let mut bright_sum = 0f32;
        for &g in &luma {
            let centered = g - mean;
            contrast_sum += centered.abs();
            bright_sum += (centered - 8.0).max(0.0);
        }
        let contrast = contrast_sum / count as f32;
        let bright = bright_sum / count as f32;

        let edges = if count > 1 {
            let mut acc = 0f32;
            for x in 1..count {
                acc += (luma[x] - luma[x - 1]).abs();
            }
            acc / (count - 1) as f32
        } else {
            0.0
        };

        data.push(mean * 2.0);
        data.push(contrast);
        data.push(bright + edges);
    }

    Cols {
        height: pixels.height,
        data,
    }
}

/// Row signatures restricted to the non-fixed sparse columns. Falls back to
/// the precomputed signatures when the mask is absent or leaves too little
/// signal. Live history, full-canvas recovery and offline matching must share
/// this rule.
pub fn matching_cols(fallback: &Cols, pixels: &Sparse, excluded_columns: Option<&[bool]>) -> Cols {
    let Some(mask) = excluded_columns else {
        return fallback.clone();
    };
    if mask.len() != pixels.columns {
        return fallback.clone();
    }
    let keep: Vec<bool> = mask.iter().map(|value| !*value).collect();
    if keep.iter().filter(|value| **value).count() < 4 {
        return fallback.clone();
    }
    compute_cols(&pixels.select_columns(&keep))
}

/// Tiny 18x24 luma grid used only to detect a completely unmoved view.
pub fn frame_signature(frame: &Rgb8) -> Vec<u8> {
    const COLS: usize = 18;
    const ROWS: usize = 24;
    let (h, w) = (frame.height, frame.width);
    if h == 0 || w == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(COLS * ROWS);
    for r in 0..ROWS {
        let y = ((r * h) / ROWS).min(h - 1);
        let row = frame.row(y);
        for c in 0..COLS {
            let x = ((c * w) / COLS).min(w - 1);
            let base = x * 3;
            let g = 0.299 * f64::from(row[base])
                + 0.587 * f64::from(row[base + 1])
                + 0.114 * f64::from(row[base + 2]);
            // numpy rint: round half to even.
            out.push(round_half_even(g) as u8);
        }
    }
    out
}

fn round_half_even(value: f64) -> f64 {
    let floor = value.floor();
    let diff = value - floor;
    if (diff - 0.5).abs() < f64::EPSILON {
        if (floor as i64) % 2 == 0 {
            floor
        } else {
            floor + 1.0
        }
    } else {
        value.round()
    }
}

/// True when two frame signatures are effectively the same view.
pub fn is_duplicate(previous: &[u8], current: &[u8]) -> bool {
    if previous.len() != current.len() || previous.is_empty() {
        return false;
    }
    let mut sum = 0u32;
    let mut max = 0u32;
    for (a, b) in previous.iter().zip(current) {
        let d = u32::from(a.abs_diff(*b));
        sum += d;
        max = max.max(d);
    }
    let mean = f64::from(sum) / previous.len() as f64;
    mean <= 1.1 && max <= 4
}

/// A frame is truly static only when both the cheap luma prefilter and the
/// denser sparse-RGB index agree. A viewport-fixed wallpaper can dominate the
/// 18x24 grid while sparse terminal text scrolls between its sample rows.
pub fn is_static_view(
    previous_signature: &[u8],
    current_signature: &[u8],
    previous_pixels: &Sparse,
    current_pixels: &Sparse,
) -> bool {
    is_duplicate(previous_signature, current_signature) && previous_pixels == current_pixels
}

/// Rows ignored at the top of an overlap so scroll inertia does not poison it.
pub fn content_top_ignore(length: usize) -> usize {
    if length < 80 {
        0
    } else {
        (length / 4).min((length / 10).max(16))
    }
}

/// Rows ignored at the bottom of an overlap (fade-in / kinetic tail).
pub fn content_bottom_ignore(length: usize) -> usize {
    if length < 80 {
        0
    } else {
        (length / 4).min((length * 8 / 100).max(16))
    }
}

pub fn effective_min_overlap(frame_height: usize) -> usize {
    (frame_height / 4).clamp(12, 100)
}

/// Overlap window for a signed offset: which rows of `a` and `b` align, and
/// how many rows overlap in total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overlap {
    pub a_start: usize,
    pub b_start: usize,
    pub length: usize,
}

pub fn overlap_window(h1: usize, h2: usize, offset: i32) -> Option<Overlap> {
    if offset >= 0 {
        let off = offset as usize;
        if off > h1 {
            return None;
        }
        let length = (h1 - off).min(h2);
        (length > 0).then_some(Overlap {
            a_start: off,
            b_start: 0,
            length,
        })
    } else {
        let off = (-offset) as usize;
        if off > h2 {
            return None;
        }
        let length = h1.min(h2 - off);
        (length > 0).then_some(Overlap {
            a_start: 0,
            b_start: off,
            length,
        })
    }
}

/// Signed offsets to try, nearest the prediction first and fanning outward.
/// A steady scroll normally matches on the first probe.
pub fn offset_candidates(max_offset: i32, predict: i32) -> impl Iterator<Item = i32> {
    let predict = predict.clamp(-max_offset, max_offset);
    std::iter::once(predict).chain((1..=2 * max_offset).flat_map(move |delta| {
        let up = predict + delta;
        let down = predict - delta;
        [
            (up <= max_offset).then_some(up),
            (down >= -max_offset).then_some(down),
        ]
        .into_iter()
        .flatten()
    }))
}

/// Mean of the smallest 80% of `scores`, used by both robust paths.
/// Mirrors `np.partition(scores, keep - 1)[:keep].mean()`.
pub fn trimmed_mean(scores: &mut [f32]) -> f32 {
    let len = scores.len();
    if len == 0 {
        return f32::INFINITY;
    }
    let keep = ((len * 4) / 5).max(1);
    if keep == len {
        return scores.iter().sum::<f32>() / len as f32;
    }
    scores.select_nth_unstable_by(keep - 1, |a, b| a.total_cmp(b));
    scores[..keep].iter().sum::<f32>() / keep as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: usize, height: usize, f: impl Fn(usize, usize) -> [u8; 3]) -> Rgb8 {
        let mut img = Rgb8::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let px = f(x, y);
                let row = img.row_mut(y);
                row[x * 3..x * 3 + 3].copy_from_slice(&px);
            }
        }
        img
    }

    #[test]
    fn sample_columns_match_numpy_arithmetic() {
        assert_eq!(sample_column_indices(1), vec![0]);
        let indices = sample_column_indices(4);
        assert_eq!(indices, vec![0, 1, 2, 3]);
        let wide = sample_column_indices(1000);
        assert_eq!(wide.len(), SAMPLE_COLUMNS);
        assert_eq!(*wide.first().unwrap(), 0);
        assert_eq!(*wide.last().unwrap(), 999);
    }

    #[test]
    fn uniform_row_has_zero_contrast_and_edges() {
        let img = frame(32, 4, |_, _| [120, 120, 120]);
        let cols = compute_cols(&sample_pixels(&img));
        for y in 0..4 {
            let row = cols.row(y);
            assert!((row[0] - 240.0).abs() < 0.01, "mean*2 = {}", row[0]);
            assert!(row[1].abs() < 0.001);
            assert!(row[2].abs() < 0.001);
        }
    }

    #[test]
    fn edges_respond_to_horizontal_structure() {
        let flat = frame(64, 1, |_, _| [100, 100, 100]);
        let striped = frame(64, 1, |x, _| {
            if x % 2 == 0 {
                [0, 0, 0]
            } else {
                [255, 255, 255]
            }
        });
        let flat_cols = compute_cols(&sample_pixels(&flat));
        let striped_cols = compute_cols(&sample_pixels(&striped));
        assert!(striped_cols.row(0)[2] > flat_cols.row(0)[2] + 50.0);
    }

    #[test]
    fn static_view_requires_sparse_pixels_to_agree() {
        let signature = vec![100; 18 * 24];
        let pixels = Sparse {
            height: 1,
            columns: 1,
            data: vec![10, 20, 30],
        };
        let mut moved = pixels.clone();
        moved.data[0] = 11;

        assert!(is_static_view(&signature, &signature, &pixels, &pixels));
        assert!(!is_static_view(&signature, &signature, &pixels, &moved));
    }

    #[test]
    fn duplicate_detection_matches_python_thresholds() {
        let a = vec![100u8; 432];
        assert!(is_duplicate(&a, &a));
        let mut b = a.clone();
        b[0] = 104; // max diff 4, mean far below 1.1 -> still duplicate
        assert!(is_duplicate(&a, &b));
        let mut c = a.clone();
        c[0] = 105; // max diff 5 -> not duplicate
        assert!(!is_duplicate(&a, &c));
        assert!(!is_duplicate(&[], &[]));
    }

    #[test]
    fn ignore_bands_match_python_formulas() {
        assert_eq!(content_top_ignore(79), 0);
        assert_eq!(content_bottom_ignore(79), 0);
        assert_eq!(content_top_ignore(80), 16);
        assert_eq!(content_bottom_ignore(80), 16);
        assert_eq!(content_top_ignore(400), 40);
        assert_eq!(content_bottom_ignore(400), 32);
        // The length/4 cap dominates for mid-size overlaps.
        assert_eq!(content_top_ignore(100), 16);
    }

    #[test]
    fn min_overlap_is_clamped_between_12_and_100() {
        assert_eq!(effective_min_overlap(8), 12);
        assert_eq!(effective_min_overlap(200), 50);
        assert_eq!(effective_min_overlap(4000), 100);
    }

    #[test]
    fn overlap_window_handles_both_directions() {
        let down = overlap_window(100, 100, 20).unwrap();
        assert_eq!(
            (down.a_start, down.b_start, down.length),
            (20, 0, 80),
            "positive offset aligns a[20:] with b[:80]"
        );
        let up = overlap_window(100, 100, -20).unwrap();
        assert_eq!((up.a_start, up.b_start, up.length), (0, 20, 80));
        assert!(overlap_window(100, 100, 100).is_none());
    }

    #[test]
    fn offset_candidates_start_at_prediction_and_fan_out() {
        let seq: Vec<i32> = offset_candidates(5, 2).take(5).collect();
        assert_eq!(seq, vec![2, 3, 1, 4, 0]);
        let all: Vec<i32> = offset_candidates(2, 0).collect();
        assert_eq!(all, vec![0, 1, -1, 2, -2]);
    }

    #[test]
    fn trimmed_mean_drops_the_noisiest_fifth() {
        let mut scores = vec![1.0, 1.0, 1.0, 1.0, 100.0];
        assert_eq!(trimmed_mean(&mut scores), 1.0);
        let mut small = vec![4.0];
        assert_eq!(trimmed_mean(&mut small), 4.0);
    }

    #[test]
    fn frame_signature_is_stable_and_sized() {
        let img = frame(90, 70, |x, y| [(x % 256) as u8, (y % 256) as u8, 0]);
        let sig = frame_signature(&img);
        assert_eq!(sig.len(), 18 * 24);
        assert_eq!(sig, frame_signature(&img));
    }
}
