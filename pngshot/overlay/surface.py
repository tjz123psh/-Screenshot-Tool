"""Stage 1 overlay surface (layer-shell) — the heart of Step 2.

Owns:
  - a fullscreen wlr-layer-shell overlay (exclusive keyboard)
  - the background snapshot (grim)
  - the Selector (rect + interaction mode)
  - the Toolbar (buttons that hover below the selection)
  - Cairo rendering & pointer/keyboard dispatch

The window is intentionally single-file for now; annotation and long-shot
modes will grow out of the same DrawingArea in later steps.
"""
from __future__ import annotations

import io
from typing import Callable

import cairo
import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gtk4LayerShell", "1.0")
gi.require_version("Gdk", "4.0")
gi.require_version("Pango", "1.0")
gi.require_version("PangoCairo", "1.0")

from gi.repository import Gdk, GLib, Gtk, Gtk4LayerShell, Pango, PangoCairo  # noqa: E402
from PIL import Image  # noqa: E402

from .annotate import Annotator, PALETTE, WIDTHS
from .model import HitKind, Mode, Rect, handle_positions
from .selector import Selector
from .toolbar import Toolbar, ToolbarButton

# Callback: fires once user picks a terminal action.
# Signature: (action: str, cropped: PIL.Image | None, rect: Rect | None) -> None
#   action in {"confirm","pin","ocr","translate","annotate","long","cancel"}
# cropped is the selection-cropped PIL image (None for cancel).
# rect is the selection in screen coordinates (None for cancel); long-shot
# needs the live screen rect to re-sample after the overlay closes.
ResultCallback = Callable[[str, "Image.Image | None", "Rect | None"], None]


from ..util.imaging import pil_to_cairo_surface as _pil_to_cairo_surface  # noqa: E402


class OverlaySurface:
    def __init__(self, app: Gtk.Application, background: Image.Image,
                 on_result: ResultCallback, *, long_shot: bool = False) -> None:
        self.app = app
        self.bg = background
        self.bg_surface, self._bg_buffer = _pil_to_cairo_surface(background)
        self.screen_w, self.screen_h = background.size
        self.on_result = on_result
        # In long-shot mode the user launched us specifically to scroll-capture,
        # so finishing a selection should start immediately rather than making
        # them hunt for the "长截图" toolbar button. The toolbar is hidden and a
        # completed drag fires the "long" action straight away.
        self.long_shot = long_shot

        self.selector = Selector(self.screen_w, self.screen_h)
        self.toolbar = Toolbar()

        # Annotation sub-mode (Step 5). When active, the region toolbar is
        # replaced by the annotate toolbar and pointer events draw strokes.
        from .toolbar import ANNOTATE_BUTTONS
        self.annotator = Annotator()
        self.annotate_toolbar = Toolbar(ANNOTATE_BUTTONS)
        self.annotating = False
        self._annotation_popup: str | None = None
        self._annotation_popup_hover = -1

        self.window = Gtk.ApplicationWindow(application=app)
        self.window.set_decorated(False)
        self.window.set_default_size(self.screen_w, self.screen_h)

        # Layer-shell setup
        Gtk4LayerShell.init_for_window(self.window)
        Gtk4LayerShell.set_layer(self.window, Gtk4LayerShell.Layer.OVERLAY)
        Gtk4LayerShell.set_namespace(self.window, "pngshot-overlay")
        Gtk4LayerShell.set_keyboard_mode(self.window, Gtk4LayerShell.KeyboardMode.EXCLUSIVE)
        for edge in (Gtk4LayerShell.Edge.TOP, Gtk4LayerShell.Edge.BOTTOM,
                     Gtk4LayerShell.Edge.LEFT, Gtk4LayerShell.Edge.RIGHT):
            Gtk4LayerShell.set_anchor(self.window, edge, True)
        Gtk4LayerShell.set_exclusive_zone(self.window, -1)  # ignore other exclusive zones

        # Drawing area fills the whole window
        self.canvas = Gtk.DrawingArea()
        self.canvas.set_draw_func(self._on_draw)
        self.canvas.set_hexpand(True)
        self.canvas.set_vexpand(True)
        self.window.set_child(self.canvas)

        self._install_controllers()

        # Hover state (for toolbar hover highlight)
        self._hover_button: str = ""

        # Escape-valve state. A fullscreen layer-shell overlay with EXCLUSIVE
        # keyboard can be orphaned: if it maps on a workspace the user isn't on
        # (or the compositor never routes input to it), no Escape ever reaches
        # us and app.run() blocks forever in poll, leaving a zombie that later
        # launches forward-activate into (yanking the user back to it). The
        # guards below guarantee the overlay always resolves WITHOUT ever
        # killing an actively-used one.
        #
        # We drive the idle detection off real INPUT ACTIVITY (pointer / key),
        # NOT GTK keyboard-focus events: under niri a layer-shell surface's
        # focus enter/leave is unreliable, so an earlier focus-only scheme
        # wrongly classified mouse-only annotation as "never engaged" and
        # cancelled it mid-draw. A monotonic timestamp updated on every real
        # event is compositor-agnostic.
        #   _finished     : one-shot latch so a result fires exactly once
        #   _last_activity: monotonic µs of the last real pointer/key event;
        #                   the idle check measures silence against it
        #   _ever_focused : set by ANY real input (kept for the focus-loss
        #                   grace path, which still helps on compositors where
        #                   focus events DO fire)
        #   _grace_source : GLib source id of a pending focus-loss grace timer,
        #                   so re-engaging can cancel it. None when idle.
        self._finished = False
        self._last_activity = GLib.get_monotonic_time()
        self._ever_focused = False
        self._grace_source: int | None = None

    # ------------------------------------------------------------------
    # public

    # Idle window: cancel only after this many seconds of TOTAL input silence
    # (no pointer, no keys). Long enough that a user pausing to think mid-
    # selection is never interrupted; the check also fully exempts annotation.
    _IDLE_TIMEOUT_S = 45
    # How often the idle watchdog wakes to compare now vs. last activity.
    _IDLE_POLL_S = 5

    def present(self) -> None:
        self.window.present()
        # Escape valve for an orphaned overlay (mapped where the user can't see
        # or reach it, so no Escape ever arrives). A recurring watchdog cancels
        # only after _IDLE_TIMEOUT_S of TOTAL input silence. Crucially it keys
        # off real pointer/key activity (see _mark_activity), NOT GTK focus
        # events, which are unreliable for layer-shell surfaces under niri and
        # previously caused mouse-only annotation to be cancelled mid-draw.
        self._last_activity = GLib.get_monotonic_time()
        GLib.timeout_add_seconds(self._IDLE_POLL_S, self._on_idle_tick)

    def _mark_activity(self) -> None:
        """Record real user input so the idle watchdog never fires on an
        actively-used overlay. Called from every pointer/key handler."""
        self._last_activity = GLib.get_monotonic_time()
        self._ever_focused = True
        # Any real input also proves the user is present, so drop a pending
        # focus-loss grace timer (a focus event may have started it spuriously).
        if self._grace_source is not None:
            GLib.source_remove(self._grace_source)
            self._grace_source = None

    def _on_focus_enter(self, _fc: "Gtk.EventControllerFocus") -> None:
        # Treated the same as any activity; harmless if it never fires.
        self._mark_activity()

    def _on_focus_leave(self, _fc: "Gtk.EventControllerFocus") -> None:
        # On compositors where focus events DO fire, losing focus after we held
        # it hints the user switched away. Never act on it mid-annotation, and
        # never cancel immediately — a brief flick is common. Start a grace
        # timer; any re-engagement (_mark_activity) cancels it. The activity-
        # based idle watchdog is the primary guard; this is a faster secondary
        # one where focus tracking happens to work.
        if not self._ever_focused or self._finished or self.annotating:
            return
        if self._grace_source is None:
            self._grace_source = GLib.timeout_add_seconds(
                10, self._on_focus_grace_expired
            )

    def _on_focus_grace_expired(self) -> bool:
        # Focus lost 10 s ago with no re-engagement: the user walked away.
        self._grace_source = None
        if not self._finished and not self.annotating:
            self._emit("cancel", None, None)
        return False  # one-shot

    def _on_idle_tick(self) -> bool:
        # Recurring watchdog. Never touches an overlay that is being annotated,
        # and only cancels after _IDLE_TIMEOUT_S of complete input silence, so
        # an actively-used overlay is safe regardless of focus-event quirks.
        if self._finished:
            return False  # stop the watchdog
        if self.annotating:
            return True  # annotation is exempt; keep watching for after it ends
        idle_s = (GLib.get_monotonic_time() - self._last_activity) / 1_000_000
        if idle_s >= self._IDLE_TIMEOUT_S:
            self._emit("cancel", None, None)
            return False  # stop
        return True  # keep polling

    # ------------------------------------------------------------------
    # event wiring

    def _install_controllers(self) -> None:
        # pointer press/release
        gc = Gtk.GestureClick.new()
        gc.set_button(0)  # any button
        gc.connect("pressed", self._on_pressed)
        gc.connect("released", self._on_released)
        self.canvas.add_controller(gc)

        # motion
        mc = Gtk.EventControllerMotion.new()
        mc.connect("motion", self._on_motion)
        self.canvas.add_controller(mc)

        # keyboard
        kc = Gtk.EventControllerKey.new()
        kc.connect("key-pressed", self._on_key)
        self.window.add_controller(kc)

        # focus — drives the orphan-overlay escape valve. "enter"/"leave" fire
        # when the window gains/loses keyboard focus; combined with _ever_focused
        # this lets us auto-cancel when the user switches away mid-selection.
        fc = Gtk.EventControllerFocus.new()
        fc.connect("enter", self._on_focus_enter)
        fc.connect("leave", self._on_focus_leave)
        self.window.add_controller(fc)

    # ------------------------------------------------------------------
    # pointer events

    def _on_pressed(self, gesture: Gtk.GestureClick, n_press: int, x: float, y: float) -> None:
        self._mark_activity()
        button = gesture.get_current_button()

        # ---- annotation sub-mode ----
        if self.annotating:
            if button != Gdk.BUTTON_PRIMARY:
                return
            if self._annotation_popup is not None:
                option = self._annotation_popup_hit_test(x, y)
                if option >= 0:
                    self._apply_annotation_popup_option(option)
                else:
                    btn = self.annotate_toolbar.hit_test(
                        x, y, self.selector.rect, self.screen_w, self.screen_h
                    )
                    if btn is not None:
                        self._invoke_annotate_button(btn)
                    else:
                        self._close_annotation_popup()
                return
            btn = self.annotate_toolbar.hit_test(x, y, self.selector.rect,
                                                 self.screen_w, self.screen_h)
            if btn is not None:
                self._invoke_annotate_button(btn)
                return
            # only draw inside the selection rect
            if self.selector.rect.contains(x, y):
                self.annotator.press(x, y)
                self.canvas.queue_draw()
            return

        if button == Gdk.BUTTON_SECONDARY:
            # right-click clears the current selection
            self.selector.clear()
            self.canvas.queue_draw()
            return
        if button != Gdk.BUTTON_PRIMARY:
            return

        # toolbar buttons only respond if a selection exists
        if self.selector.mode is Mode.HAS_SELECTION:
            btn = self.toolbar.hit_test(x, y, self.selector.rect,
                                        self.screen_w, self.screen_h)
            if btn is not None:
                self._invoke_button(btn)
                return

        self.selector.press(x, y)
        self.canvas.queue_draw()

    def _on_motion(self, _mc: Gtk.EventControllerMotion, x: float, y: float) -> None:
        self._mark_activity()
        # ---- annotation sub-mode ----
        if self.annotating:
            if self._annotation_popup is not None:
                previous = self._annotation_popup_hover
                self._annotation_popup_hover = self._annotation_popup_hit_test(x, y)
                if previous != self._annotation_popup_hover:
                    self.canvas.queue_draw()
                return
            prev_hover = self._hover_button
            self._hover_button = ""
            btn = self.annotate_toolbar.hit_test(x, y, self.selector.rect,
                                                 self.screen_w, self.screen_h)
            if btn is not None:
                self._hover_button = btn.id
            self.annotator.motion(x, y)
            self.canvas.queue_draw()
            return

        # update hover (toolbar highlight) only when idle-ish
        prev_hover = self._hover_button
        self._hover_button = ""
        if self.selector.mode is Mode.HAS_SELECTION:
            btn = self.toolbar.hit_test(x, y, self.selector.rect,
                                        self.screen_w, self.screen_h)
            if btn is not None:
                self._hover_button = btn.id

        if self.selector.mode in (Mode.SELECTING, Mode.MOVING, Mode.RESIZING):
            self.selector.motion(x, y)
            self.canvas.queue_draw()
        elif prev_hover != self._hover_button:
            self.canvas.queue_draw()

    def _on_released(self, gesture: Gtk.GestureClick, n_press: int, x: float, y: float) -> None:
        self._mark_activity()
        if gesture.get_current_button() != Gdk.BUTTON_PRIMARY:
            return
        if self.annotating:
            if self._annotation_popup is not None:
                return
            self.annotator.release(x, y)
            self.canvas.queue_draw()
            return
        was_selecting = self.selector.mode is Mode.SELECTING
        self.selector.release()
        self.canvas.queue_draw()
        # Long-shot mode: finishing a fresh selection starts capture straight
        # away, so the user never has to reach for the 长截图 toolbar button.
        # Only auto-start on a newly-drawn selection (SELECTING); a plain click
        # or a move/resize of an existing rect shouldn't fire it.
        if self.long_shot and was_selecting and self.selector.rect.valid:
            self._invoke_action("long")

    # ------------------------------------------------------------------
    # keyboard

    def _on_key(self, _kc: Gtk.EventControllerKey, keyval: int, _keycode: int,
                state: Gdk.ModifierType) -> bool:
        self._mark_activity()
        name = Gdk.keyval_name(keyval) or ""

        # ---- annotation sub-mode ----
        if self.annotating:
            return self._on_key_annotate(name, keyval, state)

        if name in ("Escape",):
            self._invoke_action("cancel")
            return True
        if not self.selector.rect.valid:
            return False
        if name in ("Return", "KP_Enter"):
            self._invoke_action("confirm")
            return True
        for btn in self.toolbar.buttons:
            if name.lower() == btn.hotkey.lower():
                self._invoke_button(btn)
                return True
        return False

    def _on_key_annotate(self, name: str, keyval: int,
                         state: Gdk.ModifierType) -> bool:
        # If a text tool edit is active, typing goes into the text box.
        editing = self.annotator.editing_text is not None
        if editing:
            if name == "Escape":
                self.annotator.commit_text()
                self.canvas.queue_draw()
                return True
            if name in ("Return", "KP_Enter"):
                self.annotator.commit_text()
                self.canvas.queue_draw()
                return True
            if name == "BackSpace":
                self.annotator.backspace()
                self.canvas.queue_draw()
                return True
            ch = chr(Gdk.keyval_to_unicode(keyval)) if Gdk.keyval_to_unicode(keyval) else ""
            if ch and ch.isprintable():
                self.annotator.type_char(ch)
                self.canvas.queue_draw()
                return True
            return True  # swallow everything else while editing

        # Not editing text — hotkeys drive tools / actions.
        if name == "Escape":
            if self._annotation_popup is not None:
                self._close_annotation_popup()
                return True
            # leave annotation without applying
            self._exit_annotate(apply=False)
            return True
        if name in ("Return", "KP_Enter"):
            self._exit_annotate(apply=True)
            return True
        ctrl = bool(state & Gdk.ModifierType.CONTROL_MASK)
        if ctrl and name.lower() == "z":
            self.annotator.undo()
            self.canvas.queue_draw()
            return True
        for btn in self.annotate_toolbar.buttons:
            if btn.hotkey and name.lower() == btn.hotkey.lower():
                self._invoke_annotate_button(btn)
                return True
        return True  # annotation mode swallows stray keys

    # ------------------------------------------------------------------
    # actions

    def _invoke_button(self, btn: ToolbarButton) -> None:
        self._invoke_action(btn.id)

    def _invoke_action(self, action: str) -> None:
        if action == "cancel":
            self._emit("cancel", None, None)
            return
        if not self.selector.rect.valid:
            return
        if action == "annotate":
            self._enter_annotate()
            return
        r = self.selector.rect
        cropped = self.bg.crop((r.x, r.y, r.x2, r.y2))
        self._emit(action, cropped, r)

    def _emit(self, action: str, cropped: "Image.Image | None",
              rect: "Rect | None") -> None:
        """Single, idempotent exit for every result path.

        Both user actions and the focus-loss / timeout escape valves funnel
        through here, so a result fires exactly once even if, say, focus-loss
        and a late click race. Without the latch the overlay could call
        on_result twice (double-processing a crop, or cancelling a valid one).
        """
        if self._finished:
            return
        self._finished = True
        # Cancel any pending focus-loss grace timer so it can't fire after we've
        # already resolved (it would be a no-op via the latch, but leaving a live
        # GLib source around is untidy and keeps a ref to self).
        if self._grace_source is not None:
            GLib.source_remove(self._grace_source)
            self._grace_source = None
        self.on_result(action, cropped, rect)

    # ------------------------------------------------------------------
    # annotation sub-mode

    def _enter_annotate(self) -> None:
        self.annotating = True
        self._annotation_popup = None
        self._annotation_popup_hover = -1
        self.annotator.begin_canvas(self.selector.rect)
        self._hover_button = ""
        self.canvas.queue_draw()

    def _exit_annotate(self, *, apply: bool) -> None:
        self.annotating = False
        self._annotation_popup = None
        self._annotation_popup_hover = -1
        self._hover_button = ""
        if not apply or not self.annotator.has_content():
            # nothing to bake — just return to the region toolbar
            self.canvas.queue_draw()
            return
        # bake strokes into the crop and finish via the normal pipeline
        r = self.selector.rect
        baked = self.annotator.bake(self.bg_surface, r)
        img = _cairo_surface_to_pil(baked)
        self._emit("confirm", img, r)

    def _annotate_active_id(self) -> str:
        """Which annotate-toolbar button should show a selected state."""
        return f"tool.{self.annotator.tool}"

    def _invoke_annotate_button(self, btn: ToolbarButton) -> None:
        bid = btn.id
        if bid == "anno.done":
            self._exit_annotate(apply=True)
        elif bid == "anno.undo":
            self.annotator.undo()
            self.canvas.queue_draw()
        elif bid == "anno.color":
            self._toggle_annotation_popup("color")
        elif bid == "anno.width":
            self._toggle_annotation_popup("width")
        elif bid.startswith("tool."):
            self.annotator.set_tool(bid[len("tool."):])
            self.canvas.queue_draw()

    # ------------------------------------------------------------------
    # annotation colour/width popup

    def _annotation_popup_layout(
        self,
    ) -> tuple[
        tuple[float, float, float, float],
        list[tuple[float, float, float, float]],
    ]:
        """Return popup and option rectangles in screen coordinates."""
        kind = self._annotation_popup
        if kind not in ("color", "width"):
            return (0, 0, 0, 0), []

        r = self.selector.rect
        self.annotate_toolbar.layout(r, self.screen_w, self.screen_h)
        button = next(
            b for b in self.annotate_toolbar.buttons if b.id == f"anno.{kind}"
        )

        count = len(PALETTE) if kind == "color" else len(WIDTHS)
        item_w = 34 if kind == "color" else 64
        item_h = 34 if kind == "color" else 42
        pad_x, pad_y, gap = 10, 9, 7
        popup_w = pad_x * 2 + count * item_w + (count - 1) * gap
        popup_h = pad_y * 2 + item_h
        popup_x = button.x + button.w / 2 - popup_w / 2
        popup_x = max(8, min(popup_x, self.screen_w - popup_w - 8))

        gap_from_bar = 8
        _bx, by, _bw, bh = self.annotate_toolbar.bar_rect
        above_y = by - popup_h - gap_from_bar
        below_y = by + bh + gap_from_bar
        if above_y >= 8:
            popup_y = above_y
        elif below_y + popup_h <= self.screen_h - 8:
            popup_y = below_y
        else:
            popup_y = max(8, min(above_y, self.screen_h - popup_h - 8))

        options: list[tuple[float, float, float, float]] = []
        ox = popup_x + pad_x
        oy = popup_y + pad_y
        for _ in range(count):
            options.append((ox, oy, item_w, item_h))
            ox += item_w + gap
        return (popup_x, popup_y, popup_w, popup_h), options

    def _annotation_popup_hit_test(self, x: float, y: float) -> int:
        _popup, options = self._annotation_popup_layout()
        for idx, (ox, oy, ow, oh) in enumerate(options):
            if ox <= x < ox + ow and oy <= y < oy + oh:
                return idx
        return -1

    def _toggle_annotation_popup(self, kind: str) -> None:
        self._annotation_popup = None if self._annotation_popup == kind else kind
        self._annotation_popup_hover = -1
        self.canvas.queue_draw()

    def _close_annotation_popup(self) -> None:
        if self._annotation_popup is None:
            return
        self._annotation_popup = None
        self._annotation_popup_hover = -1
        self.canvas.queue_draw()

    def _apply_annotation_popup_option(self, option: int) -> None:
        if self._annotation_popup == "color" and 0 <= option < len(PALETTE):
            self.annotator.color_idx = option
        elif self._annotation_popup == "width" and 0 <= option < len(WIDTHS):
            self.annotator.width_idx = option
        self._close_annotation_popup()

    # ------------------------------------------------------------------
    # drawing

    def _on_draw(self, _da: Gtk.DrawingArea, ctx: cairo.Context, w: int, h: int) -> None:
        # 1) background snapshot
        ctx.set_source_surface(self.bg_surface, 0, 0)
        ctx.paint()

        # 2) dim overlay everywhere except inside the selection
        ctx.set_source_rgba(0.025, 0.03, 0.045, 0.56)
        r = self.selector.rect
        if r.valid:
            # dim by drawing four rectangles around the selection
            # top
            ctx.rectangle(0, 0, w, r.y)
            # bottom
            ctx.rectangle(0, r.y2, w, h - r.y2)
            # left
            ctx.rectangle(0, r.y, r.x, r.h)
            # right
            ctx.rectangle(r.x2, r.y, w - r.x2, r.h)
            ctx.fill()

            # 3) selection border
            ctx.set_source_rgba(0.39, 0.52, 0.91, 1.0)
            ctx.set_line_width(2)
            ctx.rectangle(r.x + 0.5, r.y + 0.5, r.w - 1, r.h - 1)
            ctx.stroke()

            # 4) size hint above the selection
            hint = f"{r.w} × {r.h}"
            self._draw_size_hint(ctx, hint, r)

            if self.annotating:
                # annotation sub-mode: strokes clipped to the selection, then
                # the drawing toolbar (no resize handles / size hint clutter).
                ctx.save()
                ctx.rectangle(r.x, r.y, r.w, r.h)
                ctx.clip()
                self.annotator.draw(ctx)
                ctx.restore()
                self.annotate_toolbar.draw(
                    ctx, r, self.screen_w, self.screen_h,
                    hover_id=self._hover_button,
                    active_id=self._annotate_active_id(),
                )
                self._draw_annotation_toolbar_values(ctx)
                self._draw_annotation_popup(ctx)
            else:
                # 5) handles
                self._draw_handles(ctx, r)
                # 6) toolbar (only when a stable selection exists). Hidden in
                # long-shot mode: finishing the drag starts capture directly, so
                # the toolbar would only flash and invite a needless extra click.
                if self.selector.mode is Mode.HAS_SELECTION and not self.long_shot:
                    self.toolbar.draw(ctx, r, self.screen_w, self.screen_h,
                                      hover_id=self._hover_button)
        else:
            ctx.rectangle(0, 0, w, h)
            ctx.fill()
            # help hint centered
            hint_text = (
                "拖动框选长截图区域，松手即开始  ·  Esc 取消"
                if self.long_shot
                else "拖动鼠标框选  ·  Esc 取消  ·  右键清除"
            )
            self._draw_center_hint(ctx, hint_text, w, h)

    # ---- draw helpers -----------------------------------------------------

    def _draw_handles(self, ctx: cairo.Context, r: Rect) -> None:
        size = 9
        for _name, hx, hy in handle_positions(r):
            ctx.arc(hx, hy, size / 2, 0, 2 * 3.141592653589793)
            ctx.set_source_rgba(0.96, 0.97, 1.0, 1.0)
            ctx.fill_preserve()
            ctx.set_source_rgba(0.39, 0.52, 0.91, 1.0)
            ctx.set_line_width(2)
            ctx.stroke()

    def _draw_size_hint(self, ctx: cairo.Context, text: str, r: Rect) -> None:
        tw, th = _pango_measure(ctx, text, 11)
        pad_x, pad_y = 9, 5
        bw = tw + pad_x * 2
        bh = th + pad_y * 2
        bx = r.x
        by = r.y - bh - 7
        if by < 0:
            by = r.y + 7
        _rounded_rect(ctx, bx, by, bw, bh, 7)
        ctx.set_source_rgba(0.09, 0.105, 0.14, 0.94)
        ctx.fill_preserve()
        ctx.set_source_rgba(0.76, 0.82, 0.96, 0.22)
        ctx.set_line_width(1)
        ctx.stroke()
        ctx.set_source_rgba(0.93, 0.95, 1.0, 1)
        _pango_draw(ctx, text, bx + pad_x, by + pad_y, 11)

    def _draw_annotation_popup(self, ctx: cairo.Context) -> None:
        if self._annotation_popup is None:
            return
        (px, py, pw, ph), options = self._annotation_popup_layout()
        _rounded_rect(ctx, px + 1, py + 3, pw, ph, 11)
        ctx.set_source_rgba(0, 0, 0, 0.30)
        ctx.fill()
        _rounded_rect(ctx, px, py, pw, ph, 11)
        ctx.set_source_rgba(0.075, 0.09, 0.125, 0.98)
        ctx.fill_preserve()
        ctx.set_source_rgba(0.76, 0.82, 0.96, 0.22)
        ctx.set_line_width(1)
        ctx.stroke()

        current = (
            self.annotator.color_idx
            if self._annotation_popup == "color"
            else self.annotator.width_idx
        )
        for idx, (ox, oy, ow, oh) in enumerate(options):
            hovered = idx == self._annotation_popup_hover
            selected = idx == current
            _rounded_rect(ctx, ox, oy, ow, oh, 8)
            if hovered:
                ctx.set_source_rgba(0.30, 0.39, 0.62, 0.76)
            elif selected:
                ctx.set_source_rgba(0.24, 0.33, 0.55, 0.70)
            else:
                ctx.set_source_rgba(1, 1, 1, 0.045)
            ctx.fill()

            if self._annotation_popup == "color":
                cr, cg, cb = PALETTE[idx]
                cx, cy = ox + ow / 2, oy + oh / 2
                ctx.arc(cx, cy, 9, 0, 2 * 3.141592653589793)
                ctx.set_source_rgb(cr, cg, cb)
                ctx.fill_preserve()
                ctx.set_source_rgba(1, 1, 1, 0.80 if selected else 0.24)
                ctx.set_line_width(2 if selected else 1)
                ctx.stroke()
                if selected:
                    ctx.set_source_rgb(1, 1, 1)
                    ctx.set_line_width(1.8)
                    ctx.move_to(cx - 4, cy)
                    ctx.line_to(cx - 1, cy + 3)
                    ctx.line_to(cx + 5, cy - 4)
                    ctx.stroke()
            else:
                width = WIDTHS[idx]
                ctx.set_source_rgb(*self.annotator.color)
                ctx.set_line_width(width)
                ctx.set_line_cap(cairo.LINE_CAP_ROUND)
                ctx.move_to(ox + 11, oy + oh / 2)
                ctx.line_to(ox + ow - 11, oy + oh / 2)
                ctx.stroke()
                ctx.set_line_cap(cairo.LINE_CAP_BUTT)
                label = f"{int(width)} px"
                tw, th = _pango_measure(ctx, label, 9)
                ctx.set_source_rgba(0.88, 0.91, 0.98, 0.76)
                _pango_draw(ctx, label, ox + (ow - tw) / 2, oy + oh - th - 3, 9)
            if selected:
                _rounded_rect(ctx, ox - 1, oy - 1, ow + 2, oh + 2, 9)
                ctx.set_source_rgba(0.48, 0.62, 1.0, 0.85)
                ctx.set_line_width(1.5)
                ctx.stroke()

    def _draw_annotation_toolbar_values(self, ctx: cairo.Context) -> None:
        """Show the current colour and width even while the popup is closed."""
        by_id = {button.id: button for button in self.annotate_toolbar.buttons}
        color_button = by_id["anno.color"]
        cx = color_button.x + 8
        cy = color_button.y + color_button.h - 7
        ctx.arc(cx, cy, 3.5, 0, 2 * 3.141592653589793)
        ctx.set_source_rgb(*self.annotator.color)
        ctx.fill_preserve()
        ctx.set_source_rgba(1, 1, 1, 0.55)
        ctx.set_line_width(1)
        ctx.stroke()

        width_button = by_id["anno.width"]
        ctx.set_source_rgb(*self.annotator.color)
        ctx.set_line_width(min(self.annotator.width, 4.5))
        ctx.set_line_cap(cairo.LINE_CAP_ROUND)
        ctx.move_to(width_button.x + 6, width_button.y + width_button.h - 7)
        ctx.line_to(width_button.x + 18, width_button.y + width_button.h - 7)
        ctx.stroke()
        ctx.set_line_cap(cairo.LINE_CAP_BUTT)

    def _draw_center_hint(self, ctx: cairo.Context, text: str, w: int, h: int) -> None:
        tw, th = _pango_measure(ctx, text, 13)
        pad_x, pad_y = 16, 10
        bw = tw + pad_x * 2
        bh = th + pad_y * 2
        bx = (w - bw) / 2
        by = h - bh - 40
        _rounded_rect(ctx, bx, by, bw, bh, 10)
        ctx.set_source_rgba(0.09, 0.105, 0.14, 0.92)
        ctx.fill_preserve()
        ctx.set_source_rgba(1, 1, 1, 0.14)
        ctx.set_line_width(1)
        ctx.stroke()
        ctx.set_source_rgba(0.93, 0.95, 1.0, 0.92)
        _pango_draw(ctx, text, bx + pad_x, by + pad_y, 13)


# ---------------------------------------------------------------------------
# Pango helpers (shared with Toolbar). Using Pango so CJK glyphs render via
# fontconfig instead of Cairo's toy font API (which drops non-ASCII).

def _pango_layout(ctx: cairo.Context, text: str, size_pt: float) -> Pango.Layout:
    layout = PangoCairo.create_layout(ctx)
    desc = Pango.FontDescription()
    desc.set_family("Sans")
    desc.set_absolute_size(size_pt * Pango.SCALE)
    layout.set_font_description(desc)
    layout.set_text(text, -1)
    return layout


def _pango_measure(ctx: cairo.Context, text: str, size_pt: float) -> tuple[float, float]:
    layout = _pango_layout(ctx, text, size_pt)
    w, h = layout.get_pixel_size()
    return float(w), float(h)


def _pango_draw(ctx: cairo.Context, text: str, x: float, y: float, size_pt: float) -> None:
    layout = _pango_layout(ctx, text, size_pt)
    ctx.move_to(x, y)
    PangoCairo.show_layout(ctx, layout)


def _rounded_rect(cr: cairo.Context, x: float, y: float,
                  w: float, h: float, radius: float) -> None:
    import math
    radius = min(radius, w / 2, h / 2)
    cr.new_sub_path()
    cr.arc(x + w - radius, y + radius, radius, -math.pi / 2, 0)
    cr.arc(x + w - radius, y + h - radius, radius, 0, math.pi / 2)
    cr.arc(x + radius, y + h - radius, radius, math.pi / 2, math.pi)
    cr.arc(x + radius, y + radius, radius, math.pi, 3 * math.pi / 2)
    cr.close_path()


def _cairo_surface_to_pil(surface: cairo.ImageSurface) -> Image.Image:
    """Cairo ARGB32 (BGRA, little-endian) -> PIL RGBA.

    Inverse of ``pil_to_cairo_surface``: reads the surface's raw bytes and
    swaps B/R back so the returned PIL image has correct colors.
    """
    surface.flush()
    w = surface.get_width()
    h = surface.get_height()
    stride = surface.get_stride()
    buf = bytes(surface.get_data())
    # drop row padding if present
    if stride != w * 4:
        rows = [buf[i * stride: i * stride + w * 4] for i in range(h)]
        buf = b"".join(rows)
    bgra = Image.frombuffer("RGBA", (w, h), buf, "raw", "RGBA", 0, 1)
    b, g, r, a = bgra.split()
    return Image.merge("RGBA", (r, g, b, a))
