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

A frame is appended when the best overlap diff is <= ``max_diff`` (lower is
better) and it contributes at least ``min_shift_px`` new rows. A diff above
``max_diff`` means the frames don't overlap confidently -> the recorder tells
the user to scroll back a little.

Constraints (documented for the user):
  - Vertical scroll only. Horizontal movement breaks matching.
  - Sticky headers/footers repeat; the user should avoid framing them.
  - Animated content between frames raises the diff and may be rejected.
"""
from __future__ import annotations

from dataclasses import dataclass, field

import numpy as np
from PIL import Image

# Below this overlap diff a frame is considered a confident match. This is a
# mean per-channel absolute difference of the row signatures, so it is on the
# same scale as 8-bit brightness units; ~9 matches wl-longshot's threshold.
DEFAULT_MAX_DIFF = 9.0


@dataclass
class StitchResult:
    image: Image.Image
    frames_used: int
    warnings: list[str] = field(default_factory=list)


class Stitcher:
    def __init__(self, max_diff: float = DEFAULT_MAX_DIFF,
                 min_shift_px: int = 4) -> None:
        self.max_diff = max_diff
        self.min_shift_px = min_shift_px

        # Canvas storage as an ordered list of row-blocks (top -> bottom) rather
        # than one big array. Appending/prepending a block is O(1); we only pay
        # for a single full-height copy once, in ``result()``. The old code did
        # ``np.vstack([canvas, new])`` every frame, an O(canvas height) copy that
        # turned a long scroll quadratic and was a major cause of the "slower the
        # longer you scroll" stutter.
        self._blocks: list[np.ndarray] = []      # RGB uint8 row-blocks, in order
        self._height: int = 0                    # total canvas rows = sum block heights
        self._width: int = 0
        # Incremental preview thumbnail, also a block list. Each canvas block is
        # scaled *once* to preview width when it is added (~2 ms, independent of
        # total length) and cached; the live preview just concatenates the few
        # tail blocks needed to fill the view window. Rebuilding the thumbnail
        # from the whole canvas every frame was O(canvas height) and dominated
        # per-frame cost (~70 ms at 24k px), which is what made long scrolls
        # stutter and then drop frames (-> "重叠不足").
        self._thumb_blocks: list[np.ndarray] = []  # scaled RGB blocks, in order
        self._thumb_side: str = "bottom"         # where the last content landed
        self._preview_w = 220                    # thumbnail width in px
        # Row signature of the last *tracked* frame; matching is always the
        # previous frame vs the incoming one so the two sequences are the same
        # height and the offset is a simple relative scroll distance (which may
        # be negative when the user scrolls up).
        self._last_cols: np.ndarray | None = None
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

    # ------------------------------------------------------------------

    def add(self, frame: Image.Image) -> float:
        """Add a frame. Returns the overlap diff (LOWER is better; 0 = first).

        A return value > ``max_diff`` means the frame was NOT appended
        (low-confidence overlap); the caller may prompt the user to scroll back.
        A confident match that simply didn't move far enough (< min_shift_px)
        is also not appended but returns a low diff.
        """
        arr = _to_rgb_array(frame)

        if not self._blocks:
            self._width = arr.shape[1]
            self._append_block(arr, side="bottom")
            self._last_cols = _compute_cols(arr)
            self._last_signature = _frame_signature(arr)
            self._last_offset = 0
            self._anchor_pos = 0
            self.frames_used = 1
            self.last_shift = 0
            self.last_added = arr.shape[0]
            self.last_diff = 0.0
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
        shift, diff = self._find_shift(cols)
        self.last_shift = shift
        self.last_added = 0
        self.last_diff = diff

        if diff > self.max_diff:
            # low-confidence overlap: scrolled too fast, don't append
            return diff
        if abs(shift) < self.min_shift_px:
            # confident match but essentially the same view — nothing new.
            # Deliberately do NOT refresh _last_cols so several sub-threshold
            # scrolls accumulate against a fixed reference and eventually append.
            return diff

        # `shift` is signed: +down / -up. Convert to the incoming frame's
        # position within the canvas and grow whichever edge it overhangs.
        new_pos = self._anchor_pos + shift
        self._extend_canvas(arr, new_pos)

        self._last_cols = cols
        self._last_signature = sig
        self._last_offset = shift
        self.frames_used += 1
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
            self._blocks.insert(0, block)
        self._height += block.shape[0]
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
            self._thumb_blocks.insert(0, small)
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
        h = len(last)
        min_overlap = _effective_min_overlap(h)
        max_offset = max(h - min_overlap, 0)
        if max_offset == 0:
            return 0, float(_col_diff(last, cols, 0, min_overlap))

        best_off, best_diff = 0, float("inf")
        for off in _offset_candidates(max_offset, self._last_offset):
            d = _col_diff(last, cols, off, min_overlap)
            if d < best_diff:
                best_diff, best_off = d, off
                if best_diff < 0.25:  # essentially perfect, stop early
                    break
        return best_off, best_diff

    # ------------------------------------------------------------------

    def result(self) -> StitchResult:
        if not self._blocks:
            raise ValueError("no frames added")
        # The single full-height copy we deliberately deferred from every add().
        canvas = self._blocks[0] if len(self._blocks) == 1 else np.vstack(self._blocks)
        img = Image.fromarray(canvas, mode="RGB").convert("RGBA")
        return StitchResult(image=img, frames_used=self.frames_used,
                            warnings=list(self.warnings))

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
    h, w = arr.shape[:2]
    xs = _sample_columns(w)
    g = (0.299 * arr[:, xs, 0] + 0.587 * arr[:, xs, 1]
         + 0.114 * arr[:, xs, 2]).astype(np.float32)  # [H, count]
    mean = g.mean(axis=1)                              # [H]
    centered = g - mean[:, None]
    contrast = np.abs(centered).mean(axis=1)
    bright = np.maximum(centered - 8.0, 0.0).mean(axis=1)
    if g.shape[1] > 1:
        edges = np.abs(np.diff(g, axis=1)).mean(axis=1)
    else:
        edges = np.zeros(h, dtype=np.float32)
    return np.stack([mean * 2.0, contrast, bright + edges], axis=1)


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


def _col_diff(a: np.ndarray, b: np.ndarray, offset: int, min_overlap: int) -> float:
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
    if length < min_overlap + top + bottom:
        return float("inf")
    end = length - bottom
    if end <= top:
        return float("inf")
    aa = a[a_start + top: a_start + end]
    bb = b[b_start + top: b_start + end]
    return float(np.abs(aa - bb).mean())


def _offset_candidates(max_offset: int, predict: int) -> list[int]:
    """Signed offsets to try, nearest the predicted one first.

    Ranges over [-max_offset, max_offset] so both scroll directions match:
    positive = scrolled down, negative = scrolled up. Ordering starts at the
    previous frame's offset and fans outward, so a steady scroll (up OR down)
    usually matches on the first probe.
    """
    predict = min(max(predict, -max_offset), max_offset)
    seen = {predict}
    candidates = [predict]
    for delta in range(1, 2 * max_offset + 1):
        up = predict + delta
        if up <= max_offset and up not in seen:
            candidates.append(up)
            seen.add(up)
        down = predict - delta
        if down >= -max_offset and down not in seen:
            candidates.append(down)
            seen.add(down)
    return candidates


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
