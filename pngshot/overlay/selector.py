"""Selector: mouse-driven selection rectangle with 8 resize handles.

Owns only the selection state (a Rect) and the current interaction mode.
Rendering is done by the surface; hit-testing for handles lives here.
"""
from __future__ import annotations

from .model import HANDLE_ORDER, Hit, HitKind, Mode, Rect, handle_positions

HANDLE_HALF = 6         # half-size of a handle hit box in px
MIN_SEL = 4             # smallest selection you can create


class Selector:
    def __init__(self, screen_w: int, screen_h: int) -> None:
        self.screen_w = screen_w
        self.screen_h = screen_h
        self.rect = Rect()
        self.mode: Mode = Mode.IDLE

        # drag state
        self._drag_anchor: tuple[float, float] = (0, 0)   # where drag started (raw)
        self._orig_rect: Rect = Rect()
        self._active_handle: int = -1                     # 0..7

    # ---- hit-testing -------------------------------------------------------

    def hit_test(self, px: float, py: float) -> Hit:
        """Return what the pointer is over. Handle > inside > outside."""
        if self.rect.valid:
            # handles first (they sit on the edges/corners)
            for i, (_name, hx, hy) in enumerate(handle_positions(self.rect)):
                if abs(px - hx) <= HANDLE_HALF and abs(py - hy) <= HANDLE_HALF:
                    return Hit(HitKind.HANDLE, handle_index=i)
            if self.rect.contains(px, py):
                return Hit(HitKind.INSIDE)
        return Hit(HitKind.OUTSIDE)

    # ---- pointer events (called by surface) --------------------------------

    def press(self, px: float, py: float) -> None:
        hit = self.hit_test(px, py)
        self._drag_anchor = (px, py)
        self._orig_rect = Rect(self.rect.x, self.rect.y, self.rect.w, self.rect.h)

        if hit.kind is HitKind.HANDLE:
            self.mode = Mode.RESIZING
            self._active_handle = hit.handle_index
        elif hit.kind is HitKind.INSIDE:
            self.mode = Mode.MOVING
        else:
            # starting a brand-new selection
            self.mode = Mode.SELECTING
            self.rect = Rect(int(px), int(py), 0, 0)
            self._orig_rect = Rect(int(px), int(py), 0, 0)

    def motion(self, px: float, py: float) -> None:
        if self.mode is Mode.SELECTING:
            ax, ay = self._drag_anchor
            self.rect = Rect(int(ax), int(ay), int(px - ax), int(py - ay)).normalized()
        elif self.mode is Mode.MOVING:
            ax, ay = self._drag_anchor
            dx = int(px - ax)
            dy = int(py - ay)
            r = Rect(self._orig_rect.x + dx, self._orig_rect.y + dy,
                     self._orig_rect.w, self._orig_rect.h)
            # clamp inside screen
            r.x = max(0, min(r.x, self.screen_w - r.w))
            r.y = max(0, min(r.y, self.screen_h - r.h))
            self.rect = r
        elif self.mode is Mode.RESIZING:
            self.rect = self._resize_from(px, py).clamp_to(self.screen_w, self.screen_h)

    def release(self) -> None:
        if self.mode in (Mode.SELECTING, Mode.MOVING, Mode.RESIZING):
            r = self.rect.normalized().clamp_to(self.screen_w, self.screen_h)
            if r.w < MIN_SEL or r.h < MIN_SEL:
                # too small: treat as cleared
                self.rect = Rect()
                self.mode = Mode.IDLE
            else:
                self.rect = r
                self.mode = Mode.HAS_SELECTION
        self._active_handle = -1

    # ---- helpers -----------------------------------------------------------

    def clear(self) -> None:
        self.rect = Rect()
        self.mode = Mode.IDLE

    def _resize_from(self, px: float, py: float) -> Rect:
        r0 = self._orig_rect
        left, top = r0.x, r0.y
        right, bot = r0.x2, r0.y2
        name = HANDLE_ORDER[self._active_handle]
        if "n" in name: top = int(py)
        if "s" in name: bot = int(py)
        if "w" in name: left = int(px)
        if "e" in name: right = int(px)
        return Rect(min(left, right), min(top, bot),
                    abs(right - left), abs(bot - top))
