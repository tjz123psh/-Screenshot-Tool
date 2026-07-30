//! The online stitcher: accepts frames as they are captured, grows a canvas in
//! both directions, and rebuilds a better canvas offline when the capture ends.
//!
//! Ported from `vellum/longshot/stitcher.py`. The invariants that must not
//! drift (see AGENTS.md "不可放宽的行为契约"):
//!
//!   * row-signature matching plus a sparse-RGB verification, never template
//!     matching;
//!   * incremental block canvas, flattened exactly once in [`Stitcher::result`];
//!   * a confident-but-tiny shift does *not* refresh the matching reference, so
//!     several sub-threshold scrolls accumulate and eventually append;
//!   * a low-confidence frame does not advance the reference either, so a later
//!     frame can still reconnect to recent history.

use std::collections::VecDeque;
use std::sync::Arc;

use vellum_core::image::Rgb8;

use crate::canvas::{Canvas, Side};
use crate::fixed_regions::FixedRegionDetector;
use crate::offline::{self, KEYFRAME_MEMORY_LIMIT, Keyframe, KeyframeReason, OfflineCtx};
use crate::scoring::{
    Mask, col_diff, is_false_motion, pixel_change_fraction, pixel_overlap_diff, robust_col_diff,
    robust_pixel_overlap_diff,
};
use crate::signature::{
    Cols, Sparse, compute_cols, effective_min_overlap, frame_signature, is_duplicate,
    offset_candidates, sample_pixels,
};

/// How many recently accepted frames stay available for re-matching.
const HISTORY_LEN: usize = 6;

#[derive(Debug)]
pub struct StitchResult {
    pub image: Rgb8,
    pub frames_used: usize,
    pub warnings: Vec<String>,
    /// True when the offline keyframe graph produced the returned image.
    pub rebuilt: bool,
}

/// Matching state for a recently accepted frame. `position` is the frame's top
/// row in canvas coordinates and shifts when the canvas grows upward.
#[derive(Clone)]
struct Tracked {
    cols: Arc<Cols>,
    pixels: Arc<Sparse>,
    position: i64,
}

/// One accepted frame kept raw so a direction change can still be promoted to a
/// keyframe after the fact. Exactly one of these exists at a time, which is the
/// documented "one pending raw frame" outside the keyframe memory cap.
struct Pending {
    frame: Rgb8,
    cols: Arc<Cols>,
    pixels: Arc<Sparse>,
    signature: Arc<Vec<u8>>,
    sequence: u64,
    online_position: Option<i64>,
}

/// One scored candidate from the history sweep.
struct Candidate {
    diff: f32,
    shift: i32,
    position: i64,
    history_index: usize,
    false_motion: bool,
    robust: bool,
}

pub struct Stitcher {
    pub max_diff: f32,
    pub min_shift_px: u32,

    canvas: Canvas,
    last_cols: Option<Arc<Cols>>,
    last_pixels: Option<Arc<Sparse>>,
    last_signature: Option<Arc<Vec<u8>>>,
    last_offset: i32,
    /// Canvas row of the most recently accepted frame's top edge.
    anchor_pos: i64,

    pub frames_used: usize,
    pub warnings: Vec<String>,
    /// Signed relative scroll of the last `add` (+down / -up).
    pub last_shift: i32,
    /// New rows the last `add` contributed (0 when re-traversing known area).
    pub last_added: usize,
    /// Overlap diff of the last `add`. LOWER is better.
    pub last_diff: f32,
    /// True when the last `add` needed the robust path or older history.
    pub last_recovered: bool,

    history: VecDeque<Tracked>,
    sequence: u64,
    fixed_regions: Option<FixedRegionDetector>,

    keyframes: Vec<Keyframe>,
    keyframe_memory_limit: usize,
    keyframe_memory_used: usize,
    offline_disabled: bool,
    pending_motion: Option<Pending>,
    last_motion_direction: i32,
    failure_run: u32,
}

impl Stitcher {
    pub fn new(max_diff: f32, min_shift_px: u32) -> Self {
        Self::with_options(max_diff, min_shift_px, true, KEYFRAME_MEMORY_LIMIT)
    }

    /// `preview` enables the incremental thumbnail; the recorder disables it
    /// because its panel shows a text readout instead of a live image.
    pub fn with_options(
        max_diff: f32,
        min_shift_px: u32,
        preview: bool,
        keyframe_memory_limit: usize,
    ) -> Self {
        Self {
            max_diff,
            min_shift_px,
            canvas: Canvas::new(preview),
            last_cols: None,
            last_pixels: None,
            last_signature: None,
            last_offset: 0,
            anchor_pos: 0,
            frames_used: 0,
            warnings: Vec::new(),
            last_shift: 0,
            last_added: 0,
            last_diff: 0.0,
            last_recovered: false,
            history: VecDeque::with_capacity(HISTORY_LEN),
            sequence: 0,
            fixed_regions: None,
            keyframes: Vec::new(),
            keyframe_memory_limit,
            keyframe_memory_used: 0,
            offline_disabled: keyframe_memory_limit == 0,
            pending_motion: None,
            last_motion_direction: 0,
            failure_run: 0,
        }
    }

    pub fn current_height(&self) -> usize {
        self.canvas.height()
    }

    pub fn keyframe_memory_used(&self) -> usize {
        self.keyframe_memory_used
    }

    pub fn preview_thumbnail(&self, max_w: usize, max_h: usize) -> Option<Rgb8> {
        self.canvas.preview(max_w, max_h)
    }

    /// Add a frame. Returns the overlap diff (LOWER is better; 0.0 for the
    /// first frame). A value above `max_diff` means the frame was not appended.
    pub fn add(&mut self, frame: &Rgb8) -> f32 {
        self.sequence += 1;

        if self.canvas.is_empty() {
            return self.seed(frame);
        }

        // A mid-capture output resize must not abort the shot.
        let fitted;
        let arr = if frame.width != self.canvas.width() {
            fitted = frame.fit_width(self.canvas.width());
            &fitted
        } else {
            frame
        };

        // Cheap early-out: the view has not moved at all.
        let sig = Arc::new(frame_signature(arr));
        if let Some(last) = &self.last_signature
            && is_duplicate(last, &sig)
        {
            self.last_shift = 0;
            self.last_added = 0;
            self.last_diff = 0.0;
            return 0.0;
        }

        let pixels = Arc::new(sample_pixels(arr));
        let cols = Arc::new(compute_cols(&pixels));

        let row_mask = self.fixed_row_mask();
        let column_mask = self.fixed_column_mask();
        let rows = row_mask.as_deref();
        let columns = column_mask.as_deref();
        let current_match_cols = matching_cols(&cols, &pixels, columns);

        let mut matches: Vec<Candidate> = Vec::with_capacity(HISTORY_LEN);
        let history: Vec<Tracked> = self.history.iter().rev().cloned().collect();

        for (index, tracked) in history.iter().enumerate() {
            let predict = if index == 0 { self.last_offset } else { 0 };
            let tracked_match_cols = matching_cols(&tracked.cols, &tracked.pixels, columns);

            let (mut shift, mut diff) = self.find_shift_for(
                &tracked_match_cols,
                &current_match_cols,
                predict,
                false,
                rows,
            );
            let mut robust = false;
            if diff > self.max_diff {
                let (robust_shift, robust_diff) = self.find_shift_for(
                    &tracked_match_cols,
                    &current_match_cols,
                    predict,
                    true,
                    rows,
                );
                if robust_diff < diff {
                    shift = robust_shift;
                    diff = robust_diff;
                    robust = robust_diff <= self.max_diff;
                }
            }

            // With a single history entry there is nothing else to consult, so
            // record the failure and stop instead of paying for pixel checks.
            if diff > self.max_diff && index == 0 && history.len() == 1 {
                matches.push(Candidate {
                    diff,
                    shift,
                    position: tracked.position,
                    history_index: index,
                    false_motion: false,
                    robust,
                });
                continue;
            }

            let mask = Mask { rows, columns };
            let changed = pixel_change_fraction(&tracked.pixels, &pixels, &mask);
            let (aligned, stationary) = if robust {
                (
                    robust_pixel_overlap_diff(&tracked.pixels, &pixels, shift, &mask),
                    robust_pixel_overlap_diff(&tracked.pixels, &pixels, 0, &mask),
                )
            } else {
                (
                    pixel_overlap_diff(&tracked.pixels, &pixels, shift, &mask),
                    pixel_overlap_diff(&tracked.pixels, &pixels, 0, &mask),
                )
            };
            let false_motion = is_false_motion(aligned, stationary, changed, robust);

            let good =
                diff <= self.max_diff && shift.unsigned_abs() >= self.min_shift_px && !false_motion;
            matches.push(Candidate {
                diff,
                shift,
                position: tracked.position,
                history_index: index,
                false_motion,
                robust,
            });
            if good {
                break;
            }
        }

        let valid_best = matches
            .iter()
            .filter(|m| {
                m.diff <= self.max_diff
                    && m.shift.unsigned_abs() >= self.min_shift_px
                    && !m.false_motion
            })
            .min_by(|a, b| a.diff.total_cmp(&b.diff));

        let (diff, shift, position, recovered, had_valid) = match valid_best {
            Some(best) => (
                best.diff,
                best.shift,
                best.position,
                best.robust || best.history_index != 0,
                true,
            ),
            None => {
                let fallback = matches
                    .iter()
                    .min_by(|a, b| a.diff.total_cmp(&b.diff))
                    .expect("history is never empty here");
                (
                    fallback.diff,
                    fallback.shift,
                    fallback.position,
                    false,
                    false,
                )
            }
        };

        self.last_shift = shift;
        self.last_added = 0;
        self.last_diff = diff;
        self.last_recovered = recovered;

        if diff > self.max_diff {
            // Low confidence: keep the frame out and do NOT advance the
            // reference, so a later frame can reconnect to history.
            self.note_failure(arr, &cols, &pixels, &sig);
            return diff;
        }
        if shift.unsigned_abs() < self.min_shift_px {
            // Confident but essentially the same view. Deliberately keep the
            // old reference so small scrolls accumulate.
            //
            // The detector still has to see this pair. A viewport-fixed region
            // (window chrome baked into every frame) matches perfectly at offset
            // zero, so it drags the score minimum to "did not move" and lands
            // here on every frame. Feeding the detector only on accepted frames
            // therefore deadlocks: it needs three observations to activate, and
            // the very condition it exists to cancel is what stops it from ever
            // getting them. The output collapses to a single viewport.
            //
            // Safe to feed from a rejected pair: `observe` re-checks the frame
            // shape, a band wider than 45% of the axis is refused, and an
            // all-unchanged observation (the user simply paused) is cleared
            // instead of being read as "the whole viewport is chrome".
            self.observe_fixed_regions(&pixels);
            return diff;
        }
        if !had_valid {
            self.last_shift = 0;
            self.last_diff = diff;
            return diff;
        }

        let new_pos = position + i64::from(shift);
        self.extend_canvas(arr, new_pos);

        self.last_cols = Some(Arc::clone(&cols));
        self.last_pixels = Some(Arc::clone(&pixels));
        self.last_signature = Some(Arc::clone(&sig));
        self.last_offset = shift;
        self.frames_used += 1;

        self.observe_fixed_regions(&pixels);
        self.push_history(Tracked {
            cols: Arc::clone(&cols),
            pixels: Arc::clone(&pixels),
            position: self.anchor_pos,
        });
        self.note_motion(arr, &cols, &pixels, &sig, shift);
        diff
    }

    fn seed(&mut self, frame: &Rgb8) -> f32 {
        self.canvas.set_width(frame.width);
        self.canvas.push(frame.clone(), Side::Bottom);

        let pixels = Arc::new(sample_pixels(frame));
        let cols = Arc::new(compute_cols(&pixels));
        let sig = Arc::new(frame_signature(frame));
        self.fixed_regions = Some(FixedRegionDetector::new(frame.height, pixels.columns));

        self.last_cols = Some(Arc::clone(&cols));
        self.last_pixels = Some(Arc::clone(&pixels));
        self.last_signature = Some(Arc::clone(&sig));
        self.last_offset = 0;
        self.anchor_pos = 0;
        self.frames_used = 1;
        self.last_shift = 0;
        self.last_added = frame.height;
        self.last_diff = 0.0;
        self.last_recovered = false;
        self.push_history(Tracked {
            cols: Arc::clone(&cols),
            pixels: Arc::clone(&pixels),
            position: 0,
        });

        self.remember_keyframe(
            frame,
            &cols,
            &pixels,
            &sig,
            self.sequence,
            Some(0),
            KeyframeReason::Seed,
        );
        if !self.offline_disabled {
            self.pending_motion = Some(Pending {
                frame: frame.clone(),
                cols,
                pixels,
                signature: sig,
                sequence: self.sequence,
                online_position: Some(0),
            });
        }
        0.0
    }

    /// Blit the frame at `new_pos` (its top row in canvas coordinates), growing
    /// whichever edge it overhangs. Both directions are O(rows added).
    fn extend_canvas(&mut self, arr: &Rgb8, new_pos: i64) {
        let h = arr.height as i64;
        let canvas_h = self.canvas.height() as i64;

        let over_bottom = (new_pos + h) - canvas_h;
        if over_bottom > 0 {
            let start = (h - over_bottom) as usize;
            self.canvas
                .push(arr.rows_slice(start, arr.height), Side::Bottom);
            self.last_added = over_bottom as usize;
        }

        let over_top = -new_pos;
        if over_top > 0 {
            self.canvas
                .push(arr.rows_slice(0, over_top as usize), Side::Top);
            for tracked in self.history.iter_mut() {
                tracked.position += over_top;
            }
            for keyframe in self.keyframes.iter_mut() {
                if let Some(pos) = keyframe.online_position.as_mut() {
                    *pos += over_top;
                }
            }
            if let Some(pending) = self.pending_motion.as_mut()
                && let Some(pos) = pending.online_position.as_mut()
            {
                *pos += over_top;
            }
            self.anchor_pos = 0;
            self.last_added = over_top as usize;
        } else {
            self.anchor_pos = new_pos;
        }
    }

    fn push_history(&mut self, tracked: Tracked) {
        if self.history.len() == HISTORY_LEN {
            self.history.pop_front();
        }
        self.history.push_back(tracked);
    }

    /// Signed relative scroll between two signature sequences and its diff.
    fn find_shift_for(
        &self,
        last: &Cols,
        cols: &Cols,
        predict: i32,
        robust: bool,
        rows: Option<&[bool]>,
    ) -> (i32, f32) {
        find_shift_for(last, cols, predict, robust, &Mask::rows_only(rows))
    }

    /// Feed one frame pair to the fixed-region detector.
    ///
    /// The pair is always "newest tracked frame" against `pixels`, so the
    /// detector sees the same transition regardless of whether the frame was
    /// ultimately accepted. Called from both the accept path and the
    /// "confident but did not move" path; see the comment at the latter for why
    /// the rejected case matters.
    fn observe_fixed_regions(&mut self, pixels: &Arc<Sparse>) {
        if let (Some(detector), Some(newest)) = (self.fixed_regions.as_mut(), self.history.back()) {
            detector.observe(&newest.pixels, pixels);
        }
    }

    fn fixed_row_mask(&self) -> Option<Vec<bool>> {
        let detector = self.fixed_regions.as_ref()?;
        if !detector.ready() {
            return None;
        }
        let mask = detector.row_mask();
        mask.iter().any(|v| *v).then_some(mask)
    }

    fn fixed_column_mask(&self) -> Option<Vec<bool>> {
        let detector = self.fixed_regions.as_ref()?;
        if !detector.ready() {
            return None;
        }
        let mask = detector.column_mask();
        mask.iter().any(|v| *v).then_some(mask)
    }

    // ------------------------------------------------------------------
    // keyframes and offline reconstruction

    fn note_motion(
        &mut self,
        arr: &Rgb8,
        cols: &Arc<Cols>,
        pixels: &Arc<Sparse>,
        sig: &Arc<Vec<u8>>,
        shift: i32,
    ) {
        let direction = if shift > 0 { 1 } else { -1 };
        let turned = self.last_motion_direction != 0 && direction != self.last_motion_direction;

        // A direction change makes the *previous* frame an extremum of the
        // capture, so promote the frame we were still holding.
        if turned && let Some(pending) = self.pending_motion.take() {
            self.remember_keyframe(
                &pending.frame,
                &pending.cols,
                &pending.pixels,
                &pending.signature,
                pending.sequence,
                pending.online_position,
                KeyframeReason::Turn,
            );
        }

        let previous_position = self
            .keyframes
            .iter()
            .rev()
            .find_map(|frame| frame.online_position);
        let interval = (arr.height / 3).max(16) as i64;
        let far_enough = previous_position
            .map(|prev| (self.anchor_pos - prev).abs() >= interval)
            .unwrap_or(true);

        if turned || self.last_recovered || far_enough {
            let reason = if turned {
                KeyframeReason::Turn
            } else if self.last_recovered {
                KeyframeReason::Recovered
            } else {
                KeyframeReason::Motion
            };
            self.remember_keyframe(
                arr,
                cols,
                pixels,
                sig,
                self.sequence,
                Some(self.anchor_pos),
                reason,
            );
        }

        self.pending_motion = if self.offline_disabled {
            None
        } else {
            Some(Pending {
                frame: arr.clone(),
                cols: Arc::clone(cols),
                pixels: Arc::clone(pixels),
                signature: Arc::clone(sig),
                sequence: self.sequence,
                online_position: Some(self.anchor_pos),
            })
        };
        self.last_motion_direction = direction;
        self.failure_run = 0;
    }

    /// Keep a bounded sample of rejected frames: the first of a failure run and
    /// then every fourth, which is enough for the graph to bridge a damaged
    /// stretch without storing every bad frame.
    fn note_failure(
        &mut self,
        arr: &Rgb8,
        cols: &Arc<Cols>,
        pixels: &Arc<Sparse>,
        sig: &Arc<Vec<u8>>,
    ) {
        self.failure_run += 1;
        if self.failure_run != 1 && !self.failure_run.is_multiple_of(4) {
            return;
        }
        self.remember_keyframe(
            arr,
            cols,
            pixels,
            sig,
            self.sequence,
            None,
            KeyframeReason::Failure,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn remember_keyframe(
        &mut self,
        frame: &Rgb8,
        cols: &Cols,
        pixels: &Sparse,
        signature: &[u8],
        sequence: u64,
        online_position: Option<i64>,
        reason: KeyframeReason,
    ) {
        if self.offline_disabled {
            return;
        }
        if self.keyframes.iter().any(|kf| kf.sequence == sequence) {
            return;
        }
        let keyframe = Keyframe {
            data: offline::compress_frame(frame),
            width: frame.width,
            height: frame.height,
            cols: cols.clone(),
            pixels: pixels.clone(),
            signature: signature.to_vec(),
            sequence,
            online_position,
            reason,
        };
        // If two endpoints cannot fit, a bounded global graph is not useful;
        // disable it rather than exceed the documented memory cap.
        if keyframe.memory_used() > self.keyframe_memory_limit / 2 {
            self.disable_offline_rebuild();
            return;
        }
        self.keyframe_memory_used += keyframe.memory_used();
        self.keyframes.push(keyframe);
        self.keyframes.sort_by_key(|kf| kf.sequence);
        if !offline::trim(
            &mut self.keyframes,
            &mut self.keyframe_memory_used,
            self.keyframe_memory_limit,
        ) {
            self.disable_offline_rebuild();
        }
    }

    fn disable_offline_rebuild(&mut self) {
        self.keyframes.clear();
        self.keyframe_memory_used = 0;
        self.pending_motion = None;
        self.offline_disabled = true;
    }

    /// Finish the capture. Flattens the block canvas exactly once, then tries
    /// the offline rebuild and prefers it when it validates a complete path.
    pub fn result(&mut self) -> Result<StitchResult, &'static str> {
        let online = self.canvas.flatten().ok_or("no frames added")?;
        let mut warnings = self.warnings.clone();

        if let Some(pending) = self.pending_motion.take() {
            self.remember_keyframe(
                &pending.frame,
                &pending.cols,
                &pending.pixels,
                &pending.signature,
                pending.sequence,
                pending.online_position,
                KeyframeReason::Tail,
            );
        }

        let rebuilt = self.offline_rebuild();
        if rebuilt.is_none() {
            if self.offline_disabled {
                warnings.push(
                    "offline reconstruction skipped: keyframe memory limit reached; \
                     kept online result"
                        .to_string(),
                );
            } else if self.keyframes.len() >= 2 {
                warnings.push(
                    "offline reconstruction could not validate a complete path; \
                     kept online result"
                        .to_string(),
                );
            }
        }

        Ok(StitchResult {
            frames_used: self.frames_used,
            warnings,
            rebuilt: rebuilt.is_some(),
            image: rebuilt.unwrap_or(online),
        })
    }

    fn offline_rebuild(&self) -> Option<Rgb8> {
        if self.offline_disabled || self.keyframes.len() < 2 {
            return None;
        }
        let row_mask = self.fixed_row_mask();
        let column_mask = self.fixed_column_mask();
        let bands = self
            .fixed_regions
            .as_ref()
            .filter(|detector| detector.ready())
            .map(|detector| detector.bands(self.canvas.width()))
            .unwrap_or_default();
        let ctx = OfflineCtx {
            max_diff: self.max_diff,
            min_shift_px: self.min_shift_px,
            width: self.canvas.width(),
            row_mask: row_mask.as_deref(),
            column_mask: column_mask.as_deref(),
            bands,
        };
        offline::rebuild(&self.keyframes, &ctx)
    }
}

/// Signed relative scroll between two row-signature sequences, plus its diff.
///
/// Both sequences normally have the same height, so the offset is a plain
/// relative scroll distance: positive means the content moved down, negative up.
/// Candidates are probed outward from `predict`, and a near-perfect score exits
/// early, which is what keeps a steady scroll at one or two probes per frame.
///
/// Shared with the offline graph so the live and rebuild paths can never drift
/// apart in how they measure a shift.
pub(crate) fn find_shift_for(
    last: &Cols,
    cols: &Cols,
    predict: i32,
    robust: bool,
    mask: &Mask<'_>,
) -> (i32, f32) {
    let h = last.height;
    let min_overlap = effective_min_overlap(h);
    let max_offset = h.saturating_sub(min_overlap) as i32;
    let score = |offset: i32| {
        if robust {
            robust_col_diff(last, cols, offset, min_overlap, mask)
        } else {
            col_diff(last, cols, offset, min_overlap, mask)
        }
    };

    if max_offset == 0 {
        return (0, score(0));
    }

    let mut best_off = 0;
    let mut best_diff = f32::INFINITY;
    for offset in offset_candidates(max_offset, predict) {
        let d = score(offset);
        if d < best_diff {
            best_diff = d;
            best_off = offset;
            if best_diff < 0.25 {
                break; // essentially perfect
            }
        }
    }
    (best_off, best_diff)
}

/// Row signatures restricted to the non-fixed sparse columns. Falls back to the
/// precomputed signatures when the mask is absent or leaves too little signal.
fn matching_cols(fallback: &Cols, pixels: &Sparse, excluded_columns: Option<&[bool]>) -> Cols {
    let Some(mask) = excluded_columns else {
        return fallback.clone();
    };
    if mask.len() != pixels.columns {
        return fallback.clone();
    }
    let keep: Vec<bool> = mask.iter().map(|v| !*v).collect();
    if keep.iter().filter(|v| **v).count() < 4 {
        return fallback.clone();
    }
    compute_cols(&pixels.select_columns(&keep))
}

/// One-shot helper: stitch a list of frames, warning about low-overlap ones.
pub fn stitch_frames(
    frames: &[Rgb8],
    max_diff: f32,
    min_shift_px: u32,
) -> Result<StitchResult, &'static str> {
    let mut stitcher = Stitcher::new(max_diff, min_shift_px);
    let mut low = 0usize;
    for (index, frame) in frames.iter().enumerate() {
        let diff = stitcher.add(frame);
        if index > 0 && diff > max_diff {
            low += 1;
        }
    }
    if low > 0 {
        stitcher
            .warnings
            .push(format!("{low} frame(s) had low overlap confidence"));
    }
    stitcher.result()
}
