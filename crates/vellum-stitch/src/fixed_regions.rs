//! Multi-frame detection of viewport-fixed edge bands (sticky headers, footers,
//! sidebars).
//!
//! Ported from `vellum/longshot/fixed_regions.py`. The conservative gates are
//! the whole point: a band is only exposed after three confirmed scrolling
//! transitions, must hold in 80% of recent observations, and can never cover
//! more than 45% of the viewport. Loosening any of those turns "the user paused
//! scrolling" into "the page is four giant pieces of chrome".

use crate::signature::Sparse;

/// Observation window length. Older observations fall out of the ring.
const WINDOW: usize = 6;
/// Observations required before any band is reported.
const READY_AFTER: usize = 3;
/// Fraction of observations that must agree for a row/column to count as fixed.
const CONSENSUS: f32 = 0.80;
/// Per-channel delta below which a sparse pixel counts as unchanged.
const UNCHANGED_DELTA: i16 = 4;
/// A row needs nearly every sparse column to agree.
const ROW_AGREEMENT: f32 = 0.97;
/// Columns use a lower bar so a full-height sidebar survives a fixed header and
/// small animated controls inside the sidebar.
const COLUMN_AGREEMENT: f32 = 0.90;
/// Never classify more than this fraction of an axis as fixed chrome.
const MAX_BAND_FRACTION: f32 = 0.45;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FixedBands {
    pub top: usize,
    pub bottom: usize,
    pub left: usize,
    pub right: usize,
}

impl FixedBands {
    pub fn any(&self) -> bool {
        self.top != 0 || self.bottom != 0 || self.left != 0 || self.right != 0
    }
}

#[derive(Debug)]
pub struct FixedRegionDetector {
    height: usize,
    sample_columns: usize,
    row_observations: Vec<Vec<bool>>,
    column_observations: Vec<Vec<bool>>,
}

impl FixedRegionDetector {
    pub fn new(height: usize, sample_columns: usize) -> Self {
        Self {
            height,
            sample_columns,
            row_observations: Vec::with_capacity(WINDOW),
            column_observations: Vec::with_capacity(WINDOW),
        }
    }

    pub fn ready(&self) -> bool {
        self.row_observations.len() >= READY_AFTER
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn sample_columns(&self) -> usize {
        self.sample_columns
    }

    /// Record one scrolling transition. Shape mismatches are ignored, exactly
    /// like the Python version, so a resized output cannot poison the window.
    pub fn observe(&mut self, previous: &Sparse, current: &Sparse) {
        if previous.height != current.height || previous.columns != current.columns {
            return;
        }
        if previous.height != self.height || previous.columns != self.sample_columns {
            return;
        }
        let columns = self.sample_columns;
        if columns == 0 || self.height == 0 {
            return;
        }

        let mut row_flags = Vec::with_capacity(self.height);
        // Per-column unchanged counts, accumulated across rows for the column axis.
        let mut column_unchanged = vec![0usize; columns];

        for y in 0..self.height {
            let (pr, cr) = (previous.row(y), current.row(y));
            let mut unchanged_in_row = 0usize;
            for (count, (p, c)) in column_unchanged
                .iter_mut()
                .zip(pr.chunks_exact(3).zip(cr.chunks_exact(3)))
            {
                let delta = (0..3)
                    .map(|i| (i16::from(p[i]) - i16::from(c[i])).abs())
                    .max()
                    .unwrap_or(0);
                if delta <= UNCHANGED_DELTA {
                    unchanged_in_row += 1;
                    *count += 1;
                }
            }
            row_flags.push(unchanged_in_row as f32 / columns as f32 >= ROW_AGREEMENT);
        }

        let column_flags: Vec<bool> = column_unchanged
            .iter()
            .map(|count| *count as f32 / self.height as f32 >= COLUMN_AGREEMENT)
            .collect();

        push_bounded(&mut self.row_observations, row_flags);
        push_bounded(&mut self.column_observations, column_flags);
    }

    pub fn row_mask(&self) -> Vec<bool> {
        if !self.ready() {
            return vec![false; self.height];
        }
        edge_mask(&consensus(&self.row_observations, self.height), 4)
    }

    pub fn column_mask(&self) -> Vec<bool> {
        if !self.ready() {
            return vec![false; self.sample_columns];
        }
        edge_mask(
            &consensus(&self.column_observations, self.sample_columns),
            2,
        )
    }

    /// Convert masks into pixel bands for a canvas of `width` pixels.
    pub fn bands(&self, width: usize) -> FixedBands {
        let rows = self.row_mask();
        let columns = self.column_mask();
        FixedBands {
            top: leading_count(rows.iter().copied()),
            bottom: leading_count(rows.iter().rev().copied()),
            left: sample_boundary(
                width,
                self.sample_columns,
                leading_count(columns.iter().copied()),
            ),
            right: sample_boundary(
                width,
                self.sample_columns,
                leading_count(columns.iter().rev().copied()),
            ),
        }
    }
}

fn push_bounded(store: &mut Vec<Vec<bool>>, value: Vec<bool>) {
    if store.len() == WINDOW {
        store.remove(0);
    }
    store.push(value);
}

fn consensus(observations: &[Vec<bool>], len: usize) -> Vec<bool> {
    let count = observations.len() as f32;
    (0..len)
        .map(|i| {
            let agree = observations
                .iter()
                .filter(|obs| obs.get(i).copied().unwrap_or(false))
                .count() as f32;
            agree / count >= CONSENSUS
        })
        .collect()
}

/// Keep only leading/trailing runs, and only when they are neither too thin to
/// be real chrome nor large enough to swallow the viewport.
fn edge_mask(values: &[bool], min_band: usize) -> Vec<bool> {
    let mut mask = vec![false; values.len()];
    if values.is_empty() {
        return mask;
    }
    let leading = leading_count(values.iter().copied());
    let trailing = leading_count(values.iter().rev().copied());
    let max_band = min_band.max((values.len() as f32 * MAX_BAND_FRACTION) as usize);

    if (min_band..=max_band).contains(&leading) {
        mask[..leading].fill(true);
    }
    if (min_band..=max_band).contains(&trailing) {
        mask[values.len() - trailing..].fill(true);
    }
    if mask.iter().all(|v| *v) {
        mask.fill(false);
    }
    mask
}

fn leading_count(values: impl Iterator<Item = bool>) -> usize {
    let mut count = 0;
    for value in values {
        if !value {
            break;
        }
        count += 1;
    }
    count
}

/// Map a count of fixed sparse columns back to a pixel boundary, using the
/// midpoint between the last fixed sample and the first free one.
fn sample_boundary(width: usize, samples: usize, count: usize) -> usize {
    if width == 0 || samples == 0 || count == 0 {
        return 0;
    }
    if count >= samples {
        return width;
    }
    let denom = samples.saturating_sub(1).max(1);
    let xs = |i: usize| i * (width - 1) / denom;
    (xs(count - 1) + xs(count)).div_ceil(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sparse(height: usize, columns: usize, fill: u8) -> Sparse {
        Sparse {
            height,
            columns,
            data: vec![fill; height * columns * 3],
        }
    }

    /// A header band that never changes, over content that always changes.
    fn header_pair(height: usize, columns: usize, header: usize, tick: u8) -> (Sparse, Sparse) {
        let mut a = sparse(height, columns, 0);
        let mut b = sparse(height, columns, 0);
        for y in 0..height {
            let value = if y < header { 200 } else { tick };
            let other = if y < header {
                200
            } else {
                tick.wrapping_add(90)
            };
            a.data[y * columns * 3..(y + 1) * columns * 3].fill(value);
            b.data[y * columns * 3..(y + 1) * columns * 3].fill(other);
        }
        (a, b)
    }

    #[test]
    fn reports_nothing_before_three_observations() {
        let mut det = FixedRegionDetector::new(100, 96);
        let (a, b) = header_pair(100, 96, 20, 10);
        det.observe(&a, &b);
        det.observe(&a, &b);
        assert!(!det.ready());
        assert_eq!(det.bands(1000), FixedBands::default());
    }

    #[test]
    fn detects_a_stable_top_band() {
        let mut det = FixedRegionDetector::new(100, 96);
        for tick in 0..4u8 {
            let (a, b) = header_pair(100, 96, 20, tick * 10);
            det.observe(&a, &b);
        }
        assert!(det.ready());
        let bands = det.bands(1000);
        assert_eq!(bands.top, 20, "expected the 20px header, got {bands:?}");
        assert_eq!(bands.bottom, 0);
    }

    #[test]
    fn a_fully_static_screen_is_a_pause_not_chrome() {
        let mut det = FixedRegionDetector::new(100, 96);
        let still = sparse(100, 96, 128);
        for _ in 0..4 {
            det.observe(&still, &still);
        }
        // Everything looks fixed; the 45% cap plus the all-true reset must
        // refuse to classify the whole viewport as chrome.
        assert_eq!(det.bands(1000), FixedBands::default());
    }

    #[test]
    fn ignores_observations_with_the_wrong_shape() {
        let mut det = FixedRegionDetector::new(100, 96);
        let small = sparse(50, 96, 5);
        det.observe(&small, &small);
        assert!(!det.ready());
    }

    #[test]
    fn sample_boundary_is_the_midpoint_between_samples() {
        // 96 samples over 1000px: sample k sits at k*999/95.
        let boundary = sample_boundary(1000, 96, 10);
        assert!((90..=115).contains(&boundary), "got {boundary}");
        assert_eq!(sample_boundary(1000, 96, 0), 0);
        assert_eq!(sample_boundary(1000, 96, 96), 1000);
    }

    #[test]
    fn edge_mask_rejects_bands_thinner_than_the_minimum() {
        let mut values = vec![false; 100];
        values[..2].fill(true);
        assert!(edge_mask(&values, 4).iter().all(|v| !v));
    }
}
