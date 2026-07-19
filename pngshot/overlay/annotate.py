"""Annotation sub-mode for the Stage 1 overlay.

Lives inside the same overlay surface as the selector. When the user hits the
"涂鸦" button we flip into annotation: the selection rect is now the canvas and
drawing tools replace the region toolbar. Finishing bakes the strokes into the
cropped image which then re-enters the normal action pipeline.

Tools:
  - pen    : freehand polyline
  - arrow  : straight line with an arrowhead at the end
  - rect   : hollow rectangle
  - text   : click to place, type, Enter/Esc to commit

All stroke geometry is stored in *screen* coordinates (same frame as the
selector rect), so drawing over the live overlay is a 1:1 blit and baking only
needs to subtract the rect origin.
"""
from __future__ import annotations

import math
from dataclasses import dataclass, field

import cairo
import gi

gi.require_version("Pango", "1.0")
gi.require_version("PangoCairo", "1.0")
from gi.repository import Pango, PangoCairo  # noqa: E402

# Preset palette cycled by the color button.
PALETTE: list[tuple[float, float, float]] = [
    (0.93, 0.20, 0.23),   # red
    (0.99, 0.76, 0.18),   # amber
    (0.30, 0.79, 0.35),   # green
    (0.26, 0.60, 0.99),   # blue
    (0.10, 0.10, 0.11),   # near-black
    (0.98, 0.98, 0.99),   # white
]
WIDTHS: list[float] = [2.0, 4.0, 7.0, 11.0]

TOOLS = ("pen", "arrow", "rect", "text")


@dataclass
class Stroke:
    tool: str
    color: tuple[float, float, float]
    width: float
    # pen: list of points; arrow/rect: [start, end]; text: [pos]
    points: list[tuple[float, float]] = field(default_factory=list)
    text: str = ""


class Annotator:
    """Holds annotation state and renders/bakes strokes."""

    def __init__(self) -> None:
        self.strokes: list[Stroke] = []
        self.tool: str = "pen"
        self.color_idx: int = 0
        self.width_idx: int = 1
        self._active: Stroke | None = None
        # text editing
        self.editing_text: Stroke | None = None
        # Completed strokes are rasterized once.  The overlay can receive
        # hundreds of motion events per second; replaying every old pen point
        # on each frame made long annotations increasingly expensive.
        self._cache: cairo.ImageSurface | None = None
        self._cache_origin: tuple[int, int] = (0, 0)

    # ---- properties ------------------------------------------------------

    @property
    def color(self) -> tuple[float, float, float]:
        return PALETTE[self.color_idx]

    @property
    def width(self) -> float:
        return WIDTHS[self.width_idx]

    def cycle_color(self) -> None:
        self.color_idx = (self.color_idx + 1) % len(PALETTE)

    def cycle_width(self) -> None:
        self.width_idx = (self.width_idx + 1) % len(WIDTHS)

    def set_tool(self, tool: str) -> None:
        if tool in TOOLS:
            self.tool = tool

    # ---- undo ------------------------------------------------------------

    def undo(self) -> None:
        # commit-in-progress text counts as a stroke; drop the active edit first
        if self.editing_text is not None:
            self.editing_text = None
            return
        if self.strokes:
            self.strokes.pop()
            self._rebuild_cache()

    def has_content(self) -> bool:
        return bool(self.strokes) or self.editing_text is not None

    # ---- pointer ---------------------------------------------------------

    def press(self, x: float, y: float) -> None:
        if self.tool == "text":
            # commit any current text edit, then begin a new one
            self._commit_text()
            self.editing_text = Stroke(
                tool="text", color=self.color, width=self.width,
                points=[(x, y)], text="",
            )
            return
        self._active = Stroke(
            tool=self.tool, color=self.color, width=self.width, points=[(x, y)]
        )
        if self.tool in ("arrow", "rect"):
            # second point tracks the cursor
            self._active.points.append((x, y))

    def motion(self, x: float, y: float) -> None:
        s = self._active
        if s is None:
            return
        if s.tool == "pen":
            # GTK can report identical or sub-pixel-neighbouring motion events
            # at a much higher rate than the display refreshes.  They do not
            # change the visible stroke, so discard them before storing points.
            lx, ly = s.points[-1]
            if (x - lx) ** 2 + (y - ly) ** 2 < 1.0:
                return
            s.points.append((x, y))
        else:  # arrow / rect: move the end point
            s.points[1] = (x, y)

    def release(self, x: float, y: float) -> None:
        s = self._active
        self._active = None
        if s is None:
            return
        if s.tool == "pen":
            if len(s.points) >= 2:
                self._append_stroke(s)
        else:
            s.points[1] = (x, y)
            # ignore zero-size drags
            (x0, y0), (x1, y1) = s.points
            if abs(x1 - x0) > 2 or abs(y1 - y0) > 2:
                self._append_stroke(s)

    # ---- text editing ----------------------------------------------------

    def type_char(self, ch: str) -> None:
        if self.editing_text is not None:
            self.editing_text.text += ch

    def backspace(self) -> None:
        if self.editing_text is not None and self.editing_text.text:
            self.editing_text.text = self.editing_text.text[:-1]

    def _commit_text(self) -> None:
        if self.editing_text is not None:
            if self.editing_text.text.strip():
                self._append_stroke(self.editing_text)
            self.editing_text = None

    def commit_text(self) -> None:
        self._commit_text()

    # ---- rendering -------------------------------------------------------

    def begin_canvas(self, rect) -> None:
        """Prepare a transparent cache for the fixed annotation selection."""
        self._cache_origin = (int(rect.x), int(rect.y))
        width, height = int(rect.w), int(rect.h)
        if width <= 0 or height <= 0:
            self._cache = None
            return
        self._cache = cairo.ImageSurface(cairo.FORMAT_ARGB32, width, height)
        self._rebuild_cache()

    def _append_stroke(self, stroke: Stroke) -> None:
        self.strokes.append(stroke)
        self._draw_to_cache(stroke)

    def _draw_to_cache(self, stroke: Stroke) -> None:
        if self._cache is None:
            return
        cr = cairo.Context(self._cache)
        cr.translate(-self._cache_origin[0], -self._cache_origin[1])
        self._draw_stroke(cr, stroke)
        self._cache.flush()

    def _rebuild_cache(self) -> None:
        if self._cache is None:
            return
        cr = cairo.Context(self._cache)
        cr.set_operator(cairo.Operator.CLEAR)
        cr.paint()
        cr.set_operator(cairo.Operator.OVER)
        for stroke in self.strokes:
            self._draw_to_cache(stroke)

    def draw(self, cr: cairo.Context) -> None:
        if self._cache is not None:
            cr.set_source_surface(self._cache, *self._cache_origin)
            cr.paint()
        else:
            for s in self.strokes:
                self._draw_stroke(cr, s)
        if self._active is not None:
            self._draw_stroke(cr, self._active)
        if self.editing_text is not None:
            self._draw_text(cr, self.editing_text, caret=True)

    def _draw_stroke(self, cr: cairo.Context, s: Stroke) -> None:
        cr.set_source_rgb(*s.color)
        cr.set_line_width(s.width)
        cr.set_line_cap(cairo.LineCap.ROUND)
        cr.set_line_join(cairo.LineJoin.ROUND)
        if s.tool == "pen":
            if len(s.points) < 2:
                return
            cr.move_to(*s.points[0])
            for p in s.points[1:]:
                cr.line_to(*p)
            cr.stroke()
        elif s.tool == "arrow":
            self._draw_arrow(cr, s)
        elif s.tool == "rect":
            (x0, y0), (x1, y1) = s.points
            cr.rectangle(min(x0, x1), min(y0, y1), abs(x1 - x0), abs(y1 - y0))
            cr.stroke()
        elif s.tool == "text":
            self._draw_text(cr, s)

    def _draw_arrow(self, cr: cairo.Context, s: Stroke) -> None:
        (x0, y0), (x1, y1) = s.points
        cr.move_to(x0, y0)
        cr.line_to(x1, y1)
        cr.stroke()
        # arrowhead
        angle = math.atan2(y1 - y0, x1 - x0)
        head = max(10.0, s.width * 3)
        for da in (math.radians(160), math.radians(-160)):
            hx = x1 + head * math.cos(angle + da)
            hy = y1 + head * math.sin(angle + da)
            cr.move_to(x1, y1)
            cr.line_to(hx, hy)
        cr.stroke()

    def _draw_text(self, cr: cairo.Context, s: Stroke, caret: bool = False) -> None:
        x, y = s.points[0]
        size = max(12.0, s.width * 4)
        shown = s.text + ("|" if caret else "")
        # Pango so CJK glyphs render via fontconfig (Cairo toy API drops them).
        layout = PangoCairo.create_layout(cr)
        desc = Pango.FontDescription()
        desc.set_family("Sans")
        desc.set_weight(Pango.Weight.BOLD)
        desc.set_absolute_size(size * Pango.SCALE)
        layout.set_font_description(desc)
        layout.set_text(shown, -1)
        # subtle shadow for readability
        cr.set_source_rgba(0, 0, 0, 0.5)
        cr.move_to(x + 1, y + 1)
        PangoCairo.show_layout(cr, layout)
        cr.set_source_rgb(*s.color)
        cr.move_to(x, y)
        PangoCairo.show_layout(cr, layout)

    # ---- baking ----------------------------------------------------------

    def bake(self, base_surface: cairo.ImageSurface, rect) -> cairo.ImageSurface:
        """Draw all strokes onto a copy of the cropped region.

        ``base_surface`` is the full-screen background; ``rect`` is the
        selection in screen coords. Returns a new ARGB surface of the crop size
        with annotations burned in.
        """
        out = cairo.ImageSurface(cairo.FORMAT_ARGB32, int(rect.w), int(rect.h))
        cr = cairo.Context(out)
        # blit the cropped region of the background
        cr.set_source_surface(base_surface, -rect.x, -rect.y)
        cr.paint()
        # make sure any in-progress text is included
        self._commit_text()
        if self._cache is not None:
            cr.set_source_surface(self._cache, 0, 0)
            cr.paint()
        else:
            # translate so screen-space strokes land in crop-space
            cr.translate(-rect.x, -rect.y)
            self.draw(cr)
        out.flush()
        return out
