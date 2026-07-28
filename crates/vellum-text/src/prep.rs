//! Image preparation for local OCR.
//!
//! The Python original leaned on Pillow and OpenCV. Everything here is a
//! hand-rolled equivalent of the exact operations that were benchmarked for
//! character accuracy, because the two pipelines below are load-bearing and
//! swapping in "something similar" measurably degrades recognition:
//!
//! * Clean background (solid panel, terminal without transparency):
//!   autocontrast then Lanczos upscale. Accuracy ~0.85 -> ~0.97. Deliberately
//!   *no* binarisation: thresholding damages clean anti-aliased small text.
//! * Busy background (wallpaper showing through a translucent terminal, where
//!   plain OCR collapses from ~0.88 to ~0.10): estimate the background with a
//!   morphological close, divide it out, upscale, then Otsu. ~0.22 -> ~0.88.
//!   Thresholding *without* the division is worse than doing nothing (~0.02),
//!   so the divide step is not optional.
//!
//! Which pipeline runs is decided by `busyness`, not by a user setting: a solid
//! panel measures near 0 and glass-over-wallpaper measures 6 or more, so the
//! threshold sits in the empty gap between the two distributions.

use image::imageops::FilterType;
use image::{GrayImage, ImageBuffer};

use vellum_core::Rgb8;

/// Below this mean luminance the image is treated as a dark theme and inverted.
/// tesseract is trained on dark-on-light text.
const DARK_THEME_LUM: f64 = 112.0;

/// Structuring element size used to estimate the background gradient.
const BACKGROUND_KERNEL: usize = 15;

/// Structuring element size used to measure how busy the background is.
const BUSYNESS_KERNEL: usize = 25;

/// Standard deviation of the closed image above which the busy pipeline runs.
const BUSY_THRESHOLD: f64 = 3.0;

/// Single channel image. Kept separate from `Rgb8` so the morphology below can
/// work on one byte per pixel.
#[derive(Clone)]
pub struct Gray {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl Gray {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            data: vec![0; width * height],
        }
    }

    fn row(&self, y: usize) -> &[u8] {
        &self.data[y * self.width..(y + 1) * self.width]
    }

    fn row_mut(&mut self, y: usize) -> &mut [u8] {
        let width = self.width;
        &mut self.data[y * width..(y + 1) * width]
    }

    fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Rec. 601 luma, matching Pillow's `convert("L")`.
pub fn to_gray(image: &Rgb8) -> Gray {
    let mut out = Gray::new(image.width, image.height);
    for (dst, src) in out.data.iter_mut().zip(image.data.chunks_exact(3)) {
        let value =
            299 * u32::from(src[0]) + 587 * u32::from(src[1]) + 114 * u32::from(src[2]) + 500;
        *dst = (value / 1000) as u8;
    }
    out
}

fn to_rgb(gray: &Gray) -> Rgb8 {
    let mut data = Vec::with_capacity(gray.data.len() * 3);
    for &value in &gray.data {
        data.extend_from_slice(&[value, value, value]);
    }
    Rgb8::from_raw(gray.width, gray.height, data)
}

fn histogram(gray: &Gray) -> [u32; 256] {
    let mut counts = [0u32; 256];
    for &value in &gray.data {
        counts[usize::from(value)] += 1;
    }
    counts
}

/// Histogram-weighted mean, the same quantity Pillow's `ImageStat` reports.
pub fn mean_luminance(gray: &Gray) -> f64 {
    if gray.data.is_empty() {
        return 0.0;
    }
    let counts = histogram(gray);
    let total: f64 = counts
        .iter()
        .enumerate()
        .map(|(value, &count)| value as f64 * f64::from(count))
        .sum();
    total / gray.data.len() as f64
}

pub fn invert(gray: &mut Gray) {
    for value in &mut gray.data {
        *value = 255 - *value;
    }
}

/// Pillow's `ImageOps.autocontrast` with a percentage cutoff on both ends.
///
/// The cutoff matters: a single stray white pixel (a cursor, a window border)
/// would otherwise define the white point and flatten the actual text contrast.
pub fn autocontrast(gray: &mut Gray, cutoff_percent: u32) {
    if gray.data.is_empty() {
        return;
    }
    let mut counts = histogram(gray);
    let total: u32 = counts.iter().sum();
    let cut = cutoff_percent * total / 100;

    let mut remaining = cut;
    for count in counts.iter_mut() {
        if *count > remaining {
            *count -= remaining;
            break;
        }
        remaining -= *count;
        *count = 0;
    }
    let mut remaining = cut;
    for count in counts.iter_mut().rev() {
        if *count > remaining {
            *count -= remaining;
            break;
        }
        remaining -= *count;
        *count = 0;
    }

    let lo = counts.iter().position(|&count| count > 0);
    let hi = counts.iter().rposition(|&count| count > 0);
    let (Some(lo), Some(hi)) = (lo, hi) else {
        return;
    };
    if hi <= lo {
        return;
    }

    let scale = 255.0 / (hi - lo) as f64;
    let mut lut = [0u8; 256];
    for (value, slot) in lut.iter_mut().enumerate() {
        let mapped = (value as f64 - lo as f64) * scale;
        *slot = mapped.round().clamp(0.0, 255.0) as u8;
    }
    for value in &mut gray.data {
        *value = lut[usize::from(*value)];
    }
}

/// Half-widths of an elliptical structuring element, indexed by row.
///
/// Reproduces OpenCV's `getStructuringElement(MORPH_ELLIPSE, (k, k))`: each row
/// keeps `round(c * sqrt(r^2 - dy^2) / r)` columns either side of the centre.
fn ellipse_offsets(size: usize) -> Vec<(isize, usize)> {
    let r = (size / 2) as isize;
    let c = (size / 2) as f64;
    (0..size as isize)
        .map(|i| {
            let dy = i - r;
            let dx = if r == 0 {
                0
            } else {
                let radius = ((r * r - dy * dy) as f64).max(0.0).sqrt();
                (c * radius / r as f64).round() as usize
            };
            (dy, dx)
        })
        .collect()
}

/// Sliding-window extremum over each row, in linear time per row.
///
/// A naive 25x25 ellipse costs ~500 comparisons per pixel; separating it into
/// per-row windows plus a vertical pass brings that down to a handful, which is
/// what makes the background estimate affordable on a full-screen grab.
fn row_extremes(gray: &Gray, half: usize, maximum: bool) -> Gray {
    let mut out = Gray::new(gray.width, gray.height);
    if gray.is_empty() {
        return out;
    }
    let window = 2 * half + 1;
    let mut deque: std::collections::VecDeque<usize> = std::collections::VecDeque::new();

    for y in 0..gray.height {
        let src = gray.row(y);
        deque.clear();
        // Seed the window with the first `half` columns, then slide it.
        for x in 0..gray.width {
            let entering = x;
            while let Some(&back) = deque.back() {
                let keep = if maximum {
                    src[back] >= src[entering]
                } else {
                    src[back] <= src[entering]
                };
                if keep {
                    break;
                }
                deque.pop_back();
            }
            deque.push_back(entering);

            if x >= half {
                let centre = x - half;
                let lower = centre.saturating_sub(half);
                while let Some(&front) = deque.front() {
                    if front >= lower {
                        break;
                    }
                    deque.pop_front();
                }
                out.row_mut(y)[centre] = src[*deque.front().expect("window is never empty")];
            }
        }
        // Flush the trailing columns whose window is truncated by the edge.
        for centre in gray.width.saturating_sub(half)..gray.width {
            let lower = centre.saturating_sub(half);
            while let Some(&front) = deque.front() {
                if front >= lower {
                    break;
                }
                deque.pop_front();
            }
            match deque.front() {
                Some(&front) => out.row_mut(y)[centre] = src[front],
                None => out.row_mut(y)[centre] = src[centre],
            }
        }
        let _ = window;
    }
    out
}

/// Vertical pass of the separated morphology.
///
/// Out-of-frame rows use the neutral value for the operation (0 for dilate,
/// 255 for erode), which is what OpenCV's default border does.
fn vertical_extremes(rows: &[Gray], offsets: &[(isize, usize)], maximum: bool) -> Gray {
    let template = &rows[0];
    let mut out = Gray::new(template.width, template.height);
    if template.is_empty() {
        return out;
    }
    for y in 0..template.height {
        for x in 0..template.width {
            let mut best = if maximum { 0u8 } else { 255u8 };
            for (index, &(dy, _)) in offsets.iter().enumerate() {
                let sy = y as isize + dy;
                if sy < 0 || sy >= template.height as isize {
                    continue;
                }
                let value = rows[index].row(sy as usize)[x];
                best = if maximum {
                    best.max(value)
                } else {
                    best.min(value)
                };
            }
            out.row_mut(y)[x] = best;
        }
    }
    out
}

fn morph(gray: &Gray, size: usize, maximum: bool) -> Gray {
    let offsets = ellipse_offsets(size);
    let mut cache: std::collections::HashMap<usize, Gray> = std::collections::HashMap::new();
    let rows: Vec<Gray> = offsets
        .iter()
        .map(|&(_, dx)| {
            cache
                .entry(dx)
                .or_insert_with(|| row_extremes(gray, dx, maximum))
                .clone()
        })
        .collect();
    vertical_extremes(&rows, &offsets, maximum)
}

/// Morphological closing: dilate then erode.
pub fn close(gray: &Gray, size: usize) -> Gray {
    if gray.is_empty() {
        return gray.clone();
    }
    let dilated = morph(gray, size, true);
    morph(&dilated, size, false)
}

/// Spread of the closed image. Flat panels sit near zero; a wallpaper seen
/// through a translucent window pushes this well past the busy threshold.
pub fn busyness(gray: &Gray) -> f64 {
    if gray.is_empty() {
        return 0.0;
    }
    let closed = close(gray, BUSYNESS_KERNEL);
    let count = closed.data.len() as f64;
    let mean = closed.data.iter().map(|&v| f64::from(v)).sum::<f64>() / count;
    let variance = closed
        .data
        .iter()
        .map(|&v| {
            let delta = f64::from(v) - mean;
            delta * delta
        })
        .sum::<f64>()
        / count;
    variance.sqrt()
}

/// `cv2.divide(a, b, scale=255)`: flattens out an uneven background.
fn divide(a: &Gray, b: &Gray) -> Gray {
    let mut out = Gray::new(a.width, a.height);
    for (index, slot) in out.data.iter_mut().enumerate() {
        let divisor = u32::from(b.data[index]);
        // A zero background pixel yields zero, matching OpenCV rather than
        // saturating to white, which would punch holes into the glyphs.
        let numerator = u32::from(a.data[index]) * 255 + divisor / 2;
        *slot = numerator.checked_div(divisor).unwrap_or(0).min(255) as u8;
    }
    out
}

/// Otsu's threshold: the value that maximises between-class variance.
pub fn otsu(gray: &Gray) -> u8 {
    let counts = histogram(gray);
    let total: f64 = gray.data.len() as f64;
    if total == 0.0 {
        return 127;
    }
    let sum: f64 = counts
        .iter()
        .enumerate()
        .map(|(value, &count)| value as f64 * f64::from(count))
        .sum();

    let mut weight_below = 0.0;
    let mut sum_below = 0.0;
    let mut best_value = 0u8;
    let mut best_variance = -1.0;
    for (value, &count) in counts.iter().enumerate() {
        weight_below += f64::from(count);
        if weight_below == 0.0 {
            continue;
        }
        let weight_above = total - weight_below;
        if weight_above == 0.0 {
            break;
        }
        sum_below += value as f64 * f64::from(count);
        let mean_below = sum_below / weight_below;
        let mean_above = (sum - sum_below) / weight_above;
        let delta = mean_below - mean_above;
        let variance = weight_below * weight_above * delta * delta;
        if variance > best_variance {
            best_variance = variance;
            best_value = value as u8;
        }
    }
    best_value
}

fn threshold(gray: &mut Gray, value: u8) {
    for pixel in &mut gray.data {
        *pixel = if *pixel > value { 255 } else { 0 };
    }
}

/// Lanczos resample. Both Pillow's `LANCZOS` and OpenCV's `INTER_LANCZOS4` map
/// onto the same filter here.
fn upscale(gray: &Gray, factor: f32) -> Gray {
    let width = ((gray.width as f32 * factor).round() as usize).max(1);
    let height = ((gray.height as f32 * factor).round() as usize).max(1);
    if gray.is_empty() || (width == gray.width && height == gray.height) {
        return gray.clone();
    }
    let Some(buffer): Option<GrayImage> =
        ImageBuffer::from_raw(gray.width as u32, gray.height as u32, gray.data.clone())
    else {
        return gray.clone();
    };
    let resized =
        image::imageops::resize(&buffer, width as u32, height as u32, FilterType::Lanczos3);
    Gray {
        width: resized.width() as usize,
        height: resized.height() as usize,
        data: resized.into_raw(),
    }
}

/// Full preprocessing pass. `upscale_factor` below 1.0 is clamped away because
/// downscaling text never helps recognition.
pub fn prepare(image: &Rgb8, upscale_factor: f32) -> Rgb8 {
    let mut gray = to_gray(image);
    if gray.is_empty() {
        return image.clone();
    }
    if mean_luminance(&gray) < DARK_THEME_LUM {
        invert(&mut gray);
    }
    let factor = upscale_factor.max(1.0);

    if busyness(&gray) > BUSY_THRESHOLD {
        let background = close(&gray, BACKGROUND_KERNEL);
        let mut flattened = divide(&gray, &background);
        if factor > 1.0 {
            flattened = upscale(&flattened, factor);
        }
        let cut = otsu(&flattened);
        threshold(&mut flattened, cut);
        to_rgb(&flattened)
    } else {
        autocontrast(&mut gray, 1);
        if factor > 1.0 {
            gray = upscale(&gray, factor);
        }
        to_rgb(&gray)
    }
}

/// Marker type so callers can log which pipeline ran without re-measuring.
pub fn is_busy_background(image: &Rgb8) -> bool {
    let gray = to_gray(image);
    busyness(&gray) > BUSY_THRESHOLD
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: usize, height: usize, value: u8) -> Rgb8 {
        Rgb8::from_raw(width, height, vec![value; width * height * 3])
    }

    #[test]
    fn an_ellipse_kernel_matches_opencv_shape() {
        let offsets = ellipse_offsets(15);
        assert_eq!(offsets.len(), 15);
        // Centre row spans the full width, the poles collapse to a point.
        assert_eq!(offsets[7], (0, 7));
        assert_eq!(offsets[0].1, 0);
        assert_eq!(offsets[14].1, 0);
        // Half-widths grow monotonically toward the centre.
        for pair in offsets.windows(2).take(7) {
            assert!(pair[1].1 >= pair[0].1);
        }
    }

    #[test]
    fn a_flat_panel_is_not_busy() {
        let image = solid(64, 48, 200);
        assert!(!is_busy_background(&image));
    }

    #[test]
    fn a_textured_background_is_busy() {
        // A coarse gradient plus noise, i.e. wallpaper through glass.
        let (w, h) = (96usize, 96usize);
        let mut data = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                let base = ((x * 5 + y * 3) % 200) as u8;
                data.extend_from_slice(&[base, base, base]);
            }
        }
        let image = Rgb8::from_raw(w, h, data);
        assert!(is_busy_background(&image));
    }

    #[test]
    fn closing_a_flat_image_changes_nothing() {
        let gray = to_gray(&solid(40, 30, 128));
        let closed = close(&gray, 15);
        assert!(closed.data.iter().all(|&value| value == 128));
    }

    #[test]
    fn closing_removes_thin_dark_strokes() {
        // Closing is a dilate-then-erode of the *bright* image, so thin dark
        // text on a light background is what gets filled in. That is exactly
        // why it can stand in for the background.
        let (w, h) = (40usize, 20usize);
        let mut gray = Gray::new(w, h);
        gray.data.fill(220);
        for y in 4..16 {
            gray.row_mut(y)[20] = 10;
        }
        let closed = close(&gray, 15);
        assert!(closed.row(10)[20] > 200);
    }

    #[test]
    fn autocontrast_stretches_a_narrow_band() {
        let mut gray = Gray::new(10, 10);
        for (index, value) in gray.data.iter_mut().enumerate() {
            *value = if index % 2 == 0 { 100 } else { 140 };
        }
        autocontrast(&mut gray, 1);
        assert_eq!(gray.data.iter().copied().min(), Some(0));
        assert_eq!(gray.data.iter().copied().max(), Some(255));
    }

    #[test]
    fn otsu_splits_a_two_peak_histogram() {
        let mut gray = Gray::new(20, 20);
        for (index, value) in gray.data.iter_mut().enumerate() {
            *value = if index % 2 == 0 { 30 } else { 220 };
        }
        let cut = otsu(&gray);
        assert!((30..220).contains(&cut), "threshold landed at {cut}");
    }

    #[test]
    fn a_dark_theme_is_inverted_before_recognition() {
        let mut dark = solid(32, 32, 20);
        // A few bright pixels so autocontrast has a range to work with.
        for pixel in dark.data.chunks_exact_mut(3).take(16) {
            pixel.copy_from_slice(&[200, 200, 200]);
        }
        let prepared = prepare(&dark, 1.0);
        let mean = mean_luminance(&to_gray(&prepared));
        assert!(mean > 128.0, "expected a light background, got {mean}");
    }

    #[test]
    fn upscaling_keeps_the_aspect_ratio() {
        let gray = to_gray(&solid(20, 10, 90));
        let bigger = upscale(&gray, 3.0);
        assert_eq!((bigger.width, bigger.height), (60, 30));
    }

    #[test]
    fn preparing_an_empty_image_is_a_no_op() {
        let empty = Rgb8::new(0, 0);
        let prepared = prepare(&empty, 3.0);
        assert!(prepared.is_empty());
    }
}
