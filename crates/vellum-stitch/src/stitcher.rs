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

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::sync::Arc;

use vellum_core::image::Rgb8;

use crate::canvas::{Canvas, Side};
use crate::fixed_regions::FixedRegionDetector;
use crate::offline::{self, KEYFRAME_MEMORY_LIMIT, Keyframe, KeyframeReason, OfflineCtx};
use crate::scoring::{
    MAX_PIXEL_DIFF, MIN_CHANGED_FRACTION, Mask, ROBUST_MAX_PIXEL_DIFF, col_diff, is_false_motion,
    pixel_change_fraction, pixel_overlap_diff, positioned_col_diff, robust_col_diff,
    robust_pixel_overlap_diff,
};
use crate::signature::{
    Cols, Sparse, compute_cols, content_bottom_ignore, content_top_ignore, effective_min_overlap,
    frame_signature, is_static_view, matching_cols, offset_candidates, sample_pixels, trimmed_mean,
};

/// How many recently accepted frames stay available for re-matching.
const HISTORY_LEN: usize = 6;
/// A full-canvas miss first ranks every position with this small deterministic
/// fingerprint, then pays viewport-height scoring for only the best candidates.
const CANVAS_PROBE_ROWS: usize = 16;
const CANVAS_PROBE_COLUMNS: usize = 6;
const CANVAS_CANDIDATES_PER_GATE: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StitchDecision {
    Seed,
    Stationary,
    Accepted,
    Revisit,
    Reanchored,
    Rejected,
}

impl StitchDecision {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Seed => "seed",
            Self::Stationary => "stationary",
            Self::Accepted => "accepted",
            Self::Revisit => "revisit",
            Self::Reanchored => "reanchored",
            Self::Rejected => "rejected",
        }
    }
}

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
    signature: Arc<Vec<u8>>,
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
    /// The current viewport is byte-identical at the tracked position even
    /// though its shift relative to that older reference is zero.
    exact_revisit: bool,
    robust: bool,
    changed: f32,
    aligned: f32,
}

/// A frame-sized view found wholly inside the accumulated canvas.
#[derive(Debug, Clone, Copy)]
struct CanvasCandidate {
    diff: f32,
    position: i64,
}

struct CanvasSearch<'a> {
    canvas_cols: &'a Cols,
    canvas_probe_pixels: &'a Sparse,
    current_cols: &'a Cols,
    current_pixels: &'a Sparse,
    current_probe_pixels: &'a Sparse,
    mask: Mask<'a>,
    history_match: Option<(f32, f32)>,
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
    /// Machine-readable classification of the last `add`, used by the
    /// privacy-safe recorder trace without exposing frame contents.
    pub last_decision: Option<StitchDecision>,

    history: VecDeque<Tracked>,
    sequence: u64,
    fixed_regions: Option<FixedRegionDetector>,

    keyframes: Vec<Keyframe>,
    keyframe_memory_limit: usize,
    keyframe_memory_used: usize,
    offline_disabled: bool,
    /// A sudden offset discontinuity occurred. The temporal offline graph is
    /// intentionally skipped because its local edges can fold that jump.
    temporal_discontinuity: bool,
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
            last_decision: None,
            history: VecDeque::with_capacity(HISTORY_LEN),
            sequence: 0,
            fixed_regions: None,
            keyframes: Vec::new(),
            keyframe_memory_limit,
            keyframe_memory_used: 0,
            offline_disabled: keyframe_memory_limit == 0,
            temporal_discontinuity: false,
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

        // Cheap early-out: the view has not moved at all. The 18x24 luma
        // signature is only a prefilter: on a translucent terminal it may sample
        // fixed wallpaper between sparse text lines and miss real scrolling.
        // Require the denser 96-column RGB index to be byte-identical before
        // declaring the frame static. The recorder already drops exact full-frame
        // duplicates, so live damage/heartbeat traffic rarely pays this branch.
        let sig = Arc::new(frame_signature(arr));
        let pixels = Arc::new(sample_pixels(arr));
        if let (Some(last_sig), Some(last_pixels)) = (&self.last_signature, &self.last_pixels)
            && is_static_view(last_sig, &sig, last_pixels, &pixels)
        {
            self.last_shift = 0;
            self.last_added = 0;
            self.last_diff = 0.0;
            self.last_recovered = false;
            self.last_decision = Some(StitchDecision::Stationary);
            return 0.0;
        }

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
                    exact_revisit: false,
                    robust,
                    changed: 0.0,
                    aligned: f32::INFINITY,
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

            let candidate_position = tracked.position + i64::from(shift);
            let candidate_motion = candidate_position - self.anchor_pos;
            // A rollback can land exactly on an older tracked viewport. Its
            // reference-relative shift is zero, so the ordinary motion gate
            // calls it static even though its known canvas position differs
            // from the live anchor. Treat only a fully identical sparse view
            // as a spatial revisit; near-duplicates still use both scorers.
            let exact_revisit = shift.unsigned_abs() < self.min_shift_px
                && candidate_motion.unsigned_abs() >= u64::from(self.min_shift_px)
                && is_static_view(&tracked.signature, &sig, &tracked.pixels, &pixels);
            let good = diff <= self.max_diff
                && candidate_motion.unsigned_abs() >= u64::from(self.min_shift_px)
                && (!false_motion || exact_revisit);
            matches.push(Candidate {
                diff,
                shift,
                position: tracked.position,
                history_index: index,
                false_motion,
                exact_revisit,
                robust,
                changed,
                aligned,
            });
            // A near-perfect row signature can still be a different card in a
            // periodic list. Only an exact sparse-RGB overlap is safe to
            // short-circuit; otherwise let the bounded older history expose a
            // better rollback match.
            if good && !robust && aligned == 0.0 {
                break;
            }
        }

        let valid_best = matches
            .iter()
            .filter(|m| {
                let candidate_position = m.position + i64::from(m.shift);
                m.diff <= self.max_diff
                    && candidate_position.abs_diff(self.anchor_pos) >= u64::from(self.min_shift_px)
                    && (!m.false_motion || m.exact_revisit)
            })
            .min_by(|a, b| {
                a.diff
                    .total_cmp(&b.diff)
                    .then_with(|| a.aligned.total_cmp(&b.aligned))
            });

        let (diff, shift, position, recovered, had_valid, changed, aligned) = match valid_best {
            Some(best) => (
                best.diff,
                best.shift,
                best.position,
                best.robust || best.history_index != 0,
                true,
                best.changed,
                best.aligned,
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
                    fallback.changed,
                    fallback.aligned,
                )
            }
        };

        // Recent history is the hot path, but a kinetic-scroll jump can land
        // farther away than all six tracked viewports. A statistically similar
        // edge match may then look valid and duplicate old rows; a complete
        // miss otherwise leaves the UI apparently frozen. On either a miss or
        // a discontinuous offset, consult the compact full-canvas index before
        // accepting/rejecting the history result.
        let new_pos = position + i64::from(shift);
        let motion = clamp_shift(new_pos - self.anchor_pos);
        let discontinuity = motion.abs_diff(self.last_offset)
            >= effective_min_overlap(arr.height)
                .try_into()
                .unwrap_or(u32::MAX);
        if (!had_valid || discontinuity)
            && let Some(known) = self.find_canvas_reanchor(
                &current_match_cols,
                &pixels,
                rows,
                columns,
                had_valid.then_some((diff, aligned)),
            )
        {
            let relative_shift = known.position - self.anchor_pos;
            if relative_shift.unsigned_abs() >= u64::from(self.min_shift_px) {
                return self.accept_canvas_reanchor(
                    arr,
                    &cols,
                    &pixels,
                    &sig,
                    known,
                    relative_shift,
                );
            }
        }

        self.last_shift = motion;
        self.last_added = 0;
        self.last_diff = diff;
        self.last_recovered = recovered;

        if diff > self.max_diff {
            // Low confidence: keep the frame out and do NOT advance the
            // reference, so a later frame can reconnect to history.
            self.note_failure(arr, &cols, &pixels, &sig);
            self.last_decision = Some(StitchDecision::Rejected);
            return diff;
        }
        if motion.unsigned_abs() < self.min_shift_px {
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
            if changed >= MIN_CHANGED_FRACTION {
                self.observe_fixed_regions(&pixels);
            }
            self.last_decision = Some(StitchDecision::Stationary);
            return diff;
        }
        if !had_valid {
            // A low row-signature score can still be rejected by the sparse RGB
            // gate (for example when a large viewport-fixed band favours a wrong
            // non-zero offset). Only a pair with meaningful changed-pixel
            // evidence is allowed to warm the detector; a tiny local animation
            // must not become viewport-fixed chrome.
            if motion.unsigned_abs() >= self.min_shift_px && changed >= MIN_CHANGED_FRACTION {
                self.observe_fixed_regions(&pixels);
            }
            self.last_shift = 0;
            self.last_diff = diff;
            self.last_decision = Some(StitchDecision::Rejected);
            return diff;
        }

        // A learned fixed band can intentionally turn several warm-up frames
        // into one large catch-up shift; the offline graph is specifically what
        // restores that warm-up span. Only an unmasked discontinuity makes the
        // temporal reconstruction unsafe.
        self.temporal_discontinuity |= discontinuity && rows.is_none() && columns.is_none();
        self.extend_canvas(arr, new_pos);

        self.last_cols = Some(Arc::clone(&cols));
        self.last_pixels = Some(Arc::clone(&pixels));
        self.last_signature = Some(Arc::clone(&sig));
        self.last_offset = motion;
        self.frames_used += 1;

        self.observe_fixed_regions(&pixels);
        self.push_history(Tracked {
            cols: Arc::clone(&cols),
            pixels: Arc::clone(&pixels),
            signature: Arc::clone(&sig),
            position: self.anchor_pos,
        });
        self.note_motion(arr, &cols, &pixels, &sig, motion);
        self.last_decision = Some(if self.last_added == 0 {
            StitchDecision::Revisit
        } else {
            StitchDecision::Accepted
        });
        diff
    }

    /// Find an already-captured frame-sized view. The ordinary path is
    /// scanned first; robust scoring is paid only when no ordinary candidate
    /// survives sparse-RGB verification.
    fn find_canvas_reanchor(
        &self,
        current_cols: &Cols,
        current_pixels: &Sparse,
        rows: Option<&[bool]>,
        columns: Option<&[bool]>,
        history_match: Option<(f32, f32)>,
    ) -> Option<CanvasCandidate> {
        let probe_columns = canvas_probe_columns(current_pixels.columns, columns);
        let (canvas_cols, canvas_probe_pixels) =
            self.canvas.matching_snapshot(columns, &probe_columns)?;
        if current_cols.height == 0 || canvas_cols.height < current_cols.height {
            return None;
        }
        let current_probe_pixels = select_sparse_columns(current_pixels, &probe_columns)?;
        let search = CanvasSearch {
            canvas_cols: &canvas_cols,
            canvas_probe_pixels: &canvas_probe_pixels,
            current_cols,
            current_pixels,
            current_probe_pixels: &current_probe_pixels,
            mask: Mask { rows, columns },
            history_match,
        };

        let positions = self.canvas_candidate_positions(&search);
        self.scan_canvas_candidates(&search, &positions, false)
            .or_else(|| self.scan_canvas_candidates(&search, &positions, true))
    }

    /// Rank the whole canvas in O(canvas height) using independent row-signature
    /// and sparse-RGB fingerprints. The exact production scorers below still
    /// decide acceptance; this only avoids evaluating a full viewport at every
    /// possible row when the canvas is very tall.
    fn canvas_candidate_positions(&self, search: &CanvasSearch<'_>) -> Vec<usize> {
        let last_position = search.canvas_cols.height - search.current_cols.height;
        let recent_reach = search
            .current_cols
            .height
            .saturating_sub(effective_min_overlap(search.current_cols.height))
            as u64;
        let probe_rows = canvas_probe_rows(search.current_cols.height, search.mask.rows);
        if probe_rows.is_empty()
            || search.canvas_probe_pixels.columns == 0
            || search.canvas_probe_pixels.columns != search.current_probe_pixels.columns
        {
            return Vec::new();
        }

        let mut ranked = Vec::with_capacity(last_position.saturating_add(1));
        for position in 0..=last_position {
            // The recent sweep already considered every viewport within one
            // valid-overlap radius of a tracked position. Overriding its sparse
            // motion gate there can misread a not-yet-learned fixed footer as a
            // revisit. Full-canvas recovery is only for content genuinely
            // outside the bounded history.
            let covered_by_history = self
                .history
                .iter()
                .any(|tracked| tracked.position.abs_diff(position as i64) <= recent_reach);
            if covered_by_history {
                continue;
            }
            let (row_score, pixel_score) = coarse_canvas_scores(
                search.canvas_cols,
                search.canvas_probe_pixels,
                search.current_cols,
                search.current_probe_pixels,
                position,
                &probe_rows,
            );
            ranked.push(CanvasRank {
                row_score,
                pixel_score,
                position,
            });
        }

        // The two gates are deliberately shortlisted independently. A repeated
        // card can have an excellent row signature but distinct pixels, while a
        // small animation can perturb the sampled pixels but leave row structure
        // useful. The union keeps either source of evidence alive.
        let anchor = self.anchor_pos.max(0) as usize;
        let mut selected = vec![false; ranked.len()];
        mark_best_canvas_ranks(&ranked, &mut selected, anchor, false);
        mark_best_canvas_ranks(&ranked, &mut selected, anchor, true);
        let mut positions: Vec<usize> = ranked
            .iter()
            .zip(selected)
            .filter_map(|(rank, keep)| keep.then_some(rank.position))
            .collect();
        positions.sort_unstable();
        positions
    }

    fn scan_canvas_candidates(
        &self,
        search: &CanvasSearch<'_>,
        positions: &[usize],
        robust: bool,
    ) -> Option<CanvasCandidate> {
        let mut candidates = Vec::new();
        for &position in positions {
            let diff = positioned_col_diff(
                search.canvas_cols,
                search.current_cols,
                position,
                robust,
                &search.mask,
            );
            if diff <= self.max_diff {
                candidates.push((diff, position));
            }
        }

        // Row statistics choose the order; sparse RGB remains the independent
        // acceptance gate, exactly as it is for recent-history matching.
        candidates.sort_unstable_by(|(a_diff, a_pos), (b_diff, b_pos)| {
            a_diff.total_cmp(b_diff).then_with(|| {
                let anchor = self.anchor_pos.max(0) as usize;
                a_pos.abs_diff(anchor).cmp(&b_pos.abs_diff(anchor))
            })
        });
        let pixel_limit = if robust {
            ROBUST_MAX_PIXEL_DIFF
        } else {
            MAX_PIXEL_DIFF
        };
        candidates.into_iter().find_map(|(diff, position)| {
            let canvas_pixels = self
                .canvas
                .matching_pixels_window(position, search.current_pixels.height)?;
            let pixel_diff = if robust {
                robust_pixel_overlap_diff(&canvas_pixels, search.current_pixels, 0, &search.mask)
            } else {
                pixel_overlap_diff(&canvas_pixels, search.current_pixels, 0, &search.mask)
            };
            if pixel_diff > pixel_limit {
                return None;
            }

            // Spatial re-anchoring is allowed to be stricter than ordinary
            // temporal matching, never looser. With no trusted history match,
            // require both independent scores to sit in the better half of
            // their production limits. With a history match, a canvas position
            // may also win by improving both scores.
            let strong = diff <= self.max_diff * 0.5 && pixel_diff <= pixel_limit * 0.5;
            let accepted = search
                .history_match
                .map(|(history_diff, history_pixel)| {
                    // A valid recent-history position is already spatially
                    // coherent. Full-canvas recovery may replace it only with
                    // strictly better evidence on both independent gates; a
                    // merely strong periodic card must not override an exact
                    // rollback match.
                    diff < history_diff && pixel_diff < history_pixel
                })
                .unwrap_or(strong);
            accepted.then_some(CanvasCandidate {
                diff,
                position: position as i64,
            })
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn accept_canvas_reanchor(
        &mut self,
        arr: &Rgb8,
        cols: &Arc<Cols>,
        pixels: &Arc<Sparse>,
        sig: &Arc<Vec<u8>>,
        known: CanvasCandidate,
        relative_shift: i64,
    ) -> f32 {
        let shift = clamp_shift(relative_shift);
        self.anchor_pos = known.position;
        self.last_cols = Some(Arc::clone(cols));
        self.last_pixels = Some(Arc::clone(pixels));
        self.last_signature = Some(Arc::clone(sig));
        // A teleport has no useful velocity prediction. Starting the next
        // search at zero is both cheaper and less likely to favour an edge.
        self.last_offset = 0;
        self.frames_used += 1;
        self.last_shift = shift;
        self.last_added = 0;
        self.last_diff = known.diff;
        self.last_recovered = true;
        self.last_decision = Some(StitchDecision::Reanchored);
        self.temporal_discontinuity = true;

        self.observe_fixed_regions(pixels);
        self.push_history(Tracked {
            cols: Arc::clone(cols),
            pixels: Arc::clone(pixels),
            signature: Arc::clone(sig),
            position: known.position,
        });
        self.note_motion(arr, cols, pixels, sig, shift);
        known.diff
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
        self.last_decision = Some(StitchDecision::Seed);
        self.push_history(Tracked {
            cols: Arc::clone(&cols),
            pixels: Arc::clone(&pixels),
            signature: Arc::clone(&sig),
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
    /// ultimately accepted. Called from the accept path and from both confident
    /// rejection paths (tiny shift or sparse-RGB false motion); otherwise a large
    /// fixed band can prevent the detector from ever completing its warm-up.
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
        if rebuilt.is_none() && !self.temporal_discontinuity {
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
        // The graph is temporal: every edge requires overlapping consecutive
        // content. A full-canvas re-anchor can deliberately jump across a gap
        // with no temporal overlap, so forcing that sequence through the graph
        // can duplicate the revisited span. The online canvas is authoritative
        // after the spatial re-anchor.
        if self.offline_disabled || self.temporal_discontinuity || self.keyframes.len() < 2 {
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

fn clamp_shift(value: i64) -> i32 {
    i32::try_from(value).unwrap_or_else(|_| {
        if value.is_negative() {
            i32::MIN
        } else {
            i32::MAX
        }
    })
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

#[derive(Clone, Copy)]
struct CanvasRank {
    row_score: f32,
    pixel_score: f32,
    position: usize,
}

fn canvas_probe_rows(height: usize, excluded: Option<&[bool]>) -> Vec<usize> {
    let top = content_top_ignore(height);
    let end = height.saturating_sub(content_bottom_ignore(height));
    let usable_mask = excluded.filter(|mask| mask.len() == height);
    let active: Vec<usize> = (top..end)
        .filter(|row| !usable_mask.is_some_and(|mask| mask[*row]))
        .collect();
    evenly_spaced(&active, CANVAS_PROBE_ROWS)
}

fn canvas_probe_columns(columns: usize, excluded: Option<&[bool]>) -> Vec<usize> {
    let usable_mask = excluded
        .filter(|mask| mask.len() == columns && mask.iter().filter(|value| !**value).count() >= 4);
    let active: Vec<usize> = (0..columns)
        .filter(|column| !usable_mask.is_some_and(|mask| mask[*column]))
        .collect();
    evenly_spaced(&active, CANVAS_PROBE_COLUMNS)
}

fn evenly_spaced(values: &[usize], limit: usize) -> Vec<usize> {
    let count = values.len().min(limit);
    match count {
        0 => Vec::new(),
        1 => vec![values[0]],
        _ => (0..count)
            .map(|index| values[index * (values.len() - 1) / (count - 1)])
            .collect(),
    }
}

fn coarse_canvas_scores(
    canvas_cols: &Cols,
    canvas_pixels: &Sparse,
    current_cols: &Cols,
    current_pixels: &Sparse,
    position: usize,
    rows: &[usize],
) -> (f32, f32) {
    debug_assert!(rows.len() <= CANVAS_PROBE_ROWS);
    let mut row_scores = [0.0; CANVAS_PROBE_ROWS];
    let mut pixel_scores = [0.0; CANVAS_PROBE_ROWS];
    for (score_index, &row_index) in rows.iter().enumerate() {
        let canvas_row = canvas_cols.row(position + row_index);
        let current_row = current_cols.row(row_index);
        row_scores[score_index] = ((canvas_row[0] - current_row[0]).abs()
            + (canvas_row[1] - current_row[1]).abs()
            + (canvas_row[2] - current_row[2]).abs())
            / 3.0;

        let canvas_row = canvas_pixels.row(position + row_index);
        let current_row = current_pixels.row(row_index);
        let mut total = 0u32;
        for (canvas, current) in canvas_row.iter().zip(current_row) {
            total += u32::from(canvas.abs_diff(*current));
        }
        pixel_scores[score_index] = total as f32 / canvas_row.len() as f32;
    }
    (
        trimmed_mean(&mut row_scores[..rows.len()]),
        trimmed_mean(&mut pixel_scores[..rows.len()]),
    )
}

fn mark_best_canvas_ranks(
    ranked: &[CanvasRank],
    selected: &mut [bool],
    anchor: usize,
    pixel_gate: bool,
) {
    let mut order: Vec<usize> = (0..ranked.len()).collect();
    let compare = |a: &usize, b: &usize| -> Ordering {
        let a = ranked[*a];
        let b = ranked[*b];
        let score_order = if pixel_gate {
            a.pixel_score.total_cmp(&b.pixel_score)
        } else {
            a.row_score.total_cmp(&b.row_score)
        };
        score_order
            .then_with(|| {
                a.position
                    .abs_diff(anchor)
                    .cmp(&b.position.abs_diff(anchor))
            })
            .then_with(|| a.position.cmp(&b.position))
    };
    let keep = order.len().min(CANVAS_CANDIDATES_PER_GATE);
    if keep > 0 && keep < order.len() {
        order.select_nth_unstable_by(keep - 1, compare);
        order.truncate(keep);
    }
    for index in order {
        selected[index] = true;
    }
}

fn select_sparse_columns(pixels: &Sparse, columns: &[usize]) -> Option<Sparse> {
    if columns.is_empty() || columns.iter().any(|column| *column >= pixels.columns) {
        return None;
    }
    let mut keep = vec![false; pixels.columns];
    for column in columns {
        keep[*column] = true;
    }
    Some(pixels.select_columns(&keep))
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

#[cfg(test)]
mod candidate_tests {
    use super::*;

    fn patterned_page(width: usize, height: usize) -> Rgb8 {
        let mut image = Rgb8::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let index = (y * width + x) * 3;
                image.data[index] = ((y * 19 + x * 3) % 251) as u8;
                image.data[index + 1] = ((y * 7 + x * 23) % 253) as u8;
                image.data[index + 2] = ((y * 29 + x * 11) % 247) as u8;
            }
        }
        image
    }

    #[test]
    fn last_decision_distinguishes_seed_stationary_and_growth() {
        let page = patterned_page(96, 120);
        let first = page.rows_slice(0, 80);
        let second = page.rows_slice(12, 92);
        let mut stitcher = Stitcher::new(9.0, 4);

        stitcher.add(&first);
        assert_eq!(stitcher.last_decision, Some(StitchDecision::Seed));

        stitcher.add(&first);
        assert_eq!(stitcher.last_decision, Some(StitchDecision::Stationary));

        stitcher.add(&second);
        assert_eq!(stitcher.last_decision, Some(StitchDecision::Accepted));
        assert_eq!(stitcher.last_added, 12);
    }

    #[test]
    fn canvas_shortlist_preserves_both_independent_gates() {
        let target = 17usize;
        let anchor = 1_999usize;
        let ranked: Vec<CanvasRank> = (0..2_000)
            .map(|position| CanvasRank {
                // Every row signature ties, so the row shortlist stays near the
                // anchor and deliberately cannot contain the distant target.
                row_score: 0.0,
                pixel_score: if position == target { 0.0 } else { 100.0 },
                position,
            })
            .collect();
        let mut selected = vec![false; ranked.len()];
        mark_best_canvas_ranks(&ranked, &mut selected, anchor, false);
        assert!(
            !selected[target],
            "fixture did not escape the row shortlist"
        );

        mark_best_canvas_ranks(&ranked, &mut selected, anchor, true);
        assert!(
            selected[target],
            "the independent sparse-RGB gate failed to rescue the true position"
        );
        assert!(
            selected.iter().filter(|keep| **keep).count() <= CANVAS_CANDIDATES_PER_GATE * 2,
            "the shortlist exceeded its documented bound"
        );
    }
}
