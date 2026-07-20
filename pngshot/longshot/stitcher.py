"""Vertical screenshot stitching.

Given a sequence of frames captured from the *same* screen region while the
user scrolls a window vertically, reconstruct one tall image.

Algorithm (row-signature matching, ported from the approach used by
wl-longshot). The previous implementation used ``cv2.matchTemplate`` on the
raw pixels, whose TM_CCOEFF_NORMED score sat around 0.3 on real text/UI
content (sub-pixel font rendering + anti-aliasing wreck 2-D template matching),
so every frame after the first was rejected. Instead we now:

  1. Reduce each *row* of a frame to a tiny 3-number signature
     (mean brightness, contrast, edge energy) -> a frame becomes an [H, 3]
     one-dimensional sequence. This aggregation smooths away the sub-pixel
     noise that broke template matching.
  2. Match the *previous appended frame* against the *current* frame by sliding
     one signature sequence over the other and taking the mean absolute
     difference over the overlap (``_col_diff``). The best (lowest-diff) offset
     is how far the content scrolled.
  3. Ignore a slice of the top and bottom of the overlap (``_content_*_ignore``)
     so scroll inertia / fade-in rows don't poison the score.
  4. Search offsets outward from the previous offset (``_offset_candidates``)
     with an early exit, so a steady scroll usually matches on the first try.
  5. A per-frame down-sampled signature (``_frame_signature``) skips matching
     entirely when the view hasn't moved at all.
  6. If the fast whole-overlap score fails, retry with a trimmed row score that
     ignores a bounded set of locally-changing rows. This handles videos,
     loading indicators, carets, and animated page sections without relaxing
     the normal acceptance threshold for the whole frame.
  7. Keep a bounded set of lossless compressed keyframes. At the end of a
     capture, build a small temporal match graph and rebuild the canvas from a
     complete high-confidence path, skipping damaged bridge frames when
     possible. If the path cannot be validated, the online canvas is retained.
  8. Learn viewport-fixed edge bands from at least three accepted scrolling
     transitions. Exclude those rows and columns while matching, then retain a
     fixed header, footer, or sidebar only once in the offline result.
  9. Feather aligned offline overlaps with viewport-centred weights so a
     screen-fixed wallpaper behind a translucent window does not switch at
     hard frame boundaries. Large local pixel changes keep one frame state to
     avoid animation ghosts.

A frame is appended when the best overlap diff is <= ``max_diff`` (lower is
better) and it contributes at least ``min_shift_px`` new rows. A diff above
``max_diff`` means the frames don't overlap confidently. The recorder keeps
collecting and retries against recent history before showing a slow-down hint.

Constraints (documented for the user):
  - Vertical scroll only. Horizontal movement breaks matching.
  - Animated content between frames raises the diff and may be rejected.
"""
from __future__ import annotations

from collections import deque
from collections.abc import Iterator
from dataclasses import dataclass, field
import zlib

import numpy as np
from PIL import Image

from .fixed_regions import FixedBands, FixedRegionDetector

# Below this overlap diff a frame is considered a confident match. This is a
# mean per-channel absolute difference of the row signatures, so it is on the
# same scale as 8-bit brightness units; ~9 matches wl-longshot's threshold.
DEFAULT_MAX_DIFF = 9.0
# Even a low row-signature score is insufficient on its own: photos and noisy
# textures collapse to similar averages. Genuine overlapping screen pixels are
# much closer than unrelated content, so enforce an absolute sparse-RGB check.
MAX_PIXEL_DIFF = 32.0
# The robust fallback has intentionally discarded its noisiest rows, so require
# the remaining sparse real pixels to agree absolutely as well. Unrelated noisy
# images can share similar row statistics by chance, but their RGB MAD is near
# 85; genuine screen overlap is normally in the low single digits.
ROBUST_MAX_PIXEL_DIFF = 24.0
# During offline fusion, pixels that differ by more than this amount are
# usually a local animation/caret or an opaque foreground change rather than
# the translucent background we are trying to smooth. Keep one complete frame
# state for those pixels instead of averaging an obvious ghost.
FUSION_MAX_PIXEL_DELTA = 48
# Offline reconstruction retains losslessly-compressed full-resolution
# keyframes. The cap excludes one pending raw accepted frame (at most one
# selection-sized RGB image), which is needed to preserve a direction-change
# extremum without compressing every capture on the live path.
KEYFRAME_MEMORY_LIMIT = 48 * 1024 * 1024
KEYFRAME_MAX_COUNT = 160
OFFLINE_EDGE_LOOKBACK = 8


@dataclass
class StitchResult:
    image: Image.Image
    frames_used: int
    warnings: list[str] = field(default_factory=list)
    rebuilt: bool = False


@dataclass
class _TrackedFrame:
    """Small matching state for a recently accepted frame.

    Keeping a handful of these costs only the row signatures plus sparse pixel
    samples, but lets us recover when the newest frame is animated or its
    signature lands on a bad periodic match.  ``position`` is the frame top in
    canvas coordinates.
    """

    cols: np.ndarray
    pixels: np.ndarray
    signature: np.ndarray
    position: int


@dataclass
class _FrameCandidate:
    arr: np.ndarray
    cols: np.ndarray
    pixels: np.ndarray
    signature: np.ndarray
    sequence: int
    online_position: int | None


@dataclass
class _Keyframe:
    data: bytes
    shape: tuple[int, int, int]
    cols: np.ndarray
    pixels: np.ndarray
    signature: np.ndarray
    sequence: int
    online_position: int | None
    reason: str

    @property
    def memory_used(self) -> int:
        return (
            len(self.data)
            + self.cols.nbytes
            + self.pixels.nbytes
            + self.signature.nbytes
        )


@dataclass(frozen=True)
class _GraphEdge:
    shift: int
    diff: float
    robust: bool
    cost: float


class Stitcher:
    def __init__(self, max_diff: float = DEFAULT_MAX_DIFF,
                 min_shift_px: int = 4, *, preview: bool = True,
                 keyframe_memory_limit: int = KEYFRAME_MEMORY_LIMIT) -> None:
        self.max_diff = max_diff
        self.min_shift_px = min_shift_px
        self._preview_enabled = preview

        # Canvas storage as an ordered list of row-blocks (top -> bottom) rather
        # than one big array. Appending/prepending a block is O(1); we only pay
        # for a single full-height copy once, in ``result()``. The old code did
        # ``np.vstack([canvas, new])`` every frame, an O(canvas height) copy that
        # turned a long scroll quadratic and was a major cause of the "slower the
        # longer you scroll" stutter.
        self._blocks: deque[np.ndarray] = deque()  # RGB blocks, top -> bottom
        self._height: int = 0                    # total canvas rows = sum block heights
        self._width: int = 0
        # Incremental preview thumbnail, also a block list. Each canvas block is
        # scaled *once* to preview width when it is added (~2 ms, independent of
        # total length) and cached; the live preview just concatenates the few
        # tail blocks needed to fill the view window. Rebuilding the thumbnail
        # from the whole canvas every frame was O(canvas height) and dominated
        # per-frame cost (~70 ms at 24k px), which is what made long scrolls
        # stutter and then drop frames (-> "重叠不足").
        self._thumb_blocks: deque[np.ndarray] = deque()  # scaled RGB blocks
        self._thumb_side: str = "bottom"         # where the last content landed
        self._preview_w = 220                    # thumbnail width in px
        # Row signature of the last *tracked* frame; matching is always the
        # previous frame vs the incoming one so the two sequences are the same
        # height and the offset is a simple relative scroll distance (which may
        # be negative when the user scrolls up).
        self._last_cols: np.ndarray | None = None
        # Sparse real pixels from the same tracked frame. Row signatures are
        # excellent for finding offsets, but repeated cards/list rows can make
        # a wrong periodic offset look perfect. A final pixel-level comparison
        # against the zero-offset view rejects that stationary-animation trap.
        self._last_pixels: np.ndarray | None = None
        self._last_offset = 0
        self._last_signature: np.ndarray | None = None
        # Where the *last tracked frame's top* sits within the canvas, in canvas
        # rows. The canvas grows both ways: scrolling down appends to the bottom
        # (anchor unchanged), scrolling up prepends to the top (every anchor
        # shifts down by the prepended height). This is what lets the user start
        # mid-page and capture upward first, then downward — the seed frame just
        # lives somewhere in the middle of the final image.
        self._anchor_pos = 0
        self.frames_used = 0
        self.warnings: list[str] = []
        # diagnostics from the most recent add(): lets the recorder tell apart
        # "content didn't move" (small shift) from "scrolled too fast" (high
        # diff), which need opposite advice to the user. last_shift is the signed
        # relative scroll (+down / -up); last_added is how many new rows the
        # frame actually contributed (0 when re-traversing already-captured area).
        self.last_shift = 0
        self.last_added = 0
        self.last_diff = 0.0
        self.last_recovered = False
        self._history: deque[_TrackedFrame] = deque(maxlen=6)
        self._sequence = 0
        self._keyframe_memory_limit = max(0, keyframe_memory_limit)
        self._keyframes: list[_Keyframe] = []
        self._keyframe_memory_used = 0
        self._offline_disabled = self._keyframe_memory_limit == 0
        self._pending_motion: _FrameCandidate | None = None
        self._last_motion_direction = 0
        self._failure_run = 0
        self._fixed_regions: FixedRegionDetector | None = None

    # ------------------------------------------------------------------

    def add(self, frame: Image.Image) -> float:
        """Add a frame. Returns the overlap diff (LOWER is better; 0 = first).

        A return value > ``max_diff`` means the frame was NOT appended
        (low-confidence overlap); a later frame may reconnect to recent history.
        A confident match that simply didn't move far enough (< min_shift_px)
        is also not appended but returns a low diff.
        """
        self._sequence += 1
        arr = _to_rgb_array(frame)

        if not self._blocks:
            self._width = arr.shape[1]
            self._append_block(arr, side="bottom")
            cols = _compute_cols(arr)
            pixels = _sample_pixels(arr)
            signature = _frame_signature(arr)
            self._fixed_regions = FixedRegionDetector(
                arr.shape[0], pixels.shape[1]
            )
            self._last_cols = cols
            self._last_pixels = pixels
            self._last_signature = signature
            self._last_offset = 0
            self._anchor_pos = 0
            self.frames_used = 1
            self.last_shift = 0
            self.last_added = arr.shape[0]
            self.last_diff = 0.0
            self.last_recovered = False
            self._history.append(_TrackedFrame(cols, pixels, signature, 0))
            seed = _FrameCandidate(
                arr, cols, pixels, signature, self._sequence, 0
            )
            self._remember_keyframe(seed, reason="seed")
            if not self._offline_disabled:
                self._pending_motion = self._copy_candidate(seed)
            return 0.0

        # Width must match; if a resize slipped in, letterbox/crop to canvas.
        if arr.shape[1] != self._width:
            arr = _fit_width(arr, self._width)

        # Cheap early-out: if the frame is a near-duplicate of the last tracked
        # one, the view hasn't moved — skip the (relatively) costly matching.
        sig = _frame_signature(arr)
        if self._last_signature is not None and _is_duplicate(self._last_signature, sig):
            self.last_shift = 0
            self.last_added = 0
            self.last_diff = 0.0
            return 0.0

        cols = _compute_cols(arr)
        pixels = _sample_pixels(arr)

        # Normally the newest frame is enough. If it is briefly corrupted by
        # animation, a sticky header, or a periodic list pattern, retry against
        # a few recent accepted frames before declaring low overlap. This is
        # deliberately bounded so the hot path stays cheap.
        matches: list[tuple[float, int, _TrackedFrame, bool, bool]] = []
        history = list(reversed(self._history))
        excluded_rows = self._fixed_row_mask()
        excluded_columns = self._fixed_column_mask()
        current_match_cols = self._matching_cols(cols, pixels, excluded_columns)
        for index, tracked in enumerate(history):
            predict = self._last_offset if index == 0 else 0
            tracked_match_cols = self._matching_cols(
                tracked.cols, tracked.pixels, excluded_columns
            )
            shift, diff = self._find_shift_for(
                tracked_match_cols,
                current_match_cols,
                predict,
                excluded_rows=excluded_rows,
            )
            robust = False
            if diff > self.max_diff:
                robust_shift, robust_diff = self._find_shift_for(
                    tracked_match_cols,
                    current_match_cols,
                    predict,
                    robust=True,
                    excluded_rows=excluded_rows,
                )
                if robust_diff < diff:
                    shift, diff = robust_shift, robust_diff
                    robust = robust_diff <= self.max_diff
            if diff > self.max_diff and index == 0 and len(history) == 1:
                matches.append((diff, shift, tracked, False, robust))
                continue
            changed = _pixel_change_fraction(
                tracked.pixels,
                pixels,
                excluded_rows=excluded_rows,
                excluded_columns=excluded_columns,
            )
            pixel_diff_fn = _robust_pixel_overlap_diff if robust else _pixel_overlap_diff
            aligned = pixel_diff_fn(
                tracked.pixels,
                pixels,
                shift,
                excluded_rows=excluded_rows,
                excluded_columns=excluded_columns,
            )
            stationary = pixel_diff_fn(
                tracked.pixels,
                pixels,
                0,
                excluded_rows=excluded_rows,
                excluded_columns=excluded_columns,
            )
            false_motion = (
                changed < 0.012
                or stationary <= aligned + 0.2
                or aligned > (
                    ROBUST_MAX_PIXEL_DIFF if robust else MAX_PIXEL_DIFF
                )
            )
            matches.append((diff, shift, tracked, false_motion, robust))
            # A confident, meaningful match on the newest frame is the common
            # path. Older frames are only consulted for low confidence or a
            # sub-threshold shift.
            if diff <= self.max_diff and abs(shift) >= self.min_shift_px and not false_motion:
                break

        valid = [m for m in matches
                 if m[0] <= self.max_diff
                 and abs(m[1]) >= self.min_shift_px
                 and not m[3]]
        if valid:
            diff, shift, tracked, _, robust = min(valid, key=lambda m: m[0])
            recovered = robust or (tracked is not history[0] if history else False)
        else:
            diff, shift, tracked, _, _ = min(matches, key=lambda m: m[0])
            recovered = False
        self.last_shift = shift
        self.last_added = 0
        self.last_diff = diff
        self.last_recovered = recovered

        if diff > self.max_diff:
            # low-confidence overlap: keep the frame out of the canvas, but do
            # not advance the matching reference. The recorder can continue
            # collecting and a later frame may reconnect to history.
            self._note_failure(arr, cols, pixels, sig)
            return diff
        if abs(shift) < self.min_shift_px:
            # confident match but essentially the same view — nothing new.
            # Deliberately do NOT refresh _last_cols so several sub-threshold
            # scrolls accumulate against a fixed reference and eventually append.
            return diff

        # All valid candidates have already passed the sparse pixel check.
        if not valid:
            self.last_shift = 0
            self.last_diff = diff
            return diff

        # `shift` is signed: +down / -up. Convert to the incoming frame's
        # position within the canvas and grow whichever edge it overhangs.
        new_pos = tracked.position + shift
        self._extend_canvas(arr, new_pos)

        self._last_cols = cols
        self._last_pixels = pixels
        self._last_signature = sig
        self._last_offset = shift
        self.frames_used += 1
        if self._fixed_regions is not None and self._history:
            self._fixed_regions.observe(self._history[-1].pixels, pixels)
        self._history.append(
            _TrackedFrame(cols, pixels, sig, self._anchor_pos)
        )
        self._note_motion(arr, cols, pixels, sig, shift)
        return diff

    def _extend_canvas(self, arr: np.ndarray, new_pos: int) -> None:
        """Blit the frame at ``new_pos`` (top row, canvas coords), growing edges.

        Downward scroll (new_pos + h beyond the bottom) appends new rows below;
        upward scroll (new_pos < 0) prepends above and pushes every anchor down.
        Re-traversing already-captured rows adds nothing (last_added stays 0).

        Both grows are O(rows added) — a block is just appended to / inserted at
        the front of ``self._blocks`` — so a long scroll stays linear overall
        instead of the quadratic ``vstack`` the old single-array canvas paid.
        """
        h = arr.shape[0]
        canvas_h = self._height

        over_bottom = (new_pos + h) - canvas_h
        if over_bottom > 0:
            # bottom `over_bottom` rows of the frame are genuinely new
            self._append_block(arr[h - over_bottom:, :, :], side="bottom")
            self.last_added = over_bottom

        over_top = -new_pos
        if over_top > 0:
            # top `over_top` rows of the frame are new; prepend and re-anchor
            self._append_block(arr[:over_top, :, :], side="top")
            for tracked in self._history:
                tracked.position += over_top
            for keyframe in self._keyframes:
                if keyframe.online_position is not None:
                    keyframe.online_position += over_top
            if (self._pending_motion is not None
                    and self._pending_motion.online_position is not None):
                self._pending_motion.online_position += over_top
            self._anchor_pos = 0
            self.last_added = over_top
        else:
            self._anchor_pos = new_pos

    # ------------------------------------------------------------------
    # canvas block storage + incremental thumbnail

    def _append_block(self, block: np.ndarray, *, side: str) -> None:
        """Add a contiguous RGB row-block to the top or bottom of the canvas.

        Also folds the block into the live preview thumbnail incrementally so
        the recorder never has to re-scale the whole canvas.
        """
        block = np.ascontiguousarray(block)
        if side == "bottom":
            self._blocks.append(block)
        else:  # top
            self._blocks.appendleft(block)
        self._height += block.shape[0]
        if self._preview_enabled:
            self._thumb_add_block(block, side=side)

    def _thumb_add_block(self, block: np.ndarray, *, side: str) -> None:
        """Scale just this block once (~2 ms) and cache it as a preview block.

        Each canvas block becomes one down-scaled thumbnail block, stored in the
        same top->bottom order. Scaling touches only the *new* rows, so cost is
        independent of total canvas height. ``preview_thumbnail`` later stitches
        just the tail blocks it needs, so the whole preview path is O(1) in the
        canvas length rather than the old O(height) full re-scale.
        """
        tw = min(self._width, self._preview_w)
        tw = max(1, tw)
        scale = self._width / tw
        bh = block.shape[0]
        th = max(1, int(round(bh / scale)))
        small = np.asarray(
            Image.fromarray(block, mode="RGB").resize((tw, th), Image.BILINEAR),
            dtype=np.uint8,
        )
        if side == "bottom":
            self._thumb_blocks.append(small)
        else:  # top
            self._thumb_blocks.appendleft(small)
        self._thumb_side = side

    # ------------------------------------------------------------------

    def _find_shift(self, cols: np.ndarray) -> tuple[int, float]:
        """Signed relative scroll between the last tracked frame and this one.

        Both signature sequences have the same height ``h``. A positive shift
        ``s`` means the content scrolled *down* by ``s`` rows (``last_cols[s:]``
        lines up with ``cols[:h-s]``); a negative shift means it scrolled *up*.
        We search both directions so the user can start anywhere and scroll up
        or down; the sign tells ``add()`` which canvas edge to grow.
        """
        last = self._last_cols
        assert last is not None
        return self._find_shift_for(last, cols, self._last_offset)

    def _find_shift_for(self, last: np.ndarray, cols: np.ndarray,
                        predict: int, *, robust: bool = False,
                        excluded_rows: np.ndarray | None = None) -> tuple[int, float]:
        h = len(last)
        min_overlap = _effective_min_overlap(h)
        max_offset = max(h - min_overlap, 0)
        diff_fn = _robust_col_diff if robust else _col_diff
        if max_offset == 0:
            return 0, float(diff_fn(
                last, cols, 0, min_overlap, excluded_rows=excluded_rows
            ))

        best_off, best_diff = 0, float("inf")
        for off in _offset_candidates(max_offset, predict):
            d = diff_fn(
                last, cols, off, min_overlap,
                excluded_rows=excluded_rows,
            )
            if d < best_diff:
                best_diff, best_off = d, off
                if best_diff < 0.25:  # essentially perfect, stop early
                    break
        return best_off, best_diff

    def _fixed_row_mask(self) -> np.ndarray | None:
        if self._fixed_regions is None or not self._fixed_regions.ready:
            return None
        mask = self._fixed_regions.row_mask()
        return mask if bool(mask.any()) else None

    def _fixed_column_mask(self) -> np.ndarray | None:
        if self._fixed_regions is None or not self._fixed_regions.ready:
            return None
        mask = self._fixed_regions.column_mask()
        return mask if bool(mask.any()) else None

    @staticmethod
    def _matching_cols(fallback: np.ndarray, pixels: np.ndarray,
                       excluded_columns: np.ndarray | None) -> np.ndarray:
        if excluded_columns is None or excluded_columns.shape != (pixels.shape[1],):
            return fallback
        active = ~excluded_columns
        if int(active.sum()) < 4:
            return fallback
        return _compute_cols_from_pixels(pixels[:, active, :])

    # ------------------------------------------------------------------

    @property
    def keyframe_memory_used(self) -> int:
        return self._keyframe_memory_used

    def result(self) -> StitchResult:
        if not self._blocks:
            raise ValueError("no frames added")
        # The single full-height copy we deliberately deferred from every add().
        canvas = self._blocks[0] if len(self._blocks) == 1 else np.vstack(self._blocks)
        warnings = list(self.warnings)
        self._flush_pending_motion()
        rebuilt = self._offline_rebuild()
        if rebuilt is not None:
            canvas = rebuilt
        elif self._offline_disabled:
            warnings.append(
                "offline reconstruction skipped: keyframe memory limit reached; "
                "kept online result"
            )
        elif len(self._keyframes) >= 2:
            warnings.append(
                "offline reconstruction could not validate a complete path; "
                "kept online result"
            )
        img = Image.fromarray(canvas, mode="RGB").convert("RGBA")
        return StitchResult(
            image=img,
            frames_used=self.frames_used,
            warnings=warnings,
            rebuilt=rebuilt is not None,
        )

    # ------------------------------------------------------------------
    # bounded keyframes + end-of-capture global reconstruction

    def _copy_candidate(self, candidate: _FrameCandidate) -> _FrameCandidate:
        return _FrameCandidate(
            np.ascontiguousarray(candidate.arr).copy(),
            candidate.cols,
            candidate.pixels,
            candidate.signature,
            candidate.sequence,
            candidate.online_position,
        )

    def _note_motion(self, arr: np.ndarray, cols: np.ndarray,
                     pixels: np.ndarray, signature: np.ndarray,
                     shift: int) -> None:
        direction = 1 if shift > 0 else -1
        turned = (
            self._last_motion_direction != 0
            and direction != self._last_motion_direction
        )
        if turned and self._pending_motion is not None:
            self._remember_keyframe(self._pending_motion, reason="turn")

        candidate = _FrameCandidate(
            arr, cols, pixels, signature, self._sequence, self._anchor_pos
        )
        positioned = [
            keyframe for keyframe in reversed(self._keyframes)
            if keyframe.online_position is not None
        ]
        previous_position = (
            positioned[0].online_position if positioned else None
        )
        interval = max(16, arr.shape[0] // 3)
        far_enough = (
            previous_position is None
            or abs(self._anchor_pos - previous_position) >= interval
        )
        if turned or self.last_recovered or far_enough:
            reason = "turn" if turned else (
                "recovered" if self.last_recovered else "motion"
            )
            self._remember_keyframe(candidate, reason=reason)

        self._pending_motion = (
            None if self._offline_disabled else self._copy_candidate(candidate)
        )
        self._last_motion_direction = direction
        self._failure_run = 0

    def _note_failure(self, arr: np.ndarray, cols: np.ndarray,
                      pixels: np.ndarray, signature: np.ndarray) -> None:
        self._failure_run += 1
        if self._failure_run != 1 and self._failure_run % 4 != 0:
            return
        candidate = _FrameCandidate(
            arr, cols, pixels, signature, self._sequence, None
        )
        self._remember_keyframe(candidate, reason="failure")

    def _flush_pending_motion(self) -> None:
        pending = self._pending_motion
        self._pending_motion = None
        if pending is not None:
            self._remember_keyframe(pending, reason="tail")

    def _remember_keyframe(self, candidate: _FrameCandidate, *, reason: str) -> None:
        if self._offline_disabled:
            return
        if any(frame.sequence == candidate.sequence for frame in self._keyframes):
            return
        contiguous = np.ascontiguousarray(candidate.arr, dtype=np.uint8)
        payload = zlib.compress(contiguous.tobytes(), level=1)
        keyframe = _Keyframe(
            payload,
            contiguous.shape,
            candidate.cols,
            candidate.pixels,
            candidate.signature,
            candidate.sequence,
            candidate.online_position,
            reason,
        )
        # If two endpoints cannot fit, there is no useful bounded global graph.
        # Disable it instead of silently exceeding the documented memory cap.
        if keyframe.memory_used > self._keyframe_memory_limit // 2:
            self._disable_offline_rebuild()
            return
        self._keyframes.append(keyframe)
        self._keyframes.sort(key=lambda frame: frame.sequence)
        self._keyframe_memory_used += keyframe.memory_used
        self._trim_keyframes()

    def _trim_keyframes(self) -> None:
        while (
            self._keyframe_memory_used > self._keyframe_memory_limit
            or len(self._keyframes) > KEYFRAME_MAX_COUNT
        ):
            if len(self._keyframes) <= 2:
                self._disable_offline_rebuild()
                return
            candidates = list(range(1, len(self._keyframes) - 1))
            normal = [
                index for index in candidates
                if self._keyframes[index].reason in {"motion", "tail"}
            ]
            pool = normal or candidates

            def removal_cost(index: int) -> tuple[int, int]:
                frame = self._keyframes[index]
                priority = {
                    "motion": 0,
                    "tail": 1,
                    "recovered": 2,
                    "turn": 3,
                    "failure": 4,
                }.get(frame.reason, 2)
                span = (
                    self._keyframes[index + 1].sequence
                    - self._keyframes[index - 1].sequence
                )
                return priority, span

            remove_at = min(pool, key=removal_cost)
            removed = self._keyframes.pop(remove_at)
            self._keyframe_memory_used -= removed.memory_used

    def _disable_offline_rebuild(self) -> None:
        self._keyframes.clear()
        self._keyframe_memory_used = 0
        self._pending_motion = None
        self._offline_disabled = True

    def _offline_rebuild(self) -> np.ndarray | None:
        frames = self._keyframes
        if self._offline_disabled or len(frames) < 2:
            return None

        count = len(frames)
        scores = [float("inf")] * count
        parents: list[tuple[int, _GraphEdge] | None] = [None] * count
        scores[0] = 0.0
        skip_penalty = self.max_diff * 1.5

        for current in range(1, count):
            edge_cache: dict[int, _GraphEdge | None] = {}

            def edge_for(previous: int) -> _GraphEdge | None:
                if previous not in edge_cache:
                    edge_cache[previous] = self._offline_edge(
                        frames[previous], frames[current]
                    )
                return edge_cache[previous]

            previous_indices = [current - 1]
            adjacent = edge_for(current - 1)
            # The adjacent path is the common case. Probe an older node only
            # when animation/low confidence makes that edge questionable, plus
            # one periodic probe to catch a locally attractive wrong offset.
            if adjacent is None or adjacent.robust or adjacent.diff > self.max_diff * 0.65:
                previous_indices.extend(
                    range(max(0, current - OFFLINE_EDGE_LOOKBACK), current - 1)
                )
            elif current % 4 == 0 and current > 4:
                previous_indices.append(current - 4)

            for previous in dict.fromkeys(previous_indices):
                if not np.isfinite(scores[previous]):
                    continue
                edge = edge_for(previous)
                if edge is None:
                    continue
                skipped = current - previous - 1
                candidate_score = (
                    scores[previous] + edge.cost + skipped * skip_penalty
                )
                if candidate_score < scores[current]:
                    scores[current] = candidate_score
                    parents[current] = (previous, edge)

        if parents[-1] is None:
            return None

        cursor = count - 1
        position = 0
        reverse_positions: list[tuple[int, int]] = [(cursor, position)]
        while cursor != 0:
            parent = parents[cursor]
            if parent is None:
                return None
            previous, edge = parent
            position -= edge.shift
            reverse_positions.append((previous, position))
            cursor = previous
        path = list(reversed(reverse_positions))

        decoded: list[tuple[int, np.ndarray]] = []
        for index, position in path:
            frame = frames[index]
            try:
                raw = zlib.decompress(frame.data)
                arr = np.frombuffer(raw, dtype=np.uint8).reshape(frame.shape)
            except (ValueError, zlib.error):
                return None
            if arr.shape[1] != self._width:
                return None
            decoded.append((position, arr))

        bands = FixedBands()
        if self._fixed_regions is not None and self._fixed_regions.ready:
            bands = self._fixed_regions.bands(self._width)
        frame_height = decoded[0][1].shape[0]
        if bands.top + bands.bottom >= frame_height:
            bands = FixedBands(left=bands.left, right=bands.right)
        if bands.left + bands.right >= self._width:
            bands = FixedBands(top=bands.top, bottom=bands.bottom)

        min_position = min(position + bands.top for position, _ in decoded)
        max_position = max(
            position + arr.shape[0] - bands.bottom
            for position, arr in decoded
        )
        content_height = max_position - min_position
        center_start = bands.left
        center_end = self._width - bands.right
        if content_height <= 0 or center_end <= center_start:
            return None
        canvas = np.empty((content_height, self._width, 3), dtype=np.uint8)
        # A translucent application can keep its wallpaper/background fixed in
        # screen coordinates while the foreground document scrolls. Keeping
        # only the first pixel written makes that stationary layer jump at
        # every frame boundary. Fuse aligned rows with a feathered viewport
        # weight: pixels near the viewport centre carry most of the weight,
        # while pixels entering/leaving at the edges taper to one. This makes
        # the background transition gradually instead of switching at a hard
        # frame boundary. The accumulator stays bounded to one uint32 per row.
        weights = np.minimum(
            np.arange(frame_height, dtype=np.uint32) + 1,
            np.arange(frame_height, 0, -1, dtype=np.uint32),
        )
        content_weights = weights[bands.top:frame_height - bands.bottom]
        weight_sum = np.zeros(content_height, dtype=np.uint32)
        peak_weight = np.zeros(content_height, dtype=np.uint32)

        for position, arr in decoded:
            content = arr[bands.top:arr.shape[0] - bands.bottom]
            start = position + bands.top - min_position
            end = start + content.shape[0]
            if start < 0 or end > content_height:
                return None
            target = canvas[start:end]
            if content.shape[0] != content_weights.shape[0]:
                return None
            incoming_weights = content_weights
            row_weights = weight_sum[start:end]
            incoming_peak = peak_weight[start:end]
            missing = row_weights == 0
            if np.any(missing):
                target[missing, center_start:center_end] = (
                    content[missing, center_start:center_end]
                )
            overlap = ~missing
            if np.any(overlap):
                old_weight = row_weights[overlap].astype(np.uint64)[:, None, None]
                new_weight = incoming_weights[overlap].astype(np.uint64)[:, None, None]
                old = target[overlap, center_start:center_end].astype(np.uint32)
                incoming = content[overlap, center_start:center_end].astype(np.uint32)
                total_weight = old_weight + new_weight
                blended = (
                    old.astype(np.uint64) * old_weight
                    + incoming.astype(np.uint64) * new_weight
                    + total_weight // 2
                ) // total_weight
                fused = old.astype(np.uint8)
                delta = np.max(
                    np.abs(old.astype(np.int16) - incoming.astype(np.int16)),
                    axis=2,
                )
                smooth = delta <= FUSION_MAX_PIXEL_DELTA
                fused[smooth] = blended.astype(np.uint8)[smooth]
                # Keep a complete state for high-contrast local changes. A
                # frame nearer the viewport centre wins, which avoids turning
                # a blinking caret/video tile into a translucent ghost.
                prefer_incoming = (
                    incoming_weights[overlap] >= incoming_peak[overlap]
                )[:, None]
                replace = (~smooth) & prefer_incoming
                fused[replace] = incoming[replace]
                target[overlap, center_start:center_end] = fused
            weight_sum[start:end] += incoming_weights
            peak_weight[start:end] = np.maximum(
                peak_weight[start:end], incoming_weights
            )

        if not bool((weight_sum > 0).all()):
            return None

        # Fixed sidebars cannot be repeated down the page. Extend the nearest
        # scrolling pixel as a neutral background, then paste the first
        # viewport's fixed sidebar once at its real vertical position.
        if bands.left:
            canvas[:, :bands.left] = canvas[:, bands.left:bands.left + 1]
        if bands.right:
            canvas[:, center_end:] = canvas[:, center_end - 1:center_end]
        if bands.left or bands.right:
            first_position, first_arr = decoded[0]
            first_content = first_arr[
                bands.top:first_arr.shape[0] - bands.bottom
            ]
            start = first_position + bands.top - min_position
            end = start + first_content.shape[0]
            if start < 0 or end > content_height:
                return None
            if bands.left:
                canvas[start:end, :bands.left] = first_content[:, :bands.left]
            if bands.right:
                canvas[start:end, center_end:] = first_content[:, center_end:]

        parts = []
        first_arr = decoded[0][1]
        if bands.top:
            parts.append(first_arr[:bands.top])
        parts.append(canvas)
        if bands.bottom:
            parts.append(first_arr[-bands.bottom:])
        return parts[0] if len(parts) == 1 else np.vstack(parts)

    def _offline_edge(self, previous: _Keyframe,
                      current: _Keyframe) -> _GraphEdge | None:
        excluded_rows = self._fixed_row_mask()
        excluded_columns = self._fixed_column_mask()
        previous_cols = self._matching_cols(
            previous.cols, previous.pixels, excluded_columns
        )
        current_cols = self._matching_cols(
            current.cols, current.pixels, excluded_columns
        )
        predictions = [0]
        if (previous.online_position is not None
                and current.online_position is not None):
            predictions.insert(
                0, current.online_position - previous.online_position
            )

        candidates: list[_GraphEdge] = []
        for predict in dict.fromkeys(predictions):
            shift, diff = self._find_shift_for(
                previous_cols,
                current_cols,
                predict,
                excluded_rows=excluded_rows,
            )
            robust = False
            if diff > self.max_diff:
                robust_shift, robust_diff = self._find_shift_for(
                    previous_cols,
                    current_cols,
                    predict,
                    robust=True,
                    excluded_rows=excluded_rows,
                )
                if robust_diff < diff:
                    shift, diff = robust_shift, robust_diff
                    robust = robust_diff <= self.max_diff
            if diff > self.max_diff:
                continue

            if abs(shift) < self.min_shift_px:
                if not _is_duplicate(previous.signature, current.signature):
                    continue
                candidates.append(_GraphEdge(0, diff, robust, diff + 0.25))
                continue

            pixel_diff_fn = (
                _robust_pixel_overlap_diff if robust else _pixel_overlap_diff
            )
            aligned = pixel_diff_fn(
                previous.pixels,
                current.pixels,
                shift,
                excluded_rows=excluded_rows,
                excluded_columns=excluded_columns,
            )
            stationary = pixel_diff_fn(
                previous.pixels,
                current.pixels,
                0,
                excluded_rows=excluded_rows,
                excluded_columns=excluded_columns,
            )
            changed = _pixel_change_fraction(
                previous.pixels,
                current.pixels,
                excluded_rows=excluded_rows,
                excluded_columns=excluded_columns,
            )
            pixel_limit = (
                ROBUST_MAX_PIXEL_DIFF if robust else MAX_PIXEL_DIFF
            )
            if (
                changed < 0.012
                or stationary <= aligned + 0.2
                or aligned > pixel_limit
            ):
                continue
            cost = diff + (1.0 if robust else 0.0)
            candidates.append(_GraphEdge(shift, diff, robust, cost))

        if not candidates:
            return None
        return min(candidates, key=lambda edge: edge.cost)

    # ------------------------------------------------------------------
    # live preview (for the recorder UI)

    def current_height(self) -> int:
        return self._height

    def preview_thumbnail(self, max_w: int, max_h: int) -> Image.Image | None:
        """A down-scaled snapshot of the stitched canvas so far, or None.

        The heavy per-pixel scaling already happened once per block, in
        ``_thumb_add_block``. Here we only gather the *tail* thumbnail blocks
        needed to fill ``max_h`` preview rows (newest content, which is what the
        user is scrolling toward) and stack those small pieces. Cost is bounded
        by the preview size, independent of total canvas height — this is what
        removes the O(height) per-frame preview cost that made long scrolls
        stutter and drop frames.
        """
        if not self._thumb_blocks:
            return None
        # Gather tail blocks until we have at least the rows the box can show
        # (at the thumbnail's own scale). We display the newest region.
        target_rows = self._preview_rows_for(max_w, max_h)
        picked: list[np.ndarray] = []
        total = 0
        for block in reversed(self._thumb_blocks):
            picked.append(block)
            total += block.shape[0]
            if total >= target_rows:
                break
        picked.reverse()
        live = picked[0] if len(picked) == 1 else np.vstack(picked)
        if total > target_rows:
            live = live[total - target_rows:]
        img = Image.fromarray(live, mode="RGB")
        w, h = img.size
        if w <= 0 or h <= 0:
            return None
        ratio = min(max_w / w, max_h / h, 1.0)
        if ratio < 1.0:
            img = img.resize((max(1, int(w * ratio)), max(1, int(h * ratio))),
                             Image.BILINEAR)
        return img.convert("RGBA")

    def _preview_rows_for(self, max_w: int, max_h: int) -> int:
        """How many thumbnail rows are needed to fill a max_w x max_h box.

        Thumbnail blocks are stored at width ``min(canvas_w, _preview_w)``; when
        fit into the box that width becomes ``min(thumb_w, max_w)`` and the
        height scales the same way, so the thumb rows mapping onto ``max_h`` box
        rows is ``max_h * thumb_w / min(thumb_w, max_w)``. Clamped to a minimum.
        """
        tw = min(self._width, self._preview_w) if self._width else self._preview_w
        tw = max(1, tw)
        fit_w = min(tw, max_w)
        if fit_w <= 0:
            return max_h
        return max(max_h, int(round(max_h * tw / fit_w)))


# ---------------------------------------------------------------------------
# row-signature core (ported from wl-longshot, vectorised with numpy)

def _to_rgb_array(img: Image.Image) -> np.ndarray:
    return np.asarray(img.convert("RGB"), dtype=np.uint8)


def _sample_columns(width: int) -> np.ndarray:
    """Up to 96 equally spaced column indices — enough to characterise a row."""
    count = min(max(width, 1), 96)
    if count == 1:
        return np.array([0], dtype=np.intp)
    return (np.arange(count, dtype=np.int64) * (width - 1) // (count - 1)).astype(np.intp)


def _compute_cols(arr: np.ndarray) -> np.ndarray:
    """Reduce each row to [mean*2, contrast, bright+edges]; returns [H, 3]."""
    return _compute_cols_from_pixels(_sample_pixels(arr))


def _compute_cols_from_pixels(pixels: np.ndarray) -> np.ndarray:
    """Build row signatures from an already-selected set of RGB columns."""
    h = pixels.shape[0]
    g = (0.299 * pixels[:, :, 0] + 0.587 * pixels[:, :, 1]
         + 0.114 * pixels[:, :, 2]).astype(np.float32)  # [H, count]
    mean = g.mean(axis=1)                              # [H]
    centered = g - mean[:, None]
    contrast = np.abs(centered).mean(axis=1)
    bright = np.maximum(centered - 8.0, 0.0).mean(axis=1)
    if g.shape[1] > 1:
        edges = np.abs(np.diff(g, axis=1)).mean(axis=1)
    else:
        edges = np.zeros(h, dtype=np.float32)
    return np.stack([mean * 2.0, contrast, bright + edges], axis=1)


def _sample_pixels(arr: np.ndarray) -> np.ndarray:
    """Retain sparse RGB columns for the final motion-consistency check."""
    return np.ascontiguousarray(arr[:, _sample_columns(arr.shape[1]), :])


def _pixel_overlap_diff(
    a: np.ndarray,
    b: np.ndarray,
    offset: int,
    *,
    excluded_rows: np.ndarray | None = None,
    excluded_columns: np.ndarray | None = None,
) -> float:
    """Raw-pixel overlap MAD using the same ignored edge bands as matching."""
    h1, h2 = len(a), len(b)
    if offset >= 0:
        a_start, b_start, length = offset, 0, min(h1 - offset, h2)
    else:
        a_start, b_start, length = 0, -offset, min(h1, h2 + offset)
    if length <= 0:
        return float("inf")
    top = _content_top_ignore(length)
    bottom = _content_bottom_ignore(length)
    end = length - bottom
    if end <= top:
        return float("inf")
    aa, bb = _masked_overlap(
        a, b, a_start, b_start, top, end,
        excluded_rows=excluded_rows,
        excluded_columns=excluded_columns,
    )
    if aa.size == 0:
        return float("inf")
    aa = aa.astype(np.int16)
    bb = bb.astype(np.int16)
    return float(np.abs(aa - bb).mean())


def _robust_pixel_overlap_diff(
    a: np.ndarray,
    b: np.ndarray,
    offset: int,
    *,
    excluded_rows: np.ndarray | None = None,
    excluded_columns: np.ndarray | None = None,
) -> float:
    """Sparse RGB overlap MAD after dropping the noisiest 20% of rows."""
    h1, h2 = len(a), len(b)
    if offset >= 0:
        a_start, b_start, length = offset, 0, min(h1 - offset, h2)
    else:
        a_start, b_start, length = 0, -offset, min(h1, h2 + offset)
    if length <= 0:
        return float("inf")
    top = _content_top_ignore(length)
    bottom = _content_bottom_ignore(length)
    end = length - bottom
    if end <= top:
        return float("inf")
    aa, bb = _masked_overlap(
        a, b, a_start, b_start, top, end,
        excluded_rows=excluded_rows,
        excluded_columns=excluded_columns,
    )
    if aa.size == 0:
        return float("inf")
    aa = aa.astype(np.int16)
    bb = bb.astype(np.int16)
    row_scores = np.abs(aa - bb).mean(axis=(1, 2))
    keep = max(1, (len(row_scores) * 4) // 5)
    if keep == len(row_scores):
        return float(row_scores.mean())
    return float(np.partition(row_scores, keep - 1)[:keep].mean())


def _pixel_change_fraction(
    a: np.ndarray,
    b: np.ndarray,
    *,
    excluded_rows: np.ndarray | None = None,
    excluded_columns: np.ndarray | None = None,
) -> float:
    """Fraction of sparse pixels with a perceptible change at zero offset."""
    h = min(len(a), len(b))
    w = min(a.shape[1], b.shape[1])
    if h <= 0 or w <= 0:
        return 1.0
    rows = np.ones(h, dtype=bool)
    if excluded_rows is not None and excluded_rows.shape == (h,):
        rows &= ~excluded_rows
    columns = np.ones(w, dtype=bool)
    if excluded_columns is not None and excluded_columns.shape == (w,):
        columns &= ~excluded_columns
    if not bool(rows.any()) or not bool(columns.any()):
        return 1.0
    aa = a[:h, :w][rows][:, columns]
    bb = b[:h, :w][rows][:, columns]
    delta = np.abs(aa.astype(np.int16) - bb.astype(np.int16))
    return float((delta.max(axis=2) > 6).mean())


def _masked_overlap(
    a: np.ndarray,
    b: np.ndarray,
    a_start: int,
    b_start: int,
    top: int,
    end: int,
    *,
    excluded_rows: np.ndarray | None,
    excluded_columns: np.ndarray | None = None,
) -> tuple[np.ndarray, np.ndarray]:
    """Return an overlap with viewport-fixed rows/columns removed."""
    if excluded_rows is None and excluded_columns is None:
        return (
            a[a_start + top:a_start + end],
            b[b_start + top:b_start + end],
        )
    a_rows = np.arange(a_start + top, a_start + end)
    b_rows = np.arange(b_start + top, b_start + end)
    active_rows = np.ones(len(a_rows), dtype=bool)
    if excluded_rows is not None:
        if excluded_rows.shape == (len(a),) and len(a) == len(b):
            active_rows &= ~(excluded_rows[a_rows] | excluded_rows[b_rows])
    columns = np.ones(min(a.shape[1], b.shape[1]), dtype=bool)
    if excluded_columns is not None and excluded_columns.shape == columns.shape:
        active_columns = ~excluded_columns
        if int(active_columns.sum()) >= 4:
            columns = active_columns
    return (
        a[a_rows[active_rows], :len(columns)][:, columns],
        b[b_rows[active_rows], :len(columns)][:, columns],
    )


def _content_top_ignore(length: int) -> int:
    if length < 80:
        return 0
    return min(length // 4, max(16, length // 10))


def _content_bottom_ignore(length: int) -> int:
    if length < 80:
        return 0
    return min(length // 4, max(16, length * 8 // 100))


def _effective_min_overlap(frame_height: int) -> int:
    return min(100, max(12, frame_height // 4))


def _col_diff(
    a: np.ndarray,
    b: np.ndarray,
    offset: int,
    min_overlap: int,
    *,
    excluded_rows: np.ndarray | None = None,
) -> float:
    """Mean per-channel abs diff of the overlap when b is shifted by offset.

    offset >= 0 aligns a[offset:] with b[:...]; offset < 0 the reverse.
    Returns +inf when the confident overlap is too small to trust.
    """
    h1, h2 = len(a), len(b)
    if offset >= 0:
        a_start, b_start, length = offset, 0, min(h1 - offset, h2)
    else:
        a_start, b_start, length = 0, -offset, min(h1, h2 + offset)
    if length < min_overlap:
        return float("inf")
    top = _content_top_ignore(length)
    bottom = _content_bottom_ignore(length)
    end = length - bottom
    if end <= top:
        return float("inf")
    aa, bb = _masked_overlap(
        a, b, a_start, b_start, top, end,
        excluded_rows=excluded_rows,
    )
    required = min_overlap
    if excluded_rows is not None:
        required = max(12, min_overlap - int(excluded_rows.sum()))
    if len(aa) < required:
        return float("inf")
    return float(np.abs(aa - bb).mean())


def _robust_col_diff(a: np.ndarray, b: np.ndarray, offset: int,
                     min_overlap: int, *,
                     excluded_rows: np.ndarray | None = None) -> float:
    """Overlap score that tolerates a small locally-changing page region.

    This is deliberately only used after the ordinary mean score rejects a
    frame. It drops the noisiest 20% of row scores, which is enough to ignore a
    video strip, spinner, blinking caret, or lazy-loaded card while still
    requiring most of the visible page to agree at one vertical offset.
    """
    h1, h2 = len(a), len(b)
    if offset >= 0:
        a_start, b_start, length = offset, 0, min(h1 - offset, h2)
    else:
        a_start, b_start, length = 0, -offset, min(h1, h2 + offset)
    if length < min_overlap:
        return float("inf")
    top = _content_top_ignore(length)
    bottom = _content_bottom_ignore(length)
    end = length - bottom
    if end <= top:
        return float("inf")
    aa, bb = _masked_overlap(
        a, b, a_start, b_start, top, end,
        excluded_rows=excluded_rows,
    )
    required = min_overlap
    if excluded_rows is not None:
        required = max(12, min_overlap - int(excluded_rows.sum()))
    if len(aa) < required:
        return float("inf")
    row_scores = np.abs(aa - bb).mean(axis=1)
    keep = max(1, (len(row_scores) * 4) // 5)
    if keep == len(row_scores):
        return float(row_scores.mean())
    return float(np.partition(row_scores, keep - 1)[:keep].mean())


def _offset_candidates(max_offset: int, predict: int) -> Iterator[int]:
    """Signed offsets to try, nearest the predicted one first.

    Ranges over [-max_offset, max_offset] so both scroll directions match:
    positive = scrolled down, negative = scrolled up. Ordering starts at the
    previous frame's offset and fans outward, so a steady scroll (up OR down)
    usually matches on the first probe.
    """
    predict = min(max(predict, -max_offset), max_offset)
    yield predict
    for delta in range(1, 2 * max_offset + 1):
        up = predict + delta
        if up <= max_offset:
            yield up
        down = predict - delta
        if down >= -max_offset:
            yield down


def _frame_signature(arr: np.ndarray) -> np.ndarray:
    """A tiny 18x24 grayscale grid used only to detect a still (unmoved) view."""
    h, w = arr.shape[:2]
    cols, rows = 18, 24
    ys = ((np.arange(rows, dtype=np.int64) * h) // rows).clip(max=h - 1)
    xs = ((np.arange(cols, dtype=np.int64) * w) // cols).clip(max=w - 1)
    grid = arr[np.ix_(ys, xs)]  # [rows, cols, 3]
    g = (0.299 * grid[:, :, 0] + 0.587 * grid[:, :, 1]
         + 0.114 * grid[:, :, 2])
    return np.rint(g).astype(np.uint8).ravel()


def _is_duplicate(previous: np.ndarray, current: np.ndarray) -> bool:
    if previous.shape != current.shape or previous.size == 0:
        return False
    diff = np.abs(previous.astype(np.int16) - current.astype(np.int16))
    return bool(diff.mean() <= 1.1 and diff.max() <= 4)


def _fit_width(arr: np.ndarray, width: int) -> np.ndarray:
    h, w = arr.shape[:2]
    if w == width:
        return arr
    if w > width:
        return arr[:, :width, :]
    pad = np.zeros((h, width - w, 3), dtype=np.uint8)
    return np.hstack([arr, pad])


def stitch_frames(frames: list[Image.Image], *, max_diff: float = DEFAULT_MAX_DIFF,
                  min_shift_px: int = 4) -> StitchResult:
    """Convenience one-shot: stitch a list of frames."""
    st = Stitcher(max_diff=max_diff, min_shift_px=min_shift_px)
    low = 0
    for i, f in enumerate(frames):
        diff = st.add(f)
        if i > 0 and diff > max_diff:
            low += 1
    if low:
        st.warnings.append(f"{low} frame(s) had low overlap confidence")
    return st.result()
