//! End-of-capture global reconstruction.
//!
//! The online canvas is built greedily: each frame is matched only against
//! recent history, so one badly-matched bridge frame permanently bends the
//! result. At the end of a capture we have a bounded set of losslessly
//! compressed keyframes; this module builds a small temporal match graph over
//! them and rebuilds the canvas from the cheapest complete path, which lets a
//! damaged frame be skipped entirely (at a penalty).
//!
//! If no complete path validates, the caller keeps the online canvas. Silently
//! returning a partially reconstructed image would be worse than the greedy one.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::io::Read;

use vellum_core::image::Rgb8;

use crate::fixed_regions::FixedBands;
use crate::scoring::{MAX_PIXEL_DIFF, Mask, ROBUST_MAX_PIXEL_DIFF};
use crate::signature::{Cols, Sparse, is_static_view, matching_cols};

/// Lossless keyframes retained for reconstruction. Excludes one pending raw
/// accepted frame, matching the Python accounting.
pub const KEYFRAME_MEMORY_LIMIT: usize = 48 * 1024 * 1024;
pub const KEYFRAME_MAX_COUNT: usize = 160;
/// How far back the graph may reach when the adjacent edge looks unreliable.
pub const OFFLINE_EDGE_LOOKBACK: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyframeReason {
    Seed,
    Motion,
    Tail,
    Recovered,
    Turn,
    Failure,
}

impl KeyframeReason {
    /// Eviction priority: lower is dropped first.
    pub(crate) fn priority(self) -> u8 {
        match self {
            KeyframeReason::Motion => 0,
            KeyframeReason::Tail => 1,
            KeyframeReason::Recovered | KeyframeReason::Seed => 2,
            KeyframeReason::Turn => 3,
            KeyframeReason::Failure => 4,
        }
    }

    fn is_ordinary(self) -> bool {
        matches!(self, KeyframeReason::Motion | KeyframeReason::Tail)
    }
}

/// A frame kept for offline reconstruction. Pixels are zlib-compressed; the
/// matching state (signatures + sparse samples) stays uncompressed because the
/// graph search touches it repeatedly.
pub struct Keyframe {
    pub data: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub cols: Cols,
    pub pixels: Sparse,
    pub signature: Vec<u8>,
    pub sequence: u64,
    pub online_position: Option<i64>,
    pub reason: KeyframeReason,
}

impl Keyframe {
    pub fn memory_used(&self) -> usize {
        self.data.len()
            + self.cols.data.len() * std::mem::size_of::<f32>()
            + self.pixels.data.len()
            + self.signature.len()
    }

    fn decode(&self) -> Option<Rgb8> {
        let mut out = Vec::with_capacity(self.width * self.height * 3);
        let mut decoder = flate2::read::ZlibDecoder::new(&self.data[..]);
        decoder.read_to_end(&mut out).ok()?;
        if out.len() != self.width * self.height * 3 {
            return None;
        }
        Some(Rgb8::from_raw(self.width, self.height, out))
    }
}

pub fn compress_frame(frame: &Rgb8) -> Vec<u8> {
    use std::io::Write;
    // level 1: the capture is still warm in cache and the user is waiting; the
    // extra ratio from higher levels is not worth the latency.
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(1));
    encoder.write_all(&frame.data).expect("in-memory write");
    encoder.finish().expect("in-memory finish")
}

#[derive(Debug, Clone, Copy)]
struct GraphEdge {
    shift: i32,
    diff: f32,
    robust: bool,
    cost: f32,
}

/// Everything the graph needs from the live stitcher.
pub struct OfflineCtx<'a> {
    pub max_diff: f32,
    pub min_shift_px: u32,
    pub width: usize,
    pub row_mask: Option<&'a [bool]>,
    pub column_mask: Option<&'a [bool]>,
    pub bands: FixedBands,
}

impl OfflineCtx<'_> {
    fn mask(&self) -> Mask<'_> {
        Mask {
            rows: self.row_mask,
            columns: self.column_mask,
        }
    }

    /// One directed edge of the reconstruction graph, or `None` when the pair
    /// cannot be trusted at any offset.
    fn edge(&self, previous: &Keyframe, current: &Keyframe) -> Option<GraphEdge> {
        let previous_cols = matching_cols(&previous.cols, &previous.pixels, self.column_mask);
        let current_cols = matching_cols(&current.cols, &current.pixels, self.column_mask);

        // Prefer the shift implied by the online positions; fall back to 0.
        let mut predictions: Vec<i32> = Vec::with_capacity(2);
        if let (Some(a), Some(b)) = (previous.online_position, current.online_position) {
            predictions.push((b - a) as i32);
        }
        if !predictions.contains(&0) {
            predictions.push(0);
        }

        let mask = self.mask();
        let mut best: Option<GraphEdge> = None;
        for predict in predictions {
            let matched = crate::stitcher::find_match(
                (&previous_cols, &previous.pixels),
                (&current_cols, &current.pixels),
                predict,
                self.max_diff,
                &mask,
            );
            let (shift, diff, robust) = (matched.shift, matched.diff, matched.robust);
            if diff > self.max_diff {
                continue;
            }

            let candidate = if shift.unsigned_abs() < self.min_shift_px {
                // A still view is a legitimate zero-shift edge, but only when
                // the two frames really are the same view.
                if !is_static_view(
                    &previous.signature,
                    &current.signature,
                    &previous.pixels,
                    &current.pixels,
                ) {
                    continue;
                }
                GraphEdge {
                    shift: 0,
                    diff,
                    robust,
                    cost: diff + 0.25,
                }
            } else {
                if matched.false_motion {
                    continue;
                }
                GraphEdge {
                    shift,
                    diff,
                    robust,
                    cost: diff + if robust { 1.0 } else { 0.0 },
                }
            };
            if best.is_none_or(|current_best| candidate.cost < current_best.cost) {
                best = Some(candidate);
            }
            // A validated exact overlap at the online prediction needs no
            // second exhaustive search from zero. Keep the slower retry for
            // noisy/robust edges, where another prediction can still help.
            if !robust && diff < 0.25 && matched.aligned == 0.0 {
                break;
            }
        }
        best
    }
}

/// Rebuild the canvas from the keyframe graph, or `None` to keep the online one.
pub fn rebuild(frames: &[Keyframe], ctx: &OfflineCtx<'_>) -> Option<Rgb8> {
    if frames.len() < 2 {
        return None;
    }
    let path = shortest_path(frames, ctx)?;
    // Skipping a damaged bridge is useful; skipping the only spatial extremum
    // would silently erase captured content. Positions may be corrected by the
    // graph, so compare represented input coverage rather than output height.
    if online_span(frames.iter()) != online_span(path.iter().map(|(index, _)| &frames[*index])) {
        return None;
    }
    fuse(frames, &path, ctx)
}

fn online_span<'a>(frames: impl Iterator<Item = &'a Keyframe>) -> Option<(i64, i64)> {
    frames
        .filter_map(|frame| frame.online_position.map(|p| (p, p + frame.height as i64)))
        .fold(None, |span, (start, end)| {
            Some(match span {
                None => (start, end),
                Some((low, high)) => (low.min(start), high.max(end)),
            })
        })
}

/// Cheapest complete path from the first to the last keyframe, as
/// `(index, position)` pairs with the last frame pinned at position 0.
fn shortest_path(frames: &[Keyframe], ctx: &OfflineCtx<'_>) -> Option<Vec<(usize, i64)>> {
    let count = frames.len();
    let mut scores = vec![f32::INFINITY; count];
    let mut parents: Vec<Option<(usize, GraphEdge)>> = vec![None; count];
    scores[0] = 0.0;
    let skip_penalty = ctx.max_diff * 1.5;

    for current in 1..count {
        let mut cache: HashMap<usize, Option<GraphEdge>> = HashMap::new();
        let edge_for = |previous: usize, cache: &mut HashMap<usize, Option<GraphEdge>>| {
            *cache
                .entry(previous)
                .or_insert_with(|| ctx.edge(&frames[previous], &frames[current]))
        };

        let mut previous_indices = vec![current - 1];
        let adjacent = edge_for(current - 1, &mut cache);
        // The adjacent edge is the common case. Only probe older nodes when it
        // looks questionable, plus one periodic probe that catches a locally
        // attractive but wrong offset.
        let adjacent_weak = match adjacent {
            None => true,
            Some(edge) => edge.robust || edge.diff > ctx.max_diff * 0.65,
        };
        if adjacent_weak {
            previous_indices.extend(current.saturating_sub(OFFLINE_EDGE_LOOKBACK)..current - 1);
        } else if current % 4 == 0 && current > 4 {
            previous_indices.push(current - 4);
        }

        let mut seen = Vec::with_capacity(previous_indices.len());
        for previous in previous_indices {
            if seen.contains(&previous) {
                continue;
            }
            seen.push(previous);
            if !scores[previous].is_finite() {
                continue;
            }
            let Some(edge) = edge_for(previous, &mut cache) else {
                continue;
            };
            let skipped = (current - previous - 1) as f32;
            let candidate = scores[previous] + edge.cost + skipped * skip_penalty;
            if candidate < scores[current] {
                scores[current] = candidate;
                parents[current] = Some((previous, edge));
            }
        }
    }

    parents[count - 1]?;

    let mut cursor = count - 1;
    let mut position = 0i64;
    let mut reverse = vec![(cursor, position)];
    while cursor != 0 {
        let (previous, edge) = parents[cursor]?;
        position -= i64::from(edge.shift);
        reverse.push((previous, position));
        cursor = previous;
    }
    reverse.reverse();
    Some(reverse)
}

/// Compose the path with content-aware, whole-row source ownership.
/// Never average antialiased glyphs from different rendering states.
fn fuse(frames: &[Keyframe], path: &[(usize, i64)], ctx: &OfflineCtx<'_>) -> Option<Rgb8> {
    fuse_with_decoder(frames, path, ctx, Keyframe::decode)
}

// Keep decoded ownership visible so tests can measure live frame memory, not
// merely decode-call count. Production uses Rgb8 directly, without a wrapper.
fn fuse_with_decoder<D: Borrow<Rgb8>>(
    frames: &[Keyframe],
    path: &[(usize, i64)],
    ctx: &OfflineCtx<'_>,
    mut decode: impl FnMut(&Keyframe) -> Option<D>,
) -> Option<Rgb8> {
    let width = ctx.width;
    let &(first_index, first_position) = path.first()?;
    let first_keyframe = frames.get(first_index)?;
    let frame_height = first_keyframe.height;
    // Canvas geometry needs only metadata. Reject inconsistent viewports before
    // allocating/decoding; they cannot share the fusion weights below.
    for &(index, _) in path {
        let frame = frames.get(index)?;
        if frame.width != width || frame.height != frame_height {
            return None;
        }
    }
    let first_decoded = decode(first_keyframe)?;
    let first_frame = first_decoded.borrow();
    let mut bands = ctx.bands;
    // A band pair covering the whole viewport is a detection failure, not chrome.
    if bands.top + bands.bottom >= frame_height {
        bands = FixedBands {
            top: 0,
            bottom: 0,
            ..bands
        };
    }
    if bands.left + bands.right >= width {
        bands = FixedBands {
            left: 0,
            right: 0,
            ..bands
        };
    }

    // Sparse columns are enough for matching, not for destructive cropping.
    // Their midpoint boundary can include the first pixels of a scrolling
    // glyph. Only remove columns proven fixed in the decoded keyframes.
    // Keep only the first reference and one current frame alive. Refinement
    // must precede fusion, but a second decode pass is needed only for sidebars;
    // the compressed-store budget must not become N raw viewport allocations.
    if bands.left > 0 || bands.right > 0 {
        for &(index, _) in &path[1..] {
            let decoded = decode(&frames[index])?;
            let frame = decoded.borrow();
            bands.left = stable_sidebar_width(first_frame, frame, bands.left, false);
            bands.right = stable_sidebar_width(first_frame, frame, bands.right, true);
            if bands.left == 0 && bands.right == 0 {
                break;
            }
        }
    }

    let min_position = path
        .iter()
        .map(|&(_, position)| position + bands.top as i64)
        .min()?;
    let max_position = path
        .iter()
        .map(|&(_, position)| position + (frame_height - bands.bottom) as i64)
        .max()?;
    let content_height = (max_position - min_position).max(0) as usize;
    let center_start = bands.left;
    let center_end = width.checked_sub(bands.right)?;
    if content_height == 0 || center_end <= center_start {
        return None;
    }

    let mut canvas = Rgb8::new(width, content_height);
    let mut covered: Option<(usize, usize)> = None;
    for &(index, position) in path {
        // Keep at most the first reference plus one incoming decoded frame.
        let decoded;
        let frame = if index == first_index {
            first_frame
        } else {
            decoded = decode(&frames[index])?;
            decoded.borrow()
        };
        let content_rows = frame.height.checked_sub(bands.top + bands.bottom)?;
        if frame.width != width || content_rows != frame_height - bands.top - bands.bottom {
            return None;
        }
        let start = position + bands.top as i64 - min_position;
        if start < 0 || start as usize + content_rows > content_height {
            return None;
        }
        let start = start as usize;
        let end = start + content_rows;
        let mut feather = None;
        let (copy_start, copy_end) = match covered {
            None => (start, end),
            Some((low, high)) if start > high || end < low => return None,
            Some((low, high)) if end > high => {
                let overlap_start = start.max(low);
                let cut = crate::seam::choose(
                    &canvas,
                    overlap_start,
                    frame,
                    bands.top + overlap_start - start,
                    high - overlap_start,
                    center_start..center_end,
                );
                feather = crate::seam::background_patch(
                    (&canvas, overlap_start),
                    (frame, bands.top + overlap_start - start),
                    high - overlap_start,
                    cut,
                    true,
                    center_start..center_end,
                )
                .map(|patch| (overlap_start + cut, patch));
                (overlap_start + cut, end)
            }
            Some((low, high)) if start < low => {
                let cut = crate::seam::choose(
                    &canvas,
                    low,
                    frame,
                    bands.top + low - start,
                    end.min(high) - low,
                    center_start..center_end,
                );
                feather = crate::seam::background_patch(
                    (&canvas, low),
                    (frame, bands.top + low - start),
                    end.min(high) - low,
                    cut,
                    false,
                    center_start..center_end,
                )
                .map(|patch| (low + cut - patch.height, patch));
                (start, low + cut)
            }
            // Revisiting known rows must not mix another animation/font state
            // into already coherent text or repeatedly blur the same pixels.
            Some(_) => continue,
        };
        let columns = center_start * 3..center_end * 3;
        for target_row in copy_start..copy_end {
            let source = frame.row(bands.top + target_row - start);
            canvas.row_mut(target_row)[columns.clone()].copy_from_slice(&source[columns.clone()]);
        }
        if let Some((at, patch)) = feather {
            for y in 0..patch.height {
                canvas.row_mut(at + y)[columns.clone()]
                    .copy_from_slice(&patch.row(y)[columns.clone()]);
            }
        }
        covered = Some(covered.map_or((start, end), |(low, high)| (low.min(start), high.max(end))));
    }
    if covered != Some((0, content_height)) {
        return None;
    }

    // Fixed sidebars cannot be repeated down the page. A solid page margin is
    // already neutral background: preserve its colour rather than dragging the
    // nearest scrolling glyph into it. Non-uniform chrome keeps the existing
    // edge-fill policy; the real sidebar is pasted once below.
    if bands.left > 0 || bands.right > 0 {
        let left_background = uniform_band(first_frame, 0, center_start);
        let right_background = uniform_band(first_frame, center_end, width);
        for y in 0..content_height {
            let row = canvas.row_mut(y);
            if bands.left > 0 {
                let edge = left_background.unwrap_or([
                    row[center_start * 3],
                    row[center_start * 3 + 1],
                    row[center_start * 3 + 2],
                ]);
                for x in 0..center_start {
                    row[x * 3..x * 3 + 3].copy_from_slice(&edge);
                }
            }
            if bands.right > 0 {
                let base = (center_end - 1) * 3;
                let edge = right_background.unwrap_or([row[base], row[base + 1], row[base + 2]]);
                for x in center_end..width {
                    row[x * 3..x * 3 + 3].copy_from_slice(&edge);
                }
            }
        }
        let start = first_position + bands.top as i64 - min_position;
        let content_rows = first_frame.height - bands.top - bands.bottom;
        if start < 0 || start as usize + content_rows > content_height {
            return None;
        }
        let start = start as usize;
        for row in 0..content_rows {
            let source = first_frame.row(bands.top + row);
            let target = canvas.row_mut(start + row);
            if bands.left > 0 {
                target[..center_start * 3].copy_from_slice(&source[..center_start * 3]);
            }
            if bands.right > 0 {
                target[center_end * 3..].copy_from_slice(&source[center_end * 3..]);
            }
        }
    }

    // A fixed header/footer belongs in the output exactly once.
    let mut parts: Vec<Rgb8> = Vec::with_capacity(3);
    if bands.top > 0 {
        parts.push(first_frame.rows_slice(0, bands.top));
    }
    parts.push(canvas);
    if bands.bottom > 0 {
        parts.push(first_frame.rows_slice(first_frame.height - bands.bottom, first_frame.height));
    }
    Some(if parts.len() == 1 {
        parts.pop().unwrap()
    } else {
        Rgb8::vstack(&parts)
    })
}

/// Refine an approximate sparse band inward, never outward. A changed pixel
/// keeps its column in the scrolling image; noisy/animated chrome therefore
/// degrades conservatively instead of deleting real text.
fn stable_sidebar_width(first: &Rgb8, frame: &Rgb8, limit: usize, right: bool) -> usize {
    if frame.width != first.width || frame.height != first.height {
        return 0;
    }
    for depth in 0..limit.min(first.width) {
        let x = if right {
            first.width - 1 - depth
        } else {
            depth
        };
        if (0..first.height)
            .any(|y| first.row(y)[x * 3..x * 3 + 3] != frame.row(y)[x * 3..x * 3 + 3])
        {
            return depth;
        }
    }
    limit.min(first.width)
}

/// A solid excluded margin needs no invented content when the canvas grows.
fn uniform_band(frame: &Rgb8, start: usize, end: usize) -> Option<[u8; 3]> {
    if start >= end || end > frame.width || frame.height == 0 {
        return None;
    }
    let colour: [u8; 3] = frame.row(0)[start * 3..start * 3 + 3].try_into().ok()?;
    (0..frame.height)
        .all(|y| {
            frame.row(y)[start * 3..end * 3]
                .as_chunks::<3>()
                .0
                .iter()
                .all(|pixel| *pixel == colour)
        })
        .then_some(colour)
}

/// Evict keyframes until the memory and count caps hold. Returns false when the
/// set shrank to the two endpoints and offline rebuild must be abandoned.
pub fn trim(frames: &mut Vec<Keyframe>, memory_used: &mut usize, limit: usize) -> bool {
    while *memory_used > limit || frames.len() > KEYFRAME_MAX_COUNT {
        if frames.len() <= 2 {
            return false;
        }
        // Never drop the endpoints: they anchor the path.
        let interior: Vec<usize> = (1..frames.len() - 1).collect();
        let ordinary: Vec<usize> = interior
            .iter()
            .copied()
            .filter(|i| frames[*i].reason.is_ordinary())
            .collect();
        let pool = if ordinary.is_empty() {
            &interior
        } else {
            &ordinary
        };
        let remove_at = *pool
            .iter()
            .min_by_key(|index| {
                let frame = &frames[**index];
                let span = frames[**index + 1].sequence - frames[**index - 1].sequence;
                (frame.reason.priority(), span)
            })
            .expect("pool is non-empty");
        let removed = frames.remove(remove_at);
        *memory_used = memory_used.saturating_sub(removed.memory_used());
    }
    true
}

/// Absolute sparse-RGB limits are re-exported so callers cannot drift from the
/// values the graph itself enforces.
pub const PIXEL_LIMITS: (f32, f32) = (MAX_PIXEL_DIFF, ROBUST_MAX_PIXEL_DIFF);

#[cfg(test)]
mod tests {
    use super::*;

    fn keyframe(sequence: u64, reason: KeyframeReason) -> Keyframe {
        Keyframe {
            data: vec![0; 1024],
            width: 1,
            height: 1,
            cols: Cols {
                height: 0,
                data: Vec::new(),
            },
            pixels: Sparse {
                height: 0,
                columns: 0,
                data: Vec::new(),
            },
            signature: Vec::new(),
            sequence,
            online_position: None,
            reason,
        }
    }

    #[test]
    fn trim_keeps_endpoints_and_drops_ordinary_frames_first() {
        let mut frames = vec![
            keyframe(1, KeyframeReason::Seed),
            keyframe(2, KeyframeReason::Motion),
            keyframe(3, KeyframeReason::Turn),
            keyframe(4, KeyframeReason::Tail),
        ];
        let mut used = 4096;
        assert!(trim(&mut frames, &mut used, 3072));
        assert_eq!(frames.len(), 3);
        // The turn keyframe must survive; a motion one is evicted instead.
        assert!(frames.iter().any(|f| f.reason == KeyframeReason::Turn));
        assert_eq!(frames.first().unwrap().sequence, 1);
        assert_eq!(frames.last().unwrap().sequence, 4);
    }

    #[test]
    fn trim_gives_up_instead_of_exceeding_the_cap() {
        let mut frames = vec![
            keyframe(1, KeyframeReason::Seed),
            keyframe(2, KeyframeReason::Motion),
        ];
        let mut used = 4096;
        assert!(!trim(&mut frames, &mut used, 16));
    }

    fn image_keyframe(image: &Rgb8, sequence: u64) -> Keyframe {
        Keyframe {
            data: compress_frame(image),
            width: image.width,
            height: image.height,
            ..keyframe(sequence, KeyframeReason::Motion)
        }
    }

    fn fusion_context(width: usize, bands: FixedBands) -> OfflineCtx<'static> {
        OfflineCtx {
            max_diff: crate::DEFAULT_MAX_DIFF,
            min_shift_px: crate::DEFAULT_MIN_SHIFT_PX,
            width,
            row_mask: None,
            column_mask: None,
            bands,
        }
    }

    #[test]
    fn fusion_preserves_pixel_order_and_refined_fixed_bands() {
        // Whole scrolling rows must come from one captured state, including
        // low-contrast changes, upward growth, revisits and refined sidebars.
        let positions = [0, -3, 0, 3, 6, 9];
        let frames: Vec<_> = positions
            .iter()
            .enumerate()
            .map(|(i, &position)| {
                let mut image = Rgb8::new(12, 10);
                for y in 0..10 {
                    for x in 0..12 {
                        let pixel = if x == 0 {
                            [240; 3] // uniform left page margin
                        } else if x == 11 {
                            [y as u8 * 17; 3] // non-uniform right chrome
                        } else if y == 0 || y == 9 {
                            [x as u8 * 13; 3]
                        } else {
                            let document_y = (y as i64 + position + 3) as usize;
                            let base = ((document_y * 17 + x * 7) % 150 + 30) as u8;
                            if x == 5 && i % 2 == 1 {
                                [250, 10, 80]
                            } else {
                                [base + i as u8, base, base + (y % 3) as u8]
                            }
                        };
                        image.row_mut(y)[x * 3..x * 3 + 3].copy_from_slice(&pixel);
                    }
                }
                image_keyframe(&image, i as u64)
            })
            .collect();
        let path: Vec<_> = positions.into_iter().enumerate().collect();
        for bands in [
            FixedBands::default(),
            FixedBands {
                top: 1,
                bottom: 1,
                left: 2,
                right: 2,
            },
        ] {
            let image = fuse(&frames, &path, &fusion_context(12, bands)).unwrap();
            assert_eq!((image.width, image.height), (12, 22));
            for y in bands.top..image.height - bands.bottom {
                let document_y = y as i64 - 3;
                assert!(
                    path.iter().any(|&(index, position)| {
                        let local = document_y - position;
                        if local < bands.top as i64 || local >= (10 - bands.bottom) as i64 {
                            return false;
                        }
                        let frame = frames[index].decode().unwrap();
                        // The approximate two-column crop must refine to one:
                        // otherwise scrolling pixels in columns 1/10 are destroyed.
                        image.row(y)[3..33] == frame.row(local as usize)[3..33]
                    }),
                    "row {y} mixes frame states or crops scrolling pixels: {bands:?}"
                );
                assert_eq!(image.pixel(0, y), [240; 3]);
            }
        }
    }

    #[test]
    fn text_residuals_never_mix_two_halves_of_a_glyph() {
        for positions in [[0i64, 24], [24, 0]] {
            for residual in [-2i64, 2] {
                for ink in [20, 220] {
                    // high contrast and antialiased/low contrast
                    let frames: Vec<_> = positions
                        .iter()
                        .enumerate()
                        .map(|(i, &position)| {
                            let mut image = Rgb8::from_raw(32, 64, vec![245; 32 * 64 * 3]);
                            let glyph_top = 40 + if i == 0 { 0 } else { residual };
                            for y in glyph_top..glyph_top + 8 {
                                let row = (y - position) as usize;
                                image.row_mut(row)[8 * 3..16 * 3].fill(ink);
                            }
                            image_keyframe(&image, i as u64)
                        })
                        .collect();
                    let path = [(0, positions[0]), (1, positions[1]), (0, positions[0])];
                    let image =
                        fuse(&frames, &path, &fusion_context(32, FixedBands::default())).unwrap();
                    assert_eq!(image.height, 88);
                    let rows: Vec<_> = (0..image.height)
                        .filter(|&y| image.pixel(12, y)[0] != 245)
                        .collect();
                    assert_eq!(rows.len(), 8, "a complete glyph became {rows:?}");
                    assert_eq!(rows[7] - rows[0], 7, "glyph was split at a seam");
                    assert!(
                        image.data.iter().all(|&v| v == 245 || v == ink),
                        "invented blended text pixels"
                    );
                }
            }
        }
    }

    #[derive(Default)]
    struct DecodeMemory {
        live_bytes: std::cell::Cell<usize>,
        peak_bytes: std::cell::Cell<usize>,
        calls: std::cell::Cell<usize>,
    }

    struct TrackedDecode<'a> {
        image: Rgb8,
        memory: &'a DecodeMemory,
    }

    impl DecodeMemory {
        fn decode(&self, keyframe: &Keyframe) -> Option<TrackedDecode<'_>> {
            let image = keyframe.decode()?;
            let bytes = self.live_bytes.get() + image.data.len();
            self.live_bytes.set(bytes);
            self.peak_bytes.set(self.peak_bytes.get().max(bytes));
            self.calls.set(self.calls.get() + 1);
            Some(TrackedDecode {
                image,
                memory: self,
            })
        }
    }

    impl Borrow<Rgb8> for TrackedDecode<'_> {
        fn borrow(&self) -> &Rgb8 {
            &self.image
        }
    }

    impl Drop for TrackedDecode<'_> {
        fn drop(&mut self) {
            self.memory
                .live_bytes
                .set(self.memory.live_bytes.get() - self.image.data.len());
        }
    }

    #[test]
    fn fusion_holds_at_most_two_decoded_frames_regardless_of_path_length() {
        let image = Rgb8::from_raw(512, 256, vec![42; 512 * 256 * 3]);
        let frames: Vec<_> = (0..KEYFRAME_MAX_COUNT)
            .map(|i| image_keyframe(&image, i as u64))
            .collect();
        let path: Vec<_> = (0..frames.len()).map(|i| (i, 0)).collect();
        assert!(frames.iter().map(Keyframe::memory_used).sum::<usize>() < KEYFRAME_MEMORY_LIMIT);
        assert!(image.data.len() * frames.len() > KEYFRAME_MEMORY_LIMIT);

        for bands in [
            FixedBands::default(),
            FixedBands {
                top: 1,
                bottom: 1,
                left: 1,
                right: 1,
            },
        ] {
            let memory = DecodeMemory::default();
            let output =
                fuse_with_decoder(&frames, &path, &fusion_context(image.width, bands), |key| {
                    memory.decode(key)
                })
                .unwrap();
            assert_eq!(output, image);
            assert_eq!(memory.peak_bytes.get(), 2 * image.data.len());
            assert_eq!(
                memory.live_bytes.get(),
                0,
                "all decoded frames must be released"
            );
            let expected_calls = if bands.left == 0 {
                frames.len()
            } else {
                2 * frames.len() - 1
            };
            assert_eq!(memory.calls.get(), expected_calls);
        }
    }

    #[test]
    fn fusion_discards_partial_output_and_releases_decodes_on_corruption() {
        let image = Rgb8::from_raw(8, 10, vec![42; 240]);
        for bands in [
            FixedBands::default(),
            FixedBands {
                top: 1,
                bottom: 1,
                left: 1,
                right: 1,
            },
        ] {
            for corrupt_index in 0..3 {
                let mut frames: Vec<_> = (0..3).map(|i| image_keyframe(&image, i)).collect();
                frames[corrupt_index].data = vec![0];
                let memory = DecodeMemory::default();
                assert!(
                    fuse_with_decoder(
                        &frames,
                        &[(0, 0), (1, 3), (2, 6)],
                        &fusion_context(8, bands),
                        |key| memory.decode(key)
                    )
                    .is_none()
                );
                assert_eq!(memory.live_bytes.get(), 0);
                assert!(memory.peak_bytes.get() <= 2 * image.data.len());
            }
        }
    }

    #[test]
    fn fusion_skips_damaged_off_path_frames_but_rejects_gaps_and_size_changes() {
        let image = Rgb8::from_raw(8, 10, vec![42; 240]);
        let mut frames: Vec<_> = (0..3).map(|i| image_keyframe(&image, i)).collect();
        frames[1].data = vec![0];
        let ctx = fusion_context(8, FixedBands::default());
        let output = fuse(&frames, &[(0, 0), (2, 3)], &ctx).unwrap();
        assert_eq!(output, Rgb8::from_raw(8, 13, vec![42; 8 * 13 * 3]));
        assert!(fuse(&frames, &[(0, 0), (2, 11)], &ctx).is_none());
        frames[2] = image_keyframe(&Rgb8::new(8, 9), 2);
        assert!(fuse(&frames, &[(0, 0), (2, 3)], &ctx).is_none());
        frames[2] = image_keyframe(&Rgb8::new(7, 10), 2);
        assert!(fuse(&frames, &[(0, 0), (2, 3)], &ctx).is_none());
    }

    #[test]
    fn compressed_frame_roundtrips() {
        let mut frame = Rgb8::new(4, 3);
        frame.row_mut(1)[0..3].copy_from_slice(&[9, 8, 7]);
        let key = Keyframe {
            data: compress_frame(&frame),
            width: 4,
            height: 3,
            cols: Cols {
                height: 0,
                data: Vec::new(),
            },
            pixels: Sparse {
                height: 0,
                columns: 0,
                data: Vec::new(),
            },
            signature: Vec::new(),
            sequence: 1,
            online_position: Some(0),
            reason: KeyframeReason::Seed,
        };
        assert_eq!(key.decode().unwrap(), frame);
    }
}
