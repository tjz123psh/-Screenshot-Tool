"""Non-interactive selection outline shown while a long shot is recording.

Each edge is its own tiny layer-shell surface placed strictly *outside* the
sampled rectangle.  The selected area stays unobscured and grim cannot record
the blue guide into the result (unlike drawing a border over the capture).
"""
from __future__ import annotations

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gtk4LayerShell", "1.0")

from gi.repository import Gtk, Gtk4LayerShell  # noqa: E402

from ..overlay.model import Rect
from ..util import theme


EDGE_WIDTH = 4


def edge_rects(rect: Rect, screen_size: tuple[int, int],
               width: int = EDGE_WIDTH) -> list[tuple[int, int, int, int]]:
    """Return visible outline segments, all outside ``rect``."""
    sw, sh = screen_size
    edges: list[tuple[int, int, int, int]] = []
    if rect.y >= width:
        edges.append((rect.x, rect.y - width, rect.w, width))
    if rect.y + rect.h + width <= sh:
        edges.append((rect.x, rect.y + rect.h, rect.w, width))
    if rect.x >= width:
        edges.append((rect.x - width, rect.y, width, rect.h))
    if rect.x + rect.w + width <= sw:
        edges.append((rect.x + rect.w, rect.y, width, rect.h))
    return [r for r in edges if r[2] > 0 and r[3] > 0]


class SelectionHighlight:
    def __init__(self, app: Gtk.Application, rect: Rect,
                 screen_size: tuple[int, int] | None) -> None:
        self.windows: list[Gtk.ApplicationWindow] = []
        if not screen_size:
            return
        for x, y, w, h in edge_rects(rect, screen_size):
            win = Gtk.ApplicationWindow(application=app)
            win.set_decorated(False)
            win.set_focusable(False)
            win.set_default_size(w, h)
            Gtk4LayerShell.init_for_window(win)
            Gtk4LayerShell.set_layer(win, Gtk4LayerShell.Layer.OVERLAY)
            Gtk4LayerShell.set_namespace(win, "pngshot-longshot-highlight")
            Gtk4LayerShell.set_keyboard_mode(win, Gtk4LayerShell.KeyboardMode.NONE)
            Gtk4LayerShell.set_anchor(win, Gtk4LayerShell.Edge.TOP, True)
            Gtk4LayerShell.set_anchor(win, Gtk4LayerShell.Edge.LEFT, True)
            Gtk4LayerShell.set_margin(win, Gtk4LayerShell.Edge.TOP, y)
            Gtk4LayerShell.set_margin(win, Gtk4LayerShell.Edge.LEFT, x)
            # Selection coordinates come from the full-output overlay/grim,
            # including areas behind Niri's top bar.  Ignore existing exclusive
            # zones so layer-shell margins use that exact same origin.
            Gtk4LayerShell.set_exclusive_zone(win, -1)
            theme.apply(win)
            win.add_css_class("pngshot-highlight-window")
            edge = Gtk.Box()
            edge.add_css_class("pngshot-highlight-edge")
            win.set_child(edge)
            self.windows.append(win)

    def present(self) -> None:
        for window in self.windows:
            window.present()

    def close(self) -> None:
        for window in self.windows:
            window.close()
        self.windows.clear()
