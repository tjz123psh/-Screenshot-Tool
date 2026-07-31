//! Image preparation for local OCR.
//!
//! The proven clean/busy pipelines remain the baseline: clean panels use
//! autocontrast plus Lanczos, while translucent or textured backgrounds use a
//! morphological background estimate, division and Otsu. Hard-thresholding is
//! deliberately kept away from clean anti-aliased text.
//!
//! Real desktop text is not always separable by luminance, though. Faded glyphs
//! can occupy less than one percent of a crop, light text can sit on a bright
//! gradient, and two vivid colors may have exactly the same grayscale value.
//! A preparation plan therefore adds specialist candidates only when scene
//! statistics justify them:
//!
//! * CLAHE for locally dim/low-contrast strokes;
//! * the opposite busy-background polarity for pale text on uneven surfaces;
//! * max-channel or principal-color projection for hue-only contrast.
//!
//! Candidates are rendered lazily. Tesseract confidence normally accepts the
//! original baseline, so clean screenshots retain the old cost and appearance.

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

/// Robust range below which global contrast alone tends to lose faded glyphs.
const LOW_CONTRAST_RANGE: u8 = 96;

/// Average per-pixel RGB spread that makes a color-preserving projection worth
/// trying. Neutral screenshots stay on the cheaper luminance path.
const COLORFUL_CHROMA: f64 = 12.0;

/// CLAHE tiles per axis. Eight matches the OpenCV reference pipeline while the
/// implementation automatically shrinks the grid for tiny crops.
const CLAHE_GRID: usize = 8;
const CLAHE_CLIP_FACTOR: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateKind {
    Baseline,
    LocalContrast,
    OppositePolarity,
    ColorContrast,
}

pub(crate) struct Preparation<'a> {
    source: &'a Rgb8,
    original: Gray,
    oriented: Gray,
    factor: f32,
    busy: bool,
    luma_range: u8,
    chroma: f64,
    channel_range: u8,
}

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

/// Histogram stretch with a sub-percent cutoff. Small, faint text can occupy
/// less than one percent of a large screenshot; the legacy one-percent cutoff
/// may therefore classify the glyphs themselves as outliers. One per mille
/// still ignores a stray cursor pixel without erasing a whole text line.
fn quantile_from_histogram(counts: &[u32; 256], cut: usize, from_high: bool) -> Option<usize> {
    let mut remaining = cut as u64;
    let mut visit = |index: usize| {
        let count = u64::from(counts[index]);
        if count > remaining {
            Some(index)
        } else {
            remaining = remaining.saturating_sub(count);
            None
        }
    };
    if from_high {
        (0..256).rev().find_map(&mut visit)
    } else {
        (0..256).find_map(visit)
    }
}

fn robust_range(gray: &Gray) -> u8 {
    if gray.data.is_empty() {
        return 0;
    }
    let counts = histogram(gray);
    let cut = gray.data.len() / 1000;
    match (
        quantile_from_histogram(&counts, cut, false),
        quantile_from_histogram(&counts, cut, true),
    ) {
        (Some(lo), Some(hi)) => hi.saturating_sub(lo) as u8,
        _ => 0,
    }
}

/// Make the dominant border color white. Unlike a whole-image mean this still
/// handles a pale floating panel on a dark desktop and light text over a bright
/// gradient: the crop border is a better estimate of its background polarity.
fn orient_background_light(gray: &mut Gray) {
    if gray.is_empty() {
        return;
    }
    let band = (gray.width.min(gray.height) / 32).clamp(1, 5);
    let mut counts = [0u32; 256];
    let mut total = 0u32;
    for y in 0..gray.height {
        for x in 0..gray.width {
            if x < band || x + band >= gray.width || y < band || y + band >= gray.height {
                counts[usize::from(gray.row(y)[x])] += 1;
                total += 1;
            }
        }
    }
    let mut seen = 0u32;
    let median = counts
        .iter()
        .enumerate()
        .find_map(|(value, count)| {
            seen += *count;
            (seen * 2 >= total).then_some(value as u8)
        })
        .unwrap_or(255);
    if median < 128 {
        invert(gray);
    }
}

fn interpolation_axis(pixel: usize, length: usize, tiles: usize) -> (usize, usize, f64) {
    debug_assert!(length > 0 && tiles > 0);
    let position =
        ((pixel as f64 + 0.5) * tiles as f64 / length as f64 - 0.5).clamp(0.0, (tiles - 1) as f64);
    let lower = position.floor() as usize;
    let upper = (lower + 1).min(tiles - 1);
    let weight = if lower == upper {
        0.0
    } else {
        position - lower as f64
    };
    (lower, upper, weight)
}

/// Contrast-limited adaptive histogram equalisation (CLAHE), implemented on a
/// small tile grid and bilinearly interpolated to avoid seams. It expands dim
/// anti-aliased strokes without forcing the hard binary edge that damages clean
/// text, while clipping each histogram stops colored noise from taking over a
/// whole tile.
fn clahe(gray: &Gray) -> Gray {
    if gray.is_empty() {
        return gray.clone();
    }
    let tiles_x = CLAHE_GRID.min(gray.width).max(1);
    let tiles_y = CLAHE_GRID.min(gray.height).max(1);
    let mut luts = vec![[0u8; 256]; tiles_x * tiles_y];

    for ty in 0..tiles_y {
        let y0 = ty * gray.height / tiles_y;
        let y1 = (ty + 1) * gray.height / tiles_y;
        for tx in 0..tiles_x {
            let x0 = tx * gray.width / tiles_x;
            let x1 = (tx + 1) * gray.width / tiles_x;
            let area = ((x1 - x0) * (y1 - y0)).max(1) as u32;
            let mut counts = [0u32; 256];
            for y in y0..y1 {
                for &value in &gray.row(y)[x0..x1] {
                    counts[usize::from(value)] += 1;
                }
            }

            let limit = (CLAHE_CLIP_FACTOR * area / 256).max(1);
            let mut excess = 0u32;
            for count in &mut counts {
                if *count > limit {
                    excess += *count - limit;
                    *count = limit;
                }
            }
            let share = excess / 256;
            let remainder = excess % 256;
            for count in &mut counts {
                *count += share;
            }
            if remainder > 0 {
                for index in 0..remainder as usize {
                    // Spread the remainder rather than biasing the darkest bins.
                    counts[index * 256 / remainder as usize] += 1;
                }
            }

            let lut = &mut luts[ty * tiles_x + tx];
            let mut cumulative = 0u32;
            for (value, slot) in lut.iter_mut().enumerate() {
                cumulative += counts[value];
                *slot = ((u64::from(cumulative) * 255 + u64::from(area) / 2) / u64::from(area))
                    .min(255) as u8;
            }
        }
    }

    let mut out = Gray::new(gray.width, gray.height);
    for y in 0..gray.height {
        let (y0, y1, wy) = interpolation_axis(y, gray.height, tiles_y);
        for x in 0..gray.width {
            let (x0, x1, wx) = interpolation_axis(x, gray.width, tiles_x);
            let value = usize::from(gray.row(y)[x]);
            let top = f64::from(luts[y0 * tiles_x + x0][value]) * (1.0 - wx)
                + f64::from(luts[y0 * tiles_x + x1][value]) * wx;
            let bottom = f64::from(luts[y1 * tiles_x + x0][value]) * (1.0 - wx)
                + f64::from(luts[y1 * tiles_x + x1][value]) * wx;
            out.row_mut(y)[x] = (top * (1.0 - wy) + bottom * wy).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

fn max_channel(image: &Rgb8) -> Gray {
    let mut out = Gray::new(image.width, image.height);
    for (dst, src) in out.data.iter_mut().zip(image.data.chunks_exact(3)) {
        *dst = src[0].max(src[1]).max(src[2]);
    }
    out
}

fn mean_chroma(image: &Rgb8) -> f64 {
    if image.data.is_empty() {
        return 0.0;
    }
    let sum: u64 = image
        .data
        .chunks_exact(3)
        .map(|pixel| u64::from(pixel.iter().max().unwrap() - pixel.iter().min().unwrap()))
        .sum();
    sum as f64 / (image.data.len() / 3) as f64
}

fn strongest_channel_range(image: &Rgb8) -> u8 {
    (0..3)
        .map(|channel| {
            let mut gray = Gray::new(image.width, image.height);
            for (dst, pixel) in gray.data.iter_mut().zip(image.data.chunks_exact(3)) {
                *dst = pixel[channel];
            }
            robust_range(&gray)
        })
        .max()
        .unwrap_or(0)
}

/// Principal-color projection. Luminance deliberately gives equal weight to
/// colors with equal brightness, which can erase red-on-blue or green-on-red
/// text completely. PCA finds the RGB direction with the most separation and
/// maps it back to one channel; neutral images naturally collapse to luma-like
/// weights.
fn principal_color_gray(image: &Rgb8) -> Gray {
    if image.is_empty() {
        return to_gray(image);
    }
    let pixels = image.data.len() / 3;
    let stride = (pixels / 200_000).max(1);
    let mut mean = [0.0f64; 3];
    let mut count = 0.0f64;
    for pixel in image.data.chunks_exact(3).step_by(stride) {
        for channel in 0..3 {
            mean[channel] += f64::from(pixel[channel]);
        }
        count += 1.0;
    }
    for value in &mut mean {
        *value /= count.max(1.0);
    }

    let mut covariance = [[0.0f64; 3]; 3];
    for pixel in image.data.chunks_exact(3).step_by(stride) {
        let centered = [
            f64::from(pixel[0]) - mean[0],
            f64::from(pixel[1]) - mean[1],
            f64::from(pixel[2]) - mean[2],
        ];
        for row in 0..3 {
            for col in 0..3 {
                covariance[row][col] += centered[row] * centered[col];
            }
        }
    }

    let seed = (0..3)
        .max_by(|&a, &b| {
            covariance[a]
                .iter()
                .map(|value| value * value)
                .sum::<f64>()
                .total_cmp(&covariance[b].iter().map(|value| value * value).sum::<f64>())
        })
        .unwrap_or(0);
    let seed_norm = covariance[seed]
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();
    if seed_norm <= f64::EPSILON {
        return to_gray(image);
    }
    let mut vector = [
        covariance[seed][0] / seed_norm,
        covariance[seed][1] / seed_norm,
        covariance[seed][2] / seed_norm,
    ];
    for _ in 0..16 {
        let next = [
            covariance[0][0] * vector[0]
                + covariance[0][1] * vector[1]
                + covariance[0][2] * vector[2],
            covariance[1][0] * vector[0]
                + covariance[1][1] * vector[1]
                + covariance[1][2] * vector[2],
            covariance[2][0] * vector[0]
                + covariance[2][1] * vector[1]
                + covariance[2][2] * vector[2],
        ];
        let norm = (next[0] * next[0] + next[1] * next[1] + next[2] * next[2]).sqrt();
        if norm <= f64::EPSILON {
            return to_gray(image);
        }
        vector = [next[0] / norm, next[1] / norm, next[2] / norm];
    }

    let mut projected = Vec::with_capacity(pixels);
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for pixel in image.data.chunks_exact(3) {
        let value = (f64::from(pixel[0]) - mean[0]) * vector[0]
            + (f64::from(pixel[1]) - mean[1]) * vector[1]
            + (f64::from(pixel[2]) - mean[2]) * vector[2];
        min = min.min(value);
        max = max.max(value);
        projected.push(value);
    }
    if max <= min + f64::EPSILON {
        return to_gray(image);
    }

    // Approximate 0.1/99.9 percentiles with a compact histogram so a one-pixel
    // border does not flatten the useful color separation.
    let mut counts = [0u32; 1024];
    for &value in &projected {
        let bin = (((value - min) * 1023.0 / (max - min)).round() as usize).min(1023);
        counts[bin] += 1;
    }
    let cut = pixels / 1000;
    let mut remaining = cut as u64;
    let low_bin = (0..1024)
        .find(|&index| {
            let count = u64::from(counts[index]);
            if count > remaining {
                true
            } else {
                remaining = remaining.saturating_sub(count);
                false
            }
        })
        .unwrap_or(0);
    let mut remaining = cut as u64;
    let high_bin = (0..1024)
        .rev()
        .find(|&index| {
            let count = u64::from(counts[index]);
            if count > remaining {
                true
            } else {
                remaining = remaining.saturating_sub(count);
                false
            }
        })
        .unwrap_or(1023);
    let low = min + (max - min) * low_bin as f64 / 1023.0;
    let high = min + (max - min) * high_bin as f64 / 1023.0;
    let scale = 255.0 / (high - low).max(f64::EPSILON);
    Gray {
        width: image.width,
        height: image.height,
        data: projected
            .into_iter()
            .map(|value| ((value - low) * scale).round().clamp(0.0, 255.0) as u8)
            .collect(),
    }
}

fn local_contrast_candidate(mut gray: Gray, factor: f32) -> Rgb8 {
    // CLAHE needs the original narrow histogram to recognise that a faded
    // stroke is locally unusual. Globally stretching first can turn the few
    // glyph pixels into clipped extrema and loses the distinction again.
    orient_background_light(&mut gray);
    gray = clahe(&gray);
    if factor > 1.0 {
        gray = upscale(&gray, factor);
    }
    to_rgb(&gray)
}

fn busy_candidate(gray: &Gray, factor: f32) -> Rgb8 {
    let background = close(gray, BACKGROUND_KERNEL);
    let mut flattened = divide(gray, &background);
    if factor > 1.0 {
        flattened = upscale(&flattened, factor);
    }
    let cut = otsu(&flattened);
    threshold(&mut flattened, cut);
    to_rgb(&flattened)
}

fn baseline_candidate(mut gray: Gray, factor: f32, busy: bool) -> Rgb8 {
    if busy {
        busy_candidate(&gray, factor)
    } else {
        autocontrast(&mut gray, 1);
        if factor > 1.0 {
            gray = upscale(&gray, factor);
        }
        to_rgb(&gray)
    }
}

impl<'a> Preparation<'a> {
    pub(crate) fn new(image: &'a Rgb8, upscale_factor: f32) -> Self {
        let original = to_gray(image);
        let luma_range = robust_range(&original);
        let chroma = mean_chroma(image);
        let channel_range = strongest_channel_range(image);
        let mut oriented = original.clone();
        if mean_luminance(&oriented) < DARK_THEME_LUM {
            invert(&mut oriented);
        }
        let busy = busyness(&oriented) > BUSY_THRESHOLD;
        Self {
            source: image,
            original,
            oriented,
            factor: upscale_factor.max(1.0),
            busy,
            luma_range,
            chroma,
            channel_range,
        }
    }

    pub(crate) fn kinds(&self) -> Vec<CandidateKind> {
        let mut kinds = vec![CandidateKind::Baseline];
        if self.luma_range < LOW_CONTRAST_RANGE {
            kinds.push(CandidateKind::LocalContrast);
        }
        if self.busy {
            kinds.push(CandidateKind::OppositePolarity);
        }
        if self.chroma >= COLORFUL_CHROMA || self.channel_range > self.luma_range.saturating_add(20)
        {
            kinds.push(CandidateKind::ColorContrast);
        }
        kinds
    }

    /// Render on demand. OCR often accepts the baseline immediately, so the
    /// extra morphology/PCA work must not be paid before confidence says it is
    /// needed.
    pub(crate) fn render(&self, kind: CandidateKind) -> Rgb8 {
        match kind {
            CandidateKind::Baseline => {
                baseline_candidate(self.oriented.clone(), self.factor, self.busy)
            }
            CandidateKind::LocalContrast => {
                local_contrast_candidate(self.original.clone(), self.factor)
            }
            CandidateKind::OppositePolarity => {
                let mut opposite = self.oriented.clone();
                invert(&mut opposite);
                busy_candidate(&opposite, self.factor)
            }
            CandidateKind::ColorContrast => {
                let isoluminant =
                    self.luma_range < 40 && self.channel_range > self.luma_range.saturating_add(32);
                let color = if isoluminant {
                    principal_color_gray(self.source)
                } else {
                    max_channel(self.source)
                };
                local_contrast_candidate(color, self.factor)
            }
        }
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
    fn principal_color_keeps_isoluminant_text_visible() {
        let (width, height) = (80usize, 48usize);
        // These colors both map to Rec.601 luma 91, so the ordinary grayscale
        // candidate contains no glyph contrast at all.
        let background = [35u8, 105, 170];
        let foreground = [255u8, 25, 0];
        let mut data = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            for x in 0..width {
                let pixel = if (16..64).contains(&x) && (12..36).contains(&y) {
                    foreground
                } else {
                    background
                };
                data.extend_from_slice(&pixel);
            }
        }
        let image = Rgb8::from_raw(width, height, data);
        assert_eq!(robust_range(&to_gray(&image)), 0);
        assert!(robust_range(&principal_color_gray(&image)) > 200);

        let plan = Preparation::new(&image, 1.0);
        assert!(plan.kinds().contains(&CandidateKind::ColorContrast));
    }

    #[test]
    fn low_contrast_scenes_request_a_local_candidate() {
        let (width, height) = (96usize, 64usize);
        let mut image = solid(width, height, 220);
        for y in 18..46 {
            for x in 20..76 {
                let base = (y * width + x) * 3;
                image.data[base..base + 3].copy_from_slice(&[190, 190, 190]);
            }
        }
        let plan = Preparation::new(&image, 1.0);
        assert!(plan.kinds().contains(&CandidateKind::LocalContrast));
        let local = plan.render(CandidateKind::LocalContrast);
        assert!(robust_range(&to_gray(&local)) > robust_range(&to_gray(&image)));
    }

    #[test]
    fn busy_scenes_offer_the_other_text_polarity() {
        let (width, height) = (96usize, 72usize);
        let mut data = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            for x in 0..width {
                let value = ((x * 5 + y * 7) % 180 + 35) as u8;
                data.extend_from_slice(&[value, value.saturating_add(8), value]);
            }
        }
        let image = Rgb8::from_raw(width, height, data);
        let plan = Preparation::new(&image, 1.0);
        assert!(plan.kinds().contains(&CandidateKind::OppositePolarity));
    }

    #[test]
    fn clahe_handles_tiny_uniform_images_without_seams() {
        let gray = Gray {
            width: 2,
            height: 2,
            data: vec![120; 4],
        };
        let enhanced = clahe(&gray);
        assert_eq!((enhanced.width, enhanced.height), (2, 2));
        assert!(enhanced.data.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn clahe_preserves_mirrored_image_edges() {
        let axis = [
            10u8, 20, 40, 60, 80, 100, 120, 140, 140, 120, 100, 80, 60, 40, 20, 10,
        ];
        let mut gray = Gray::new(axis.len(), axis.len());
        for y in 0..gray.height {
            for x in 0..gray.width {
                gray.row_mut(y)[x] = ((u16::from(axis[x]) + u16::from(axis[y])) / 2) as u8;
            }
        }

        let enhanced = clahe(&gray);
        for y in 0..enhanced.height {
            for x in 0..enhanced.width {
                assert_eq!(
                    enhanced.row(y)[x],
                    enhanced.row(y)[enhanced.width - 1 - x],
                    "horizontal symmetry at ({x}, {y})"
                );
                assert_eq!(
                    enhanced.row(y)[x],
                    enhanced.row(enhanced.height - 1 - y)[x],
                    "vertical symmetry at ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn preparing_an_empty_image_is_a_no_op() {
        let empty = Rgb8::new(0, 0);
        let prepared = prepare(&empty, 3.0);
        assert!(prepared.is_empty());
    }
}
