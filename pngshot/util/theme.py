"""Shared GTK CSS theme for pngshot's floating windows.

One place to keep the look consistent across the long-shot control panel and
the OCR / translation result window: rounded cards, soft borders, a single
accent colour, and buttons with clear hover / active / focus feedback.

Usage::

    from ..util import theme
    theme.apply(self.window)          # loads CSS once per display
    root.add_css_class("pngshot-card")

The provider is installed on the window's ``Gdk.Display`` at
``APPLICATION`` priority so it layers cleanly on top of the user's GTK theme
without fighting it, and is only installed once per display.
"""
from __future__ import annotations

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")

from gi.repository import Gdk, Gtk  # noqa: E402

# Bumped whenever the CSS below changes so a long-lived display re-loads it.
_CSS_VERSION = 3
_INSTALLED: set[int] = set()

# Accent + surface palette. Kept in one block so the two windows never drift.
#   accent      interactive / primary actions
#   surface     card background (semi-opaque dark, works over any wallpaper)
#   text/dim    foreground tiers
_CSS = b"""
/* ---- card surface -------------------------------------------------- */
.pngshot-card {
  background-color: rgba(28, 30, 38, 0.96);
  border: 1px solid rgba(255, 255, 255, 0.08);
  border-radius: 18px;
  color: #e8eaf0;
  /* Softer, more translucent drop shadow so the card floats without the
     heavy "smudge" a single opaque shadow leaves on a dark background. */
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.28),
              0 14px 34px rgba(0, 0, 0, 0.40);
}

/* headings inside a card */
.pngshot-card .pngshot-title {
  font-size: 15px;
  font-weight: 700;
  color: #f4f6fb;
}

.pngshot-card .pngshot-dim {
  color: rgba(232, 234, 240, 0.55);
  font-size: 12px;
}

/* ---- preview frame (long-shot) ------------------------------------- */
.pngshot-preview {
  background-color: rgba(0, 0, 0, 0.35);
  border: 1px solid rgba(255, 255, 255, 0.10);
  border-radius: 12px;
  padding: 4px;
}

/* the card turns its border red when overlap confidence drops, without a
   layout shift (border width is unchanged, only the colour) */
.pngshot-card.pngshot-alert {
  border-color: rgba(255, 107, 107, 0.85);
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.28),
              0 14px 34px rgba(0, 0, 0, 0.40),
              0 0 0 1px rgba(255, 107, 107, 0.35);
}

/* ---- text view (result window) ------------------------------------- */
.pngshot-textview {
  background-color: rgba(0, 0, 0, 0.38);
  border: 1px solid rgba(255, 255, 255, 0.10);
  border-radius: 12px;
  color: #eef0f6;
  font-size: 14px;
}
.pngshot-textview text {
  background-color: transparent;
  color: #eef0f6;
  padding: 12px 16px;
  line-height: 1.5;
}
.pngshot-textview text selection {
  background-color: rgba(76, 116, 217, 0.55);
  color: #ffffff;
}

/* ---- buttons ------------------------------------------------------- */
.pngshot-card button,
.pngshot-window button {
  border-radius: 13px;
  padding: 8px 18px;
  font-weight: 600;
  border: 1px solid rgba(255, 255, 255, 0.10);
  background-image: none;
  background-color: rgba(255, 255, 255, 0.06);
  color: #e8eaf0;
  transition: background-color 120ms ease, border-color 120ms ease;
  box-shadow: none;
  text-shadow: none;
}
.pngshot-card button:hover,
.pngshot-window button:hover {
  background-color: rgba(255, 255, 255, 0.13);
  border-color: rgba(255, 255, 255, 0.18);
}
.pngshot-card button:active,
.pngshot-window button:active {
  background-color: rgba(255, 255, 255, 0.20);
}
.pngshot-card button:focus,
.pngshot-window button:focus {
  outline: none;
  border-color: rgba(76, 116, 217, 0.75);
}

/* primary / suggested action: filled accent */
.pngshot-card button.suggested-action,
.pngshot-window button.suggested-action {
  background-image: none;
  background-color: #4c74d9;
  border-color: rgba(76, 116, 217, 0.0);
  color: #ffffff;
}
.pngshot-card button.suggested-action:hover,
.pngshot-window button.suggested-action:hover {
  background-color: #5b82e6;
}
.pngshot-card button.suggested-action:active,
.pngshot-window button.suggested-action:active {
  background-color: #4064c2;
}

/* destructive-ish (cancel) stays subtle unless hovered */
.pngshot-card button.pngshot-quiet:hover,
.pngshot-window button.pngshot-quiet:hover {
  background-color: rgba(255, 107, 107, 0.18);
  border-color: rgba(255, 107, 107, 0.45);
}

/* the whole result window surface */
.pngshot-window {
  background-color: rgba(28, 30, 38, 0.98);
  color: #e8eaf0;
}

/* Transparent window chrome for the long-shot control panel: the panel is a
   layer-shell overlay floating over the desktop, so only the rounded
   .pngshot-card (and its drop shadow) should be visible. Without this the
   window's default theme background paints a solid rectangle in the margin
   around the card's rounded corners.

   Only the window node itself is cleared (NOT its descendants) so the
   .pngshot-card keeps its own opaque surface. (GTK auto-adds `.background`
   to the window; our APPLICATION-priority provider overrides it regardless
   of selector specificity.) */
.pngshot-transparent {
  background-color: transparent;
}
"""


def apply(window: Gtk.Widget) -> None:
    """Install the shared CSS on the widget's display (once per display)."""
    display = window.get_display() if hasattr(window, "get_display") else None
    if display is None:
        display = Gdk.Display.get_default()
    if display is None:
        return
    key = id(display) ^ (_CSS_VERSION << 24)
    if key in _INSTALLED:
        return
    provider = Gtk.CssProvider()
    provider.load_from_data(_CSS)
    Gtk.StyleContext.add_provider_for_display(
        display, provider, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION
    )
    _INSTALLED.add(key)
