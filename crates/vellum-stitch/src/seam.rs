//! Pick one horizontal ownership boundary, never blend two states of a glyph.
//! Work is bounded by a 256-row strip and 192 sampled columns.
use vellum_core::Rgb8;

const SEARCH_ROWS: usize = 256;
const SAMPLE_COLUMNS: usize = 192;
const GUARD: usize = 3;

/// `a_start` and `b_start` describe the same aligned overlap in both images.
/// Return rows to keep from the upper image; whole rows below the cut belong
/// to the lower image. Callers may reverse which image owns each side.
pub(crate) fn choose(
    a: &Rgb8,
    a_start: usize,
    b: &Rgb8,
    b_start: usize,
    overlap: usize,
    columns: std::ops::Range<usize>,
) -> usize {
    if overlap < 2 * GUARD + 2 || columns.is_empty() {
        return overlap / 2;
    }
    debug_assert!(a_start + overlap <= a.height && b_start + overlap <= b.height);
    debug_assert!(columns.end <= a.width && columns.end <= b.width);
    let rows = overlap.min(SEARCH_ROWS);
    let first = (overlap - rows) / 2;
    let count = columns.len().min(SAMPLE_COLUMNS);
    let mut energy = Vec::with_capacity(rows);
    for y in first..first + rows {
        let ar = a.row(a_start + y);
        let br = b.row(b_start + y);
        let ap = a.row(a_start + y.saturating_sub(1));
        let bp = b.row(b_start + y.saturating_sub(1));
        let mut total = 0u64;
        for i in 0..count {
            let x = columns.start + i * (columns.len() - 1) / count.saturating_sub(1).max(1);
            let p = x * 3;
            let left = x.saturating_sub(1).max(columns.start) * 3;
            for c in 0..3 {
                // Blank agreement is cheap. Text edges, mismatched states and
                // rows just above/below a glyph are expensive to cut through.
                total += 2 * u64::from(ar[p + c].abs_diff(br[p + c]));
                total += u64::from(ar[p + c].abs_diff(ar[left + c]));
                total += u64::from(br[p + c].abs_diff(br[left + c]));
                total += u64::from(ar[p + c].abs_diff(ap[p + c]));
                total += u64::from(br[p + c].abs_diff(bp[p + c]));
            }
        }
        energy.push(total as f64 / (count * 3) as f64);
    }
    let centre = rows / 2;
    let mut best = centre;
    let mut best_cost = f64::INFINITY;
    for cut in GUARD..rows - GUARD {
        let cost = energy[cut - GUARD..=cut + GUARD].iter().sum::<f64>()
            + cut.abs_diff(centre) as f64 * 0.01;
        if cost < best_cost {
            best = cut;
            best_cost = cost;
        }
    }
    first + best
}

/// Feather only a proven smooth background on the incoming side of a cut.
/// A single real text edge vetoes the entire band, including low-contrast text;
/// per-pixel delta alone cannot distinguish antialiasing from wallpaper.
pub(crate) fn background_patch(
    previous: (&Rgb8, usize),
    incoming: (&Rgb8, usize),
    overlap: usize,
    cut: usize,
    new_below: bool,
    columns: std::ops::Range<usize>,
) -> Option<Rgb8> {
    let (a, a_start) = previous;
    let (b, b_start) = incoming;
    let (start, end) = if new_below {
        (cut, (cut + 8).min(overlap))
    } else {
        (cut.saturating_sub(8), cut)
    };
    if end - start < 2 {
        return None;
    }
    // Inspect every pixel, not sparse columns: a thin glyph must not be missed.
    for y in start.saturating_sub(2)..(end + 2).min(overlap) {
        let ar = a.row(a_start + y);
        let br = b.row(b_start + y);
        let ap = a.row(a_start + y.saturating_sub(1));
        let bp = b.row(b_start + y.saturating_sub(1));
        for x in columns.clone() {
            let p = x * 3;
            let left = x.saturating_sub(1).max(columns.start) * 3;
            for c in 0..3 {
                if ar[p + c].abs_diff(br[p + c]) > 48
                    || ar[p + c].abs_diff(ar[left + c]) > 6
                    || br[p + c].abs_diff(br[left + c]) > 6
                    || ar[p + c].abs_diff(ap[p + c]) > 6
                    || br[p + c].abs_diff(bp[p + c]) > 6
                {
                    return None;
                }
            }
        }
    }
    let mut patch = a.rows_slice(a_start + start, a_start + end);
    let count = end - start;
    let total = count as u32 + 1;
    for y in 0..count {
        let weight = if new_below { y + 1 } else { count - y } as u32;
        let source = b.row(b_start + start + y);
        let target = patch.row_mut(y);
        for c in columns.start * 3..columns.end * 3 {
            target[c] = ((u32::from(target[c]) * (total - weight)
                + u32::from(source[c]) * weight
                + total / 2)
                / total) as u8;
        }
    }
    Some(patch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seam_avoids_text_that_crosses_the_geometric_midpoint() {
        let mut a = Rgb8::from_raw(96, 80, vec![248; 96 * 80 * 3]);
        let mut b = a.clone();
        for y in 29..53 {
            for x in 12..80 {
                a.row_mut(y)[x * 3..x * 3 + 3].fill(40);
                b.row_mut(y + 2)[x * 3..x * 3 + 3].fill(80);
            }
        }
        let cut = choose(&a, 0, &b, 0, 80, 0..96);
        assert!(!(26..58).contains(&cut), "cut went through a glyph: {cut}");
    }

    #[test]
    fn feather_is_background_only_even_for_low_contrast_thin_text() {
        let a = Rgb8::from_raw(32, 32, vec![100; 32 * 32 * 3]);
        let mut b = Rgb8::from_raw(32, 32, vec![130; 32 * 32 * 3]);
        let patch = background_patch((&a, 0), (&b, 0), 32, 16, true, 0..32).unwrap();
        assert!(patch.pixel(0, 0)[0] < 110 && patch.pixel(0, 7)[0] > 120);
        b.row_mut(19)[15 * 3..16 * 3].fill(110);
        assert!(background_patch((&a, 0), (&b, 0), 32, 16, true, 0..32).is_none());
    }

    #[test]
    fn empty_and_small_overlaps_have_a_bounded_cut() {
        let image = Rgb8::new(8, 8);
        for rows in 0..=8 {
            assert!(choose(&image, 0, &image, 0, rows, 0..8) <= rows);
        }
    }
}
