"""Toolbar: layout + Cairo/Pango drawing + hit-testing for the selection toolbar.

Uses Pango (via PangoCairo) instead of Cairo's toy-font API so that CJK
characters render correctly on systems where the default toy face doesn't
cover them.

Public API used by ``surface.py``:

    tb.hit_test(x, y, sel, screen_w, screen_h) -> ToolbarButton | None
    tb.draw(ctx, sel, screen_w, screen_h, hover_id="")
    tb.buttons  # iterable of ToolbarButton (with .id/.label/.hotkey/.hint)
"""
from __future__ import annotations

import math
from dataclasses import dataclass

import cairo
import gi

gi.require_version("Pango", "1.0")
gi.require_version("PangoCairo", "1.0")
from gi.repository import Pango, PangoCairo  # noqa: E402

from .model import Rect


@dataclass
class ToolbarButton:
    id: str
    label: str
    hotkey: str          # Gdk keyval name (lower-cased); "" = no hotkey
    hint: str            # short glyph shown on the button ("S", "⏎", ...)
    # populated during layout()
    x: float = 0.0
    y: float = 0.0
    w: float = 0.0
    h: float = 0.0

    def contains(self, px: float, py: float) -> bool:
        return self.x <= px < self.x + self.w and self.y <= py < self.y + self.h


# Order matters: this is what the user sees left-to-right.
BUTTONS: list[ToolbarButton] = [
    ToolbarButton("confirm",   "完成",   "Return", "⏎"),
    ToolbarButton("annotate",  "标注",   "d",      "D"),
    ToolbarButton("ocr",       "OCR",    "o",      "O"),
    ToolbarButton("translate", "翻译",   "t",      "T"),
    ToolbarButton("pin",       "钉图",   "p",      "P"),
    ToolbarButton("long",      "长截图", "l",      "L"),
    ToolbarButton("cancel",    "取消",   "Escape", "⎋"),
]

# Annotation sub-mode toolbar. Tool buttons switch the active tool; the last
# three are actions. The surface highlights the active tool separately from
# hover, so ids here matter.
ANNOTATE_BUTTONS: list[ToolbarButton] = [
    ToolbarButton("tool.pen",    "画笔",   "b", "B"),
    ToolbarButton("tool.arrow",  "箭头",   "a", "A"),
    ToolbarButton("tool.rect",   "矩形",   "r", "R"),
    ToolbarButton("tool.text",   "文字",   "x", "X"),
    ToolbarButton("anno.color",  "颜色",   "c", "C"),
    ToolbarButton("anno.width",  "粗细",   "w", "W"),
    ToolbarButton("anno.undo",   "撤销",   "u", "U"),
    ToolbarButton("anno.done",   "完成",   "Return", "⏎"),
]

BTN_PAD_X = 11
BTN_PAD_Y = 8
BTN_GAP = 4
GROUP_GAP = 9
BAR_MARGIN = 10         # gap between selection edge and toolbar
BAR_INNER_PAD = 6
LABEL_FONT = "Sans 10.5"
HINT_FONT = "Sans 8"
CORNER_R = 9

# Read the toolbar as deliberate action groups: finish, tools, dismiss.
GROUP_BREAK_BEFORE = {"annotate", "cancel", "anno.color", "anno.done"}


def _pango_layout(cr: cairo.Context, text: str, font: str) -> Pango.Layout:
    layout = PangoCairo.create_layout(cr)
    layout.set_font_description(Pango.FontDescription.from_string(font))
    layout.set_text(text, -1)
    return layout


def _text_pixel_size(cr: cairo.Context, text: str, font: str) -> tuple[int, int]:
    layout = _pango_layout(cr, text, font)
    return layout.get_pixel_size()


class Toolbar:
    def __init__(self, buttons: list[ToolbarButton] | None = None) -> None:
        # Deep-copy buttons so layout positions don't leak between instances.
        src = buttons if buttons is not None else BUTTONS
        self.buttons: list[ToolbarButton] = [
            ToolbarButton(b.id, b.label, b.hotkey, b.hint) for b in src
        ]
        self.bar_rect: tuple[float, float, float, float] = (0, 0, 0, 0)
        self._label_sizes: dict[str, tuple[int, int]] = {}
        self._hint_sizes: dict[str, tuple[int, int]] = {}
        self._button_h: float = 0.0
        self._separator_x: list[float] = []

    # ---- one-shot text measurement ---------------------------------------

    def _measure(self) -> None:
        if self._label_sizes:
            return
        surf = cairo.ImageSurface(cairo.FORMAT_ARGB32, 1, 1)
        cr = cairo.Context(surf)
        max_h = 0
        for b in self.buttons:
            lw, lh = _text_pixel_size(cr, b.label, LABEL_FONT)
            self._label_sizes[b.id] = (lw, lh)
            max_h = max(max_h, lh)
            if b.hint:
                self._hint_sizes[b.id] = _text_pixel_size(cr, b.hint, HINT_FONT)
        self._button_h = max_h + BTN_PAD_Y * 2

    # ---- layout ----------------------------------------------------------

    def layout(self, sel: Rect, screen_w: int, screen_h: int) -> None:
        self._measure()
        bh = self._button_h

        widths = []
        for b in self.buttons:
            lw, _ = self._label_sizes[b.id]
            hw, _ = self._hint_sizes.get(b.id, (0, 0))
            widths.append(lw + hw + BTN_PAD_X * 2 + (8 if hw else 0))
        group_count = sum(1 for b in self.buttons if b.id in GROUP_BREAK_BEFORE)
        bar_w = (sum(widths) + BTN_GAP * (len(self.buttons) - 1)
                 + GROUP_GAP * group_count + BAR_INNER_PAD * 2)
        bar_h = bh + BAR_INNER_PAD * 2

        # horizontal: center on selection, then clamp to screen
        cx = sel.x + sel.w / 2
        bar_x = cx - bar_w / 2
        bar_x = max(4, min(bar_x, screen_w - bar_w - 4))

        # vertical: prefer below, flip above if it would spill
        below_y = sel.y2 + BAR_MARGIN
        above_y = sel.y - BAR_MARGIN - bar_h
        if below_y + bar_h <= screen_h - 4:
            bar_y = below_y
        elif above_y >= 4:
            bar_y = above_y
        else:
            # neither fits — pin just inside the selection bottom
            bar_y = max(4, min(sel.y2 - bar_h - 4, screen_h - bar_h - 4))

        self.bar_rect = (bar_x, bar_y, bar_w, bar_h)

        bx = bar_x + BAR_INNER_PAD
        self._separator_x = []
        by = bar_y + BAR_INNER_PAD
        for b, w in zip(self.buttons, widths):
            if b.id in GROUP_BREAK_BEFORE:
                self._separator_x.append(bx - (BTN_GAP + GROUP_GAP / 2))
                bx += GROUP_GAP
            b.x, b.y, b.w, b.h = bx, by, w, bh
            bx += w + BTN_GAP

    # ---- hit test --------------------------------------------------------

    def hit_test(self, px: float, py: float, sel: Rect,
                 screen_w: int, screen_h: int) -> ToolbarButton | None:
        if not sel.valid:
            return None
        self.layout(sel, screen_w, screen_h)
        bx, by, bw, bh = self.bar_rect
        if not (bx <= px < bx + bw and by <= py < by + bh):
            return None
        for b in self.buttons:
            if b.contains(px, py):
                return b
        return None

    # ---- drawing ---------------------------------------------------------

    def draw(self, cr: cairo.Context, sel: Rect,
             screen_w: int, screen_h: int, hover_id: str = "",
             active_id: str = "") -> None:
        if not sel.valid:
            return
        self.layout(sel, screen_w, screen_h)
        bx, by, bw, bh = self.bar_rect

        # Two crisp translucent layers give the floating palette depth without
        # the muddy pseudo-shadow the previous single rectangle produced.
        _rounded_rect(cr, bx + 1, by + 3, bw, bh, CORNER_R)
        cr.set_source_rgba(0, 0, 0, 0.28)
        cr.fill()
        _rounded_rect(cr, bx, by, bw, bh, CORNER_R)
        cr.set_source_rgba(0.09, 0.105, 0.14, 0.96)
        cr.fill_preserve()
        cr.set_source_rgba(0.76, 0.82, 0.96, 0.18)
        cr.set_line_width(1)
        cr.stroke()

        for sx in self._separator_x:
            cr.set_source_rgba(1, 1, 1, 0.13)
            cr.set_line_width(1)
            cr.move_to(sx, by + 7)
            cr.line_to(sx, by + bh - 7)
            cr.stroke()

        for b in self.buttons:
            hovered = b.id == hover_id
            active = b.id == active_id
            _rounded_rect(cr, b.x, b.y, b.w, b.h, CORNER_R - 2)
            if b.id in ("confirm", "anno.done"):
                cr.set_source_rgba(0.39, 0.52, 0.91, 0.96)
            elif active:
                cr.set_source_rgba(0.34, 0.48, 0.88, 0.88)
            elif b.id == "cancel" and hovered:
                cr.set_source_rgba(0.56, 0.20, 0.26, 0.80)
            elif hovered:
                cr.set_source_rgba(0.30, 0.38, 0.58, 0.78)
            else:
                cr.set_source_rgba(1, 1, 1, 0.055)
            cr.fill()

            # Label + a quiet keycap; the old overlapping corner glyphs read
            # like debug annotations rather than useful shortcuts.
            lw, lh = self._label_sizes[b.id]
            cr.set_source_rgba(0.97, 0.98, 1.0, 1.0)
            cr.move_to(b.x + BTN_PAD_X, b.y + (b.h - lh) / 2 + 1)
            layout = _pango_layout(cr, b.label, LABEL_FONT)
            PangoCairo.show_layout(cr, layout)

            if b.hint and b.id in self._hint_sizes:
                hw, hh = self._hint_sizes[b.id]
                key_x = b.x + b.w - hw - BTN_PAD_X
                key_y = b.y + (b.h - hh) / 2
                _rounded_rect(cr, key_x - 4, key_y - 2, hw + 8, hh + 4, 5)
                cr.set_source_rgba(1, 1, 1, 0.10)
                cr.fill()
                cr.set_source_rgba(0.86, 0.90, 1.0, 0.72)
                cr.move_to(key_x, key_y)
                hlayout = _pango_layout(cr, b.hint, HINT_FONT)
                PangoCairo.show_layout(cr, hlayout)


def _rounded_rect(cr: cairo.Context, x: float, y: float,
                  w: float, h: float, r: float) -> None:
    r = min(r, w / 2, h / 2)
    cr.new_sub_path()
    cr.arc(x + w - r, y + r,     r, -math.pi / 2, 0)
    cr.arc(x + w - r, y + h - r, r, 0,             math.pi / 2)
    cr.arc(x + r,     y + h - r, r, math.pi / 2,   math.pi)
    cr.arc(x + r,     y + r,     r, math.pi,       3 * math.pi / 2)
    cr.close_path()
