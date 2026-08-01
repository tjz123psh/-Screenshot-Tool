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

use std::collections::HashMap;
use std::io::Read;

use vellum_core::image::Rgb8;

use crate::fixed_regions::FixedBands;
use crate::scoring::{
    self, FUSION_MAX_PIXEL_DELTA, MAX_PIXEL_DIFF, Mask, ROBUST_MAX_PIXEL_DIFF, is_false_motion,
};
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
    fn priority(self) -> u8 {
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

    fn find_shift(
        &self,
        previous: &Cols,
        current: &Cols,
        predict: i32,
        robust: bool,
    ) -> (i32, f32) {
        crate::stitcher::find_shift_for(previous, current, predict, robust, &self.mask())
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
            let (mut shift, mut diff) =
                self.find_shift(&previous_cols, &current_cols, predict, false);
            let mut robust = false;
            if diff > self.max_diff {
                let (robust_shift, robust_diff) =
                    self.find_shift(&previous_cols, &current_cols, predict, true);
                if robust_diff < diff {
                    shift = robust_shift;
                    diff = robust_diff;
                    robust = robust_diff <= self.max_diff;
                }
            }
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
                let aligned = if robust {
                    scoring::robust_pixel_overlap_diff(
                        &previous.pixels,
                        &current.pixels,
                        shift,
                        &mask,
                    )
                } else {
                    scoring::pixel_overlap_diff(&previous.pixels, &current.pixels, shift, &mask)
                };
                let stationary = if robust {
                    scoring::robust_pixel_overlap_diff(&previous.pixels, &current.pixels, 0, &mask)
                } else {
                    scoring::pixel_overlap_diff(&previous.pixels, &current.pixels, 0, &mask)
                };
                let changed =
                    scoring::pixel_change_fraction(&previous.pixels, &current.pixels, &mask);
                if is_false_motion(aligned, stationary, changed, robust) {
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
    fuse(frames, &path, ctx)
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

/// Blend the path's frames into one canvas with feathered viewport weights.
fn fuse(frames: &[Keyframe], path: &[(usize, i64)], ctx: &OfflineCtx<'_>) -> Option<Rgb8> {
    let width = ctx.width;
    let mut decoded: Vec<(i64, Rgb8)> = Vec::with_capacity(path.len());
    for &(index, position) in path {
        let frame = frames[index].decode()?;
        if frame.width != width {
            return None;
        }
        decoded.push((position, frame));
    }

    let frame_height = decoded[0].1.height;
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

    let min_position = decoded
        .iter()
        .map(|(position, _)| position + bands.top as i64)
        .min()?;
    let max_position = decoded
        .iter()
        .map(|(position, frame)| position + (frame.height - bands.bottom) as i64)
        .max()?;
    let content_height = (max_position - min_position).max(0) as usize;
    let center_start = bands.left;
    let center_end = width.checked_sub(bands.right)?;
    if content_height == 0 || center_end <= center_start {
        return None;
    }

    let mut canvas = Rgb8::new(width, content_height);
    // Triangular viewport weight: rows near the viewport centre dominate, rows
    // entering/leaving at the edges taper to one. Without this a screen-fixed
    // wallpaper behind a translucent window visibly jumps at frame boundaries.
    let weights: Vec<u32> = (0..frame_height)
        .map(|y| ((y + 1) as u32).min((frame_height - y) as u32))
        .collect();
    let content_weights = &weights[bands.top..frame_height - bands.bottom];
    let mut weight_sum = vec![0u32; content_height];
    let mut peak_weight = vec![0u32; content_height];

    for (position, frame) in &decoded {
        let content_rows = frame.height.checked_sub(bands.top + bands.bottom)?;
        if content_rows != content_weights.len() {
            return None;
        }
        let start = position + bands.top as i64 - min_position;
        if start < 0 || start as usize + content_rows > content_height {
            return None;
        }
        let start = start as usize;

        for (row, &incoming_weight) in content_weights.iter().enumerate().take(content_rows) {
            let target_row = start + row;
            let source = frame.row(bands.top + row);
            let existing = weight_sum[target_row];
            let prefer_incoming = incoming_weight >= peak_weight[target_row];
            let target = canvas.row_mut(target_row);

            if existing == 0 {
                let range = center_start * 3..center_end * 3;
                target[range.clone()].copy_from_slice(&source[range]);
            } else {
                let old_weight = u64::from(existing);
                let new_weight = u64::from(incoming_weight);
                let total = old_weight + new_weight;
                for x in center_start..center_end {
                    let base = x * 3;
                    let old = [target[base], target[base + 1], target[base + 2]];
                    let incoming = [source[base], source[base + 1], source[base + 2]];
                    let delta = (0..3)
                        .map(|c| (i16::from(old[c]) - i16::from(incoming[c])).abs())
                        .max()
                        .unwrap_or(0);
                    if delta <= FUSION_MAX_PIXEL_DELTA {
                        for c in 0..3 {
                            let blended = (u64::from(old[c]) * old_weight
                                + u64::from(incoming[c]) * new_weight
                                + total / 2)
                                / total;
                            target[base + c] = blended as u8;
                        }
                    } else if prefer_incoming {
                        // Keep one complete state for high-contrast local
                        // change, so a caret or video tile is not ghosted.
                        target[base..base + 3].copy_from_slice(&incoming);
                    }
                }
            }
            weight_sum[target_row] += incoming_weight;
            peak_weight[target_row] = peak_weight[target_row].max(incoming_weight);
        }
    }

    if weight_sum.contains(&0) {
        return None;
    }

    // Fixed sidebars cannot be repeated down the page: extend the nearest
    // scrolling pixel as neutral background, then paste the real sidebar once.
    if bands.left > 0 || bands.right > 0 {
        for y in 0..content_height {
            let row = canvas.row_mut(y);
            if bands.left > 0 {
                let edge = [
                    row[center_start * 3],
                    row[center_start * 3 + 1],
                    row[center_start * 3 + 2],
                ];
                for x in 0..center_start {
                    row[x * 3..x * 3 + 3].copy_from_slice(&edge);
                }
            }
            if bands.right > 0 {
                let base = (center_end - 1) * 3;
                let edge = [row[base], row[base + 1], row[base + 2]];
                for x in center_end..width {
                    row[x * 3..x * 3 + 3].copy_from_slice(&edge);
                }
            }
        }
        let (first_position, first_frame) = &decoded[0];
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
    let first_frame = &decoded[0].1;
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
