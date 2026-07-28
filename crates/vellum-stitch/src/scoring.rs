//! Overlap scoring.
//!
//! Two independent gates decide whether a frame is accepted, and both must be
//! preserved verbatim (see AGENTS.md "不可放宽的行为契约"):
//!
//! 1. Row-signature diff (`col_diff`) finds the vertical offset cheaply.
//! 2. Sparse RGB MAD (`pixel_overlap_diff`) rejects false periodic matches that
//!    row statistics rate as perfect (repeated cards, list rows, photos).
//!
//! Both have a "robust" variant that discards the noisiest 20% of rows. Those
//! are used only after the ordinary score has already failed, so a video strip
//! or blinking caret cannot lower the bar for the whole frame.

use crate::signature::{
    Cols, Sparse, content_bottom_ignore, content_top_ignore, overlap_window, trimmed_mean,
};

/// Absolute sparse-RGB MAD limit on the normal path.
pub const MAX_PIXEL_DIFF: f32 = 32.0;
/// Absolute sparse-RGB MAD limit on the robust path (stricter: rows were dropped).
pub const ROBUST_MAX_PIXEL_DIFF: f32 = 24.0;
/// Offline fusion treats larger per-channel deltas as local animation, not as
/// translucent background to be blended.
pub const FUSION_MAX_PIXEL_DELTA: i16 = 48;

/// Viewport-fixed bands to ignore while scoring.
#[derive(Debug, Clone, Default)]
pub struct Mask<'a> {
    /// Per-frame-row exclusion, length must equal both frame heights.
    pub rows: Option<&'a [bool]>,
    /// Per-sparse-column exclusion, length must equal the sparse column count.
    pub columns: Option<&'a [bool]>,
}

impl<'a> Mask<'a> {
    pub fn rows_only(rows: Option<&'a [bool]>) -> Self {
        Self {
            rows,
            columns: None,
        }
    }

    fn excluded_row_count(&self) -> usize {
        self.rows
            .map(|rows| rows.iter().filter(|v| **v).count())
            .unwrap_or(0)
    }
}

/// Which overlap rows survive the row mask, as (a_row, b_row) index pairs.
fn active_rows(
    a_len: usize,
    b_len: usize,
    a_start: usize,
    b_start: usize,
    top: usize,
    end: usize,
    rows: Option<&[bool]>,
) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(end - top);
    // Python only applies the row mask when it matches both frame heights;
    // an ambiguous mask is ignored rather than mis-indexed.
    let usable = rows.filter(|mask| mask.len() == a_len && a_len == b_len);
    for offset in top..end {
        let (ai, bi) = (a_start + offset, b_start + offset);
        if let Some(mask) = usable
            && (mask[ai] || mask[bi])
        {
            continue;
        }
        out.push((ai, bi));
    }
    out
}

/// Sparse columns to compare. Falls back to all columns when the mask would
/// leave fewer than 4 (too little signal to trust).
fn active_columns(count: usize, columns: Option<&[bool]>) -> Option<Vec<usize>> {
    let mask = columns?;
    if mask.len() != count {
        return None;
    }
    let kept: Vec<usize> = (0..count).filter(|i| !mask[*i]).collect();
    (kept.len() >= 4).then_some(kept)
}

/// Row-signature overlap score. LOWER is better; `INFINITY` when the confident
/// overlap is too small to trust.
pub fn col_diff(a: &Cols, b: &Cols, offset: i32, min_overlap: usize, mask: &Mask<'_>) -> f32 {
    let Some(window) = overlap_window(a.height, b.height, offset) else {
        return f32::INFINITY;
    };
    if window.length < min_overlap {
        return f32::INFINITY;
    }
    let top = content_top_ignore(window.length);
    let end = window.length - content_bottom_ignore(window.length);
    if end <= top {
        return f32::INFINITY;
    }
    let pairs = active_rows(
        a.height,
        b.height,
        window.a_start,
        window.b_start,
        top,
        end,
        mask.rows,
    );
    let required = if mask.rows.is_some() {
        min_overlap
            .saturating_sub(mask.excluded_row_count())
            .max(12)
    } else {
        min_overlap
    };
    if pairs.len() < required {
        return f32::INFINITY;
    }
    let mut total = 0f32;
    for (ai, bi) in &pairs {
        let (ra, rb) = (a.row(*ai), b.row(*bi));
        total += (ra[0] - rb[0]).abs() + (ra[1] - rb[1]).abs() + (ra[2] - rb[2]).abs();
    }
    total / (pairs.len() * 3) as f32
}

/// Row-signature score that tolerates a bounded locally-changing region.
/// Only used after `col_diff` has already rejected the frame.
pub fn robust_col_diff(
    a: &Cols,
    b: &Cols,
    offset: i32,
    min_overlap: usize,
    mask: &Mask<'_>,
) -> f32 {
    let Some(window) = overlap_window(a.height, b.height, offset) else {
        return f32::INFINITY;
    };
    if window.length < min_overlap {
        return f32::INFINITY;
    }
    let top = content_top_ignore(window.length);
    let end = window.length - content_bottom_ignore(window.length);
    if end <= top {
        return f32::INFINITY;
    }
    let pairs = active_rows(
        a.height,
        b.height,
        window.a_start,
        window.b_start,
        top,
        end,
        mask.rows,
    );
    let required = if mask.rows.is_some() {
        min_overlap
            .saturating_sub(mask.excluded_row_count())
            .max(12)
    } else {
        min_overlap
    };
    if pairs.len() < required {
        return f32::INFINITY;
    }
    let mut scores: Vec<f32> = pairs
        .iter()
        .map(|(ai, bi)| {
            let (ra, rb) = (a.row(*ai), b.row(*bi));
            ((ra[0] - rb[0]).abs() + (ra[1] - rb[1]).abs() + (ra[2] - rb[2]).abs()) / 3.0
        })
        .collect();
    trimmed_mean(&mut scores)
}

/// Absolute sparse-RGB MAD over the same ignored bands used for matching.
pub fn pixel_overlap_diff(a: &Sparse, b: &Sparse, offset: i32, mask: &Mask<'_>) -> f32 {
    match sparse_row_scores(a, b, offset, mask) {
        None => f32::INFINITY,
        Some(scores) if scores.is_empty() => f32::INFINITY,
        Some(scores) => scores.iter().sum::<f32>() / scores.len() as f32,
    }
}

/// Sparse-RGB MAD after dropping the noisiest 20% of rows.
pub fn robust_pixel_overlap_diff(a: &Sparse, b: &Sparse, offset: i32, mask: &Mask<'_>) -> f32 {
    match sparse_row_scores(a, b, offset, mask) {
        None => f32::INFINITY,
        Some(mut scores) if !scores.is_empty() => trimmed_mean(&mut scores),
        Some(_) => f32::INFINITY,
    }
}

/// Per-row mean absolute difference of the sparse overlap.
fn sparse_row_scores(a: &Sparse, b: &Sparse, offset: i32, mask: &Mask<'_>) -> Option<Vec<f32>> {
    let window = overlap_window(a.height, b.height, offset)?;
    let top = content_top_ignore(window.length);
    let end = window
        .length
        .checked_sub(content_bottom_ignore(window.length))?;
    if end <= top {
        return None;
    }
    let pairs = active_rows(
        a.height,
        b.height,
        window.a_start,
        window.b_start,
        top,
        end,
        mask.rows,
    );
    let count = a.columns.min(b.columns);
    let kept = active_columns(count, mask.columns);
    let columns: &[usize] = match &kept {
        Some(list) => list,
        None => &[],
    };

    let mut scores = Vec::with_capacity(pairs.len());
    for (ai, bi) in pairs {
        let (ra, rb) = (a.row(ai), b.row(bi));
        let mut total = 0u32;
        let mut samples = 0u32;
        if columns.is_empty() {
            for c in 0..count {
                let base = c * 3;
                for k in 0..3 {
                    total += u32::from(ra[base + k].abs_diff(rb[base + k]));
                }
                samples += 3;
            }
        } else {
            for &c in columns {
                let base = c * 3;
                for k in 0..3 {
                    total += u32::from(ra[base + k].abs_diff(rb[base + k]));
                }
                samples += 3;
            }
        }
        if samples == 0 {
            return None;
        }
        scores.push(total as f32 / samples as f32);
    }
    Some(scores)
}

/// Fraction of sparse pixels that changed perceptibly at zero offset.
/// A near-zero value means the view is static, so any "shift" is a false match.
pub fn pixel_change_fraction(a: &Sparse, b: &Sparse, mask: &Mask<'_>) -> f32 {
    let h = a.height.min(b.height);
    let w = a.columns.min(b.columns);
    if h == 0 || w == 0 {
        return 1.0;
    }
    let row_mask = mask.rows.filter(|rows| rows.len() == h);
    let kept = active_columns_for_change(w, mask.columns);
    let columns: Vec<usize> = match kept {
        Some(list) => list,
        None => (0..w).collect(),
    };
    if columns.is_empty() {
        return 1.0;
    }
    let rows: Vec<usize> = (0..h)
        .filter(|y| row_mask.map(|mask| !mask[*y]).unwrap_or(true))
        .collect();
    if rows.is_empty() {
        return 1.0;
    }
    let mut changed = 0usize;
    let mut total = 0usize;
    for y in rows {
        let (ra, rb) = (a.row(y), b.row(y));
        for &c in &columns {
            let base = c * 3;
            let delta = (0..3)
                .map(|k| ra[base + k].abs_diff(rb[base + k]))
                .max()
                .unwrap_or(0);
            if delta > 6 {
                changed += 1;
            }
            total += 1;
        }
    }
    if total == 0 {
        1.0
    } else {
        changed as f32 / total as f32
    }
}

/// `pixel_change_fraction` keeps every column when the mask does not apply,
/// and unlike matching it has no 4-column floor (it is a ratio, not a score).
fn active_columns_for_change(count: usize, columns: Option<&[bool]>) -> Option<Vec<usize>> {
    let mask = columns?;
    if mask.len() != count {
        return None;
    }
    Some((0..count).filter(|i| !mask[*i]).collect())
}

/// The shared "is this really motion?" gate used by both the live path and the
/// offline graph. Returns true when the match must be rejected.
pub fn is_false_motion(aligned: f32, stationary: f32, changed: f32, robust: bool) -> bool {
    let limit = if robust {
        ROBUST_MAX_PIXEL_DIFF
    } else {
        MAX_PIXEL_DIFF
    };
    changed < 0.012 || stationary <= aligned + 0.2 || aligned > limit
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature::{compute_cols, sample_pixels};
    use vellum_core::image::Rgb8;

    /// Deterministic pseudo-random page content, shifted vertically by `shift`.
    fn page(width: usize, height: usize, shift: usize) -> Rgb8 {
        let mut img = Rgb8::new(width, height);
        for y in 0..height {
            let source = y + shift;
            for x in 0..width {
                let v = ((source * 7919 + x * 104729) % 251) as u8;
                let row = img.row_mut(y);
                row[x * 3] = v;
                row[x * 3 + 1] = v.wrapping_mul(3);
                row[x * 3 + 2] = v.wrapping_add(17);
            }
        }
        img
    }

    #[test]
    fn identical_frames_score_zero_at_zero_offset() {
        let a = page(120, 200, 0);
        let cols = compute_cols(&sample_pixels(&a));
        assert_eq!(col_diff(&cols, &cols, 0, 50, &Mask::default()), 0.0);
        let sparse = sample_pixels(&a);
        assert_eq!(
            pixel_overlap_diff(&sparse, &sparse, 0, &Mask::default()),
            0.0
        );
    }

    #[test]
    fn true_scroll_offset_scores_best() {
        let a = page(120, 200, 0);
        let b = page(120, 200, 20);
        let ca = compute_cols(&sample_pixels(&a));
        let cb = compute_cols(&sample_pixels(&b));
        let truth = col_diff(&ca, &cb, 20, 50, &Mask::default());
        assert!(truth < 0.01, "expected near-zero at true offset: {truth}");
        for wrong in [5, 12, 19, 21, 40] {
            let score = col_diff(&ca, &cb, wrong, 50, &Mask::default());
            assert!(score > truth, "offset {wrong} scored {score} <= {truth}");
        }
    }

    #[test]
    fn upward_scroll_matches_with_negative_offset() {
        let a = page(120, 200, 20);
        let b = page(120, 200, 0);
        let ca = compute_cols(&sample_pixels(&a));
        let cb = compute_cols(&sample_pixels(&b));
        assert!(col_diff(&ca, &cb, -20, 50, &Mask::default()) < 0.01);
    }

    #[test]
    fn insufficient_overlap_is_infinite() {
        let a = page(120, 200, 0);
        let cols = compute_cols(&sample_pixels(&a));
        assert!(col_diff(&cols, &cols, 195, 50, &Mask::default()).is_infinite());
    }

    #[test]
    fn robust_path_tolerates_a_local_animation_strip() {
        let a = page(120, 200, 0);
        let mut b = page(120, 200, 20);
        // A 30-row "video" band that changes completely between frames.
        for y in 60..90 {
            for x in 0..120 {
                let row = b.row_mut(y);
                row[x * 3] = ((x * 31 + y * 17) % 255) as u8;
                row[x * 3 + 1] = 240;
                row[x * 3 + 2] = 12;
            }
        }
        let ca = compute_cols(&sample_pixels(&a));
        let cb = compute_cols(&sample_pixels(&b));
        let normal = col_diff(&ca, &cb, 20, 50, &Mask::default());
        let robust = robust_col_diff(&ca, &cb, 20, 50, &Mask::default());
        assert!(
            robust < normal,
            "robust {robust} should beat normal {normal}"
        );
        assert!(robust <= 9.0, "robust score must stay acceptable: {robust}");
    }

    #[test]
    fn sparse_pixel_check_rejects_a_periodic_false_match() {
        // A page of identical repeating rows: row signatures cannot tell the
        // difference between offsets, so the absolute pixel check must.
        let mut a = Rgb8::new(64, 200);
        for y in 0..200 {
            let value = if (y / 10) % 2 == 0 { 30 } else { 200 };
            for x in 0..64 {
                let row = a.row_mut(y);
                row[x * 3..x * 3 + 3].copy_from_slice(&[value, value, value]);
            }
        }
        let sparse = sample_pixels(&a);
        // Offset 20 lines the stripes back up perfectly even though nothing moved.
        let aligned = pixel_overlap_diff(&sparse, &sparse, 20, &Mask::default());
        let stationary = pixel_overlap_diff(&sparse, &sparse, 0, &Mask::default());
        let changed = pixel_change_fraction(&sparse, &sparse, &Mask::default());
        assert_eq!(changed, 0.0);
        assert!(
            is_false_motion(aligned, stationary, changed, false),
            "static periodic content must be rejected"
        );
    }

    #[test]
    fn row_mask_excludes_a_fixed_header_from_scoring() {
        let a = page(120, 200, 0);
        let mut b = page(120, 200, 20);
        // Fixed 40px header: identical in both frames at screen coordinates.
        for y in 0..40 {
            let source = a.row(y).to_vec();
            b.row_mut(y).copy_from_slice(&source);
        }
        let ca = compute_cols(&sample_pixels(&a));
        let cb = compute_cols(&sample_pixels(&b));
        let mut rows = vec![false; 200];
        rows[..40].fill(true);
        let masked = col_diff(&ca, &cb, 20, 50, &Mask::rows_only(Some(&rows)));
        let unmasked = col_diff(&ca, &cb, 20, 50, &Mask::default());
        assert!(
            masked <= unmasked,
            "masking a fixed header must not worsen the score"
        );
    }

    #[test]
    fn column_mask_falls_back_when_too_few_columns_remain() {
        let a = page(120, 200, 0);
        let sparse = sample_pixels(&a);
        let mut columns = vec![true; sparse.columns];
        columns[0] = false;
        columns[1] = false;
        let mask = Mask {
            rows: None,
            columns: Some(&columns),
        };
        // Only 2 usable columns: must fall back to all of them, not to garbage.
        let score = pixel_overlap_diff(&sparse, &sparse, 0, &mask);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn false_motion_gate_matches_python_conditions() {
        assert!(
            is_false_motion(1.0, 1.1, 0.5, false),
            "stationary <= aligned+0.2"
        );
        assert!(is_false_motion(1.0, 50.0, 0.005, false), "changed < 0.012");
        assert!(is_false_motion(33.0, 90.0, 0.5, false), "aligned > 32");
        assert!(!is_false_motion(25.0, 90.0, 0.5, false));
        // The robust path is stricter on absolute pixel agreement.
        assert!(is_false_motion(25.0, 90.0, 0.5, true));
    }
}
