//! Synthetic "scrollable page" generator for the stitching regression suite.
//!
//! Real screenshots are not checked into the repo (they would be user data), so
//! the acceptance cases from ARCHITECTURE.md section 4 run against a generated
//! page that has the properties the matcher actually depends on:
//!
//!   * every row is distinguishable from its neighbours, so a wrong offset is
//!     visibly wrong rather than accidentally cheap,
//!   * horizontal edges at the 96 sampled columns, since `compute_cols` only
//!     sees those,
//!   * text-like runs instead of noise, because noise makes *any* offset score
//!     badly and would hide real regressions.

use vellum_core::image::Rgb8;

pub const PAGE_W: usize = 320;

/// Deterministic 32-bit mixer. Keeps the fixture reproducible across machines
/// (no `rand`, no float rounding differences).
fn mix(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^ (x >> 16)
}

/// A tall page of text-like lines. Line pitch is 12px with 5px tall glyphs, so
/// a 20px scroll never lands on an identical row pattern.
///
/// Every row must be *individually* identifiable, because `compute_cols`
/// reduces a row to three numbers and the matcher compares nothing else. An
/// earlier version of this fixture left the 7 inter-line gap rows as flat
/// background; those rows then had byte-identical signatures across the whole
/// page, dozens of offsets tied at ~0 diff, and any small perturbation (a
/// blinking tile) flipped the winner to a wrong-but-equally-cheap offset. The
/// Python reference suite avoids this by giving each row its own value, so the
/// gap rows here carry a faint per-row wash plus sparse tick marks.
pub fn page(width: usize, height: usize) -> Rgb8 {
    let mut img = Rgb8::new(width, height);
    for row in 0..height {
        let line = row / 12;
        let within = row % 12;
        // Per-row wash in 238..=252: invisible to a human, but it makes the
        // row signature of every single row distinct.
        let noise = mix(row as u32 ^ 0x9e37_79b9);
        let bg = (238 + (noise % 15)) as u8;
        let px = img.row_mut(row);
        for value in px.iter_mut() {
            *value = bg;
        }
        // Sparse per-row ticks so gap rows also have edge energy that shifts
        // with the content instead of being uniform everywhere.
        let tick_phase = (noise >> 8) as usize % 17;
        let tick_shade = (150 + (noise >> 16) % 60) as u8;
        let mut col = 4 + tick_phase;
        while col < width {
            let base = col * 3;
            px[base] = tick_shade;
            px[base + 1] = tick_shade;
            px[base + 2] = tick_shade.saturating_add(10);
            col += 19;
        }
        if within >= 5 {
            continue; // inter-line gap
        }

        // Lay out 4..7 dark runs per line, widths 6..20, with gaps.
        let mut state = mix((line as u32).wrapping_mul(2_654_435_761).wrapping_add(17));
        let runs = 4 + (state % 4) as usize;
        let mut x = 8 + (mix(state) % 12) as usize;
        for run in 0..runs {
            state = mix(state.wrapping_add(run as u32));
            let w = 6 + (state % 15) as usize;
            if x + w >= width.saturating_sub(6) {
                break;
            }
            // Vary the grey per run and per row so rows inside one line differ.
            let shade = (30 + (mix(state ^ within as u32) % 90)) as u8;
            for col in x..x + w {
                let base = col * 3;
                px[base] = shade;
                px[base + 1] = shade;
                px[base + 2] = shade.saturating_add(12);
            }
            x += w + 5 + (state >> 8) as usize % 9;
        }
    }
    img
}

/// Viewport onto `src` starting at row `top`. Panics if the window escapes the
/// page, which would mean the test itself is wrong.
pub fn viewport(src: &Rgb8, top: usize, height: usize) -> Rgb8 {
    assert!(
        top + height <= src.height,
        "viewport {top}+{height} exceeds page height {}",
        src.height
    );
    src.rows_slice(top, top + height)
}

/// Overwrite rows `[from, to)` with static chrome (a fixed header/footer band).
pub fn paint_band(frame: &mut Rgb8, from: usize, to: usize) {
    for row in from..to.min(frame.height) {
        let width = frame.width;
        let px = frame.row_mut(row);
        for col in 0..width {
            let base = col * 3;
            // A deliberately structured band: flat fills score as "unchanged"
            // everywhere and would not exercise band detection realistically.
            let on = (col / 7 + row / 3) % 3 == 0;
            let (r, g, b) = if on { (60, 70, 90) } else { (210, 214, 222) };
            px[base] = r;
            px[base + 1] = g;
            px[base + 2] = b;
        }
    }
}

/// Overwrite columns `[from, to)` with a static vertical sidebar.
pub fn paint_sidebar(frame: &mut Rgb8, from: usize, to: usize) {
    let width = frame.width;
    for row in 0..frame.height {
        let px = frame.row_mut(row);
        for col in from..to.min(width) {
            let base = col * 3;
            let on = (row / 9 + col / 5) % 4 == 0;
            let (r, g, b) = if on { (40, 90, 60) } else { (225, 232, 226) };
            px[base] = r;
            px[base + 1] = g;
            px[base + 2] = b;
        }
    }
}

/// A small animating rectangle (video tile, spinner, caret) whose content is
/// unrelated to the scroll. Exercises the trimmed-score robust path.
///
/// Deliberately *structured*, not noise. Per-pixel noise was tried first and is
/// wrong as a fixture: its edge energy dominates `compute_cols`' third channel,
/// so every candidate offset scores badly, the matcher confidently locks onto
/// junk offsets, and the case stops testing the robust path at all. (The Python
/// reference behaves identically on a noise tile, so this was a fixture defect,
/// not a port defect.) A flat tile with one moving bar changes every frame while
/// keeping the surrounding text the strongest signal in the row signature.
pub fn paint_animation(frame: &mut Rgb8, rect: (usize, usize, usize, usize), tick: u32) {
    let (x0, y0, w, h) = rect;
    let y_end = (y0 + h).min(frame.height);
    let x_end = (x0 + w).min(frame.width);
    // Bar sweeps down the tile, one tile-row per tick.
    let bar_h = (h / 6).max(3);
    let bar_top = y0 + (tick as usize * 5) % h.max(1);
    for row in y0..y_end {
        let px = frame.row_mut(row);
        let in_bar = row >= bar_top && row < bar_top + bar_h;
        let (r, g, b) = if in_bar {
            (70u8, 120u8, 200u8)
        } else {
            (150u8, 150u8, 158u8)
        };
        for col in x0..x_end {
            let base = col * 3;
            px[base] = r;
            px[base + 1] = g;
            px[base + 2] = b;
        }
    }
}

/// Overwrite rows `[from, to)` with a flat fill, emulating a dialog or overlay
/// that suddenly covers most of the viewport. Unlike [`paint_animation`] this is
/// meant to exceed what the trimmed robust score can absorb, so the frame is
/// rejected outright.
pub fn paint_occlusion(frame: &mut Rgb8, from: usize, to: usize, value: u8) {
    for row in from..to.min(frame.height) {
        for byte in frame.row_mut(row) {
            *byte = value;
        }
    }
}

/// Blend the frame towards a solid tint, emulating a translucent terminal over
/// a wallpaper: contrast drops, so matching has less signal to work with.
pub fn apply_translucency(frame: &mut Rgb8, alpha: f32, tint: [u8; 3]) {
    for value in 0..frame.data.len() {
        let channel = tint[value % 3] as f32;
        let src = frame.data[value] as f32;
        frame.data[value] = (src * alpha + channel * (1.0 - alpha)).round() as u8;
    }
}

/// Mean absolute per-channel difference between two equally sized images.
pub fn mean_abs_diff(a: &Rgb8, b: &Rgb8) -> f32 {
    assert_eq!((a.width, a.height), (b.width, b.height), "size mismatch");
    if a.data.is_empty() {
        return 0.0;
    }
    let total: u64 = a
        .data
        .iter()
        .zip(&b.data)
        .map(|(x, y)| x.abs_diff(*y) as u64)
        .sum();
    total as f32 / a.data.len() as f32
}

/// Compare `out` against the page slice it should reproduce, ignoring `skip_top`
/// and `skip_bottom` rows plus `skip_cols` columns on each side.
///
/// Interior-only comparison exists because a fixed header/footer legitimately
/// occupies the outer rows of the stitched image, and a fixed sidebar is
/// extrapolated rather than reproduced from the page.
pub fn interior_diff(
    out: &Rgb8,
    expected: &Rgb8,
    skip_top: usize,
    skip_bottom: usize,
    skip_cols: (usize, usize),
) -> f32 {
    assert_eq!(out.width, expected.width, "width mismatch");
    assert_eq!(out.height, expected.height, "height mismatch");
    let (left, right) = skip_cols;
    let x_end = out.width - right;
    let mut total = 0u64;
    let mut count = 0u64;
    for row in skip_top..out.height - skip_bottom {
        let a = out.row(row);
        let b = expected.row(row);
        for col in left..x_end {
            for channel in 0..3 {
                let base = col * 3 + channel;
                total += a[base].abs_diff(b[base]) as u64;
                count += 1;
            }
        }
    }
    if count == 0 {
        return 0.0;
    }
    total as f32 / count as f32
}
