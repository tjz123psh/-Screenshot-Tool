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
_CSS_VERSION = 5
_INSTALLED: set[int] = set()

# Accent + surface palette. Kept in one block so the two windows never drift.
#   accent      interactive / primary actions
#   surface     card background (semi-opaque dark, works over any wallpaper)
#   text/dim    foreground tiers
_CSS = b"""
/* pngshot's visual language: a dark camera-workbench surface with one calm
   blue accent. These surfaces are deliberately opaque enough to remain legible
   over a busy wallpaper while retaining a little of the desktop underneath. */
.pngshot-card,
.pngshot-window {
  background-color: rgba(23, 26, 33, 0.97);
  color: #f2f4f8;
}
.pngshot-card {
  border: 1px solid rgba(255, 255, 255, 0.12);
  border-radius: 16px;
  box-shadow: 0 3px 10px rgba(0, 0, 0, 0.30),
              0 18px 42px rgba(0, 0, 0, 0.42);
}
.pngshot-window {
  border: 1px solid rgba(255, 255, 255, 0.10);
}

.pngshot-title {
  color: #f6f7fb;
  font-size: 17px;
  font-weight: 700;
}
.pngshot-eyebrow {
  color: #8ea9ff;
  font-size: 11px;
  font-weight: 700;
}
.pngshot-dim {
  color: rgba(232, 236, 245, 0.60);
  font-size: 12px;
}
.pngshot-caption {
  color: rgba(232, 236, 245, 0.45);
  font-size: 11px;
}
.pngshot-error {
  color: #ff8995;
}
.pngshot-success {
  color: #7ed9ad;
}
.pngshot-status-chip {
  background-color: rgba(142, 169, 255, 0.14);
  border: 1px solid rgba(142, 169, 255, 0.22);
  border-radius: 999px;
  color: #b9c8ff;
  padding: 4px 9px;
  font-size: 11px;
  font-weight: 600;
}
.pngshot-status-chip.pngshot-error {
  background-color: rgba(255, 137, 149, 0.13);
  border-color: rgba(255, 137, 149, 0.28);
  color: #ffadb6;
}
.pngshot-live-dot {
  color: #7ed9ad;
  font-size: 12px;
}

/* ---- preview and text surfaces ------------------------------------ */
.pngshot-preview {
  background-color: rgba(8, 10, 14, 0.72);
  border: 1px solid rgba(255, 255, 255, 0.12);
  border-radius: 12px;
  padding: 5px;
}
.pngshot-card.pngshot-alert {
  border-color: rgba(255, 137, 149, 0.82);
  box-shadow: 0 3px 10px rgba(0, 0, 0, 0.30),
              0 18px 42px rgba(0, 0, 0, 0.42),
              0 0 0 1px rgba(255, 137, 149, 0.20);
}
.pngshot-text-shell {
  background-color: rgba(10, 12, 17, 0.74);
  border: 1px solid rgba(255, 255, 255, 0.12);
  border-radius: 13px;
  padding: 1px;
}
.pngshot-textview {
  background-color: transparent;
  color: #eef1f7;
  font-size: 14px;
}
.pngshot-textview text {
  background-color: transparent;
  color: #eef1f7;
  padding: 14px 16px;
  line-height: 1.5;
}
.pngshot-textview text selection {
  background-color: rgba(101, 132, 232, 0.60);
  color: #ffffff;
}
.pngshot-divider {
  background-color: rgba(255, 255, 255, 0.10);
  min-height: 1px;
}

/* ---- action controls ------------------------------------------------ */
.pngshot-window button,
.pngshot-card button {
  min-height: 34px;
  border-radius: 10px;
  padding: 7px 14px;
  border: 1px solid rgba(255, 255, 255, 0.12);
  background-image: none;
  background-color: rgba(255, 255, 255, 0.065);
  color: #edf0f6;
  font-weight: 600;
  box-shadow: none;
  text-shadow: none;
}
.pngshot-window button:hover,
.pngshot-card button:hover {
  background-color: rgba(255, 255, 255, 0.13);
  border-color: rgba(255, 255, 255, 0.20);
}
.pngshot-window button:active,
.pngshot-card button:active {
  background-color: rgba(255, 255, 255, 0.18);
}
.pngshot-window button:focus,
.pngshot-card button:focus {
  outline: none;
  border-color: rgba(142, 169, 255, 0.86);
}
.pngshot-window button.suggested-action,
.pngshot-card button.suggested-action {
  background-color: #6484e8;
  border-color: #6484e8;
  color: #ffffff;
}
.pngshot-window button.suggested-action:hover,
.pngshot-card button.suggested-action:hover {
  background-color: #7594f2;
  border-color: #7594f2;
}
.pngshot-window button.suggested-action:active,
.pngshot-card button.suggested-action:active {
  background-color: #526fc8;
  border-color: #526fc8;
}
.pngshot-window button.pngshot-quiet,
.pngshot-card button.pngshot-quiet {
  background-color: transparent;
  border-color: transparent;
  color: rgba(232, 236, 245, 0.64);
}
.pngshot-window button.pngshot-quiet:hover,
.pngshot-card button.pngshot-quiet:hover {
  background-color: rgba(255, 137, 149, 0.14);
  border-color: rgba(255, 137, 149, 0.28);
  color: #ffadb6;
}
.pngshot-icon-button {
  min-width: 34px;
  padding-left: 8px;
  padding-right: 8px;
}
.pngshot-footer {
  padding-top: 2px;
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
