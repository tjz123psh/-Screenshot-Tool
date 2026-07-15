"""Long-screenshot recorder.

Wayland forbids synthetic scroll events, so long-shot is *semi-automatic*:
the user scrolls the target window manually while we sample the selected
region on a timer and feed frames to the Stitcher.

Design goals (after first-round usability feedback):
  - DON'T steal the keyboard. The control panel uses ON_DEMAND keyboard mode
    and anchors to a screen corner, so the target window keeps focus and the
    user can scroll / PageDown / space-to-scroll it normally.
  - Give live feedback. A shrinking preview thumbnail of the stitched result
    grows as the user scrolls, so they are never scrolling blind, plus a
    height / frame-count readout.
  - Make the primary actions clickable buttons (完成 / 取消), not hidden
    keyboard shortcuts — clicking a button doesn't fight the target window
    for keyboard focus.
  - Loud, sticky warning when overlap confidence drops (scroll back a little)
    instead of a one-frame flash.

Flow:
  1. The overlay hands us the fixed screen rect to sample.
  2. We show a small always-on-top control panel anchored bottom-center.
  3. A *background thread* grabs the region back-to-back as fast as grim allows
     (~25 fps) and hands each frame to the main thread via ``GLib.idle_add``.
     The stitcher runs on the main thread only.
  4. The preview + readout update live; low-overlap flips the panel red.
  5. 完成 button (or Enter, when the panel happens to have focus) stitches and
     returns the tall image; 取消 (or Esc) aborts.

Why a thread instead of a GLib timer: a single grim grab blocks ~36 ms, so a
timer fast enough to keep frames overlapping (needs <=~50 ms spacing) would
stall the GTK main loop and freeze the UI. Grabbing on a worker thread keeps
the main loop responsive while still sampling ~4x faster than the old 200 ms
timer, which is what made "overlap too low" trigger on the slightest scroll.
"""
from __future__ import annotations

import threading
import time
from typing import Callable

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gtk4LayerShell", "1.0")
gi.require_version("Gdk", "4.0")
gi.require_version("GdkPixbuf", "2.0")

from gi.repository import Gdk, GdkPixbuf, GLib, Gtk, Gtk4LayerShell  # noqa: E402
from PIL import Image  # noqa: E402

from .. import capture
from ..config import LongshotConfig
from ..overlay.model import Rect
from .stitcher import Stitcher

# Callback fired when recording ends. (image | None, warnings)
DoneCallback = Callable[["Image.Image | None", list], None]

PREVIEW_W = 220
PREVIEW_H = 300

# Set PNGSHOT_LONGSHOT_DEBUG=1 to print per-frame score/shift to the terminal.
import os  # noqa: E402

_DEBUG = os.environ.get("PNGSHOT_LONGSHOT_DEBUG") == "1"


class LongshotRecorder:
    def __init__(self, app: Gtk.Application, rect: Rect, cfg: LongshotConfig,
                 on_done: DoneCallback,
                 screen_size: tuple[int, int] | None = None) -> None:
        self.app = app
        self.rect = rect
        self.cfg = cfg
        self.on_done = on_done
        # Screen dimensions let us anchor the control panel on the side of the
        # screen the selection does NOT cover, so grim never captures the panel
        # into the stitched image (the panel is a layer-shell overlay that would
        # otherwise sit on top of the sampled region).
        self.screen_size = screen_size

        self.stitcher = Stitcher(
            max_diff=cfg.max_diff,
            min_shift_px=cfg.min_shift_px,
        )
        self._sampling = True
        self._last_diff = 0.0
        self._captured_height = rect.h
        # None | "low_overlap" | "no_move" — drives the status hint colour/text
        self._hint: str | None = None
        self._finished = False

        # Background capture: grim takes ~36 ms per grab, which would freeze the
        # GTK main loop if run on it at a high rate. Instead a worker thread
        # grabs frames back-to-back (~20 fps) and hands the latest one to the
        # main thread via GLib.idle_add. The higher rate keeps consecutive
        # frames overlapping so "重叠不足" stops firing on a normal scroll.
        self._capture_thread: threading.Thread | None = None
        self._pending_frame: Image.Image | None = None  # main-thread only
        self._pending_lock = threading.Lock()
        self._idle_queued = False

        self._build_panel()

    # ------------------------------------------------------------------

    def _build_panel(self) -> None:
        self.window = Gtk.ApplicationWindow(application=self.app)
        self.window.set_decorated(False)
        Gtk4LayerShell.init_for_window(self.window)
        Gtk4LayerShell.set_layer(self.window, Gtk4LayerShell.Layer.OVERLAY)
        Gtk4LayerShell.set_namespace(self.window, "pngshot-longshot")
        # ON_DEMAND: we only get the keyboard if the user actually clicks the
        # panel. The target window keeps focus so scrolling / PageDown work.
        Gtk4LayerShell.set_keyboard_mode(
            self.window, Gtk4LayerShell.KeyboardMode.ON_DEMAND
        )
        # Anchor the panel on whichever screen edge the selection does NOT
        # cover, so grim (which grabs the selection rect) never captures the
        # panel into the stitched image. Falls back to bottom-center when we
        # don't know the screen size or the selection fills the screen.
        self._apply_panel_anchor()

        # Shared rounded-card theme (see util/theme.py).
        from ..util import theme
        theme.apply(self.window)
        # The panel floats over the desktop as a layer-shell overlay, so the
        # window itself must be transparent — only the rounded .pngshot-card
        # (plus its shadow) should show. Otherwise GTK's default window
        # background paints a solid rectangle in the 16px margin around the
        # card's rounded corners.
        self.window.add_css_class("pngshot-transparent")

        self.root = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10)
        self.root.add_css_class("pngshot-card")
        self.root.set_margin_top(16)
        self.root.set_margin_bottom(16)
        self.root.set_margin_start(16)
        self.root.set_margin_end(16)

        # live preview of the stitched result
        self.preview = Gtk.Picture()
        self.preview.set_size_request(PREVIEW_W, PREVIEW_H)
        self.preview.set_content_fit(Gtk.ContentFit.CONTAIN)
        frame = Gtk.Frame()
        frame.add_css_class("pngshot-preview")
        frame.set_child(self.preview)
        self.root.append(frame)

        # readout
        self.status = Gtk.Label()
        self.status.set_wrap(True)
        self.status.set_max_width_chars(28)
        self.status.set_xalign(0.0)
        self.root.append(self.status)

        # instructions
        hint = Gtk.Label(label="向上或向下滚动目标窗口，预览会实时增长")
        hint.add_css_class("pngshot-dim")
        hint.set_wrap(True)
        hint.set_max_width_chars(28)
        hint.set_xalign(0.0)
        self.root.append(hint)

        # buttons
        btnbox = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=10)
        btnbox.set_homogeneous(True)
        btnbox.set_margin_top(2)
        cancel_btn = Gtk.Button(label="取消 (Esc)")
        cancel_btn.add_css_class("pngshot-quiet")
        cancel_btn.connect("clicked", lambda _b: self._finish(cancel=True))
        done_btn = Gtk.Button(label="完成 (Enter)")
        done_btn.add_css_class("suggested-action")
        done_btn.connect("clicked", lambda _b: self._finish(cancel=False))
        btnbox.append(cancel_btn)
        btnbox.append(done_btn)
        self.root.append(btnbox)

        self.window.set_child(self.root)

        key = Gtk.EventControllerKey.new()
        key.connect("key-pressed", self._on_key)
        self.window.add_controller(key)

        self._update_status()

    def _apply_panel_anchor(self) -> None:
        """Pin the panel to the largest screen gap outside the selection.

        grim samples ``self.rect``; anything the panel overlaps there lands in
        the stitched image. We measure the free margin on each side of the
        selection and anchor the panel against the roomiest one. If we don't
        know the screen size, or the selection leaves no usable gap on any side
        (it effectively fills the screen), we fall back to bottom-center — the
        old behaviour — and accept that a full-screen selection can't avoid it.
        """
        Edge = Gtk4LayerShell.Edge
        if not self.screen_size:
            Gtk4LayerShell.set_anchor(self.window, Edge.BOTTOM, True)
            Gtk4LayerShell.set_margin(self.window, Edge.BOTTOM, 48)
            return

        sw, sh = self.screen_size
        r = self.rect
        margin = 24
        # A side is only usable if the panel fits *entirely* in the gap there,
        # otherwise the panel would still poke into the sampled rect. Horizontal
        # sides must clear the panel's width, vertical sides its height. These
        # are conservative estimates of the built panel's footprint (preview +
        # readout + hint + buttons + margins).
        panel_w = PREVIEW_W + 60      # ~250 in practice
        panel_h = PREVIEW_H + 220     # ~490 in practice
        # (side -> free space, room the panel needs on that side)
        sides = {
            "top": (r.y, panel_h),
            "bottom": (sh - (r.y + r.h), panel_h),
            "left": (r.x, panel_w),
            "right": (sw - (r.x + r.w), panel_w),
        }
        usable = {k: gap for k, (gap, need) in sides.items()
                  if gap >= need + margin}
        if not usable:
            # selection fills the screen; nothing we do avoids overlap, so keep
            # it bottom-center and let the user reposition/scroll if needed.
            Gtk4LayerShell.set_anchor(self.window, Edge.BOTTOM, True)
            Gtk4LayerShell.set_margin(self.window, Edge.BOTTOM, 48)
            return

        edge_map = {
            "top": Edge.TOP,
            "bottom": Edge.BOTTOM,
            "left": Edge.LEFT,
            "right": Edge.RIGHT,
        }
        side = max(usable, key=lambda k: usable[k])
        Gtk4LayerShell.set_anchor(self.window, edge_map[side], True)
        Gtk4LayerShell.set_margin(self.window, edge_map[side], margin)

    def present(self) -> None:
        self.window.present()
        # Delay the worker start so the Stage-1 overlay is fully gone and we
        # don't capture our own dimming as frame #1.
        GLib.timeout_add(300, self._start_capture)

    def _start_capture(self) -> bool:
        if self._capture_thread is None:
            self._capture_thread = threading.Thread(
                target=self._capture_loop, name="longshot-capture", daemon=True
            )
            self._capture_thread.start()
        return False  # one-shot

    # ------------------------------------------------------------------

    def _capture_loop(self) -> None:
        """Worker thread: grab the region back-to-back and hand off the newest.

        Runs off the GTK main loop so grim's ~36 ms latency never blocks the
        UI. Only the *latest* frame is kept pending; if the main thread hasn't
        consumed the previous one yet we overwrite it, so stitching naturally
        samples at whatever rate it can keep up with instead of building a
        backlog. ``poll_ms`` becomes a small inter-grab pause (a floor on the
        rate) rather than the sampling period.
        """
        r = self.rect
        pause = max(self.cfg.poll_ms, 0) / 1000.0
        while self._sampling:
            try:
                frame = capture.grab_region(r.x, r.y, r.w, r.h)
            except capture.CaptureError as e:
                if _DEBUG:
                    print(f"[longshot] grab failed: {e}")
                if not self._sampling:
                    break
                time.sleep(0.1)
                continue
            with self._pending_lock:
                self._pending_frame = frame
                if not self._idle_queued:
                    self._idle_queued = True
                    GLib.idle_add(self._consume_pending)
            if pause:
                time.sleep(pause)

    def _consume_pending(self) -> bool:
        """Main thread: stitch the newest captured frame (if any)."""
        with self._pending_lock:
            frame = self._pending_frame
            self._pending_frame = None
            self._idle_queued = False
        if frame is None or not self._sampling:
            return False
        self._process_frame(frame)
        return False  # one-shot; the worker re-queues us for the next frame

    def _process_frame(self, frame: Image.Image) -> None:
        prev_frames = self.stitcher.frames_used
        self.stitcher.add(frame)
        new_h = self.stitcher.current_height() or self.rect.h
        grew = self.stitcher.frames_used != prev_frames
        first = prev_frames == 0

        if _DEBUG:
            print(
                f"[longshot] frame#{self.stitcher.frames_used} "
                f"diff={self.stitcher.last_diff:.3f} "
                f"shift={self.stitcher.last_shift:+d}px "
                f"added={self.stitcher.last_added}px grew={grew}"
            )

        # Classify *why* a frame was not appended so the panel can give the
        # right advice. After the first frame, a non-growing sample is either
        #   - high diff  -> the views don't overlap: user scrolled too fast
        #   - small shift -> the view barely moved: user hasn't scrolled yet
        # (diff is a mean signature difference; LOWER is a better match.)
        if not first and not grew:
            if self.stitcher.last_diff > self.cfg.max_diff:
                self._hint = "low_overlap"
            else:
                self._hint = "no_move"
        else:
            self._hint = None
            if grew or first:
                # seed the preview on the very first frame so the panel isn't
                # blank before the user starts scrolling.
                self._captured_height = new_h
                self._refresh_preview()
        self._update_status()

    # ------------------------------------------------------------------

    def _refresh_preview(self) -> None:
        thumb = self.stitcher.preview_thumbnail(PREVIEW_W, PREVIEW_H)
        if thumb is None:
            return
        self.preview.set_pixbuf(_pil_to_pixbuf(thumb))

    def _update_status(self) -> None:
        tail = (
            f'已拼 {self._captured_height}px · '
            f'{self.stitcher.frames_used} 帧'
        )
        if self._hint == "low_overlap":
            self.root.add_css_class("pngshot-alert")
            self.status.set_markup(
                f'<span foreground="#ff7a85" weight="bold">⚠ 重叠不足，'
                f'向回滚一点再继续</span>\n<span foreground="#c8cdd8">{tail}</span>'
            )
        elif self._hint == "no_move":
            self.root.remove_css_class("pngshot-alert")
            self.status.set_markup(
                f'<span foreground="#ffd479" weight="bold">↑↓ 滚动目标窗口（上下均可）</span>'
                f'\n<span foreground="#c8cdd8">{tail}</span>'
            )
        else:
            self.root.remove_css_class("pngshot-alert")
            self.status.set_markup(
                f'<span foreground="#8ab4ff" weight="bold">● 采集中</span>  '
                f'<span foreground="#eef1f6">已拼 <b>{self._captured_height}px</b> · '
                f'{self.stitcher.frames_used} 帧</span>'
            )

    # ------------------------------------------------------------------

    def _on_key(self, _kc, keyval: int, _kc2: int, _state) -> bool:
        name = Gdk.keyval_name(keyval) or ""
        if name == "Escape":
            self._finish(cancel=True)
            return True
        if name in ("Return", "KP_Enter"):
            self._finish(cancel=False)
            return True
        return False

    def _finish(self, *, cancel: bool) -> None:
        if self._finished:
            return
        self._finished = True
        # Signal the worker to stop; it's a daemon so we don't hard-join on the
        # UI thread (a grim grab may be mid-flight). It exits on the next loop
        # check. Any late idle callback bails out because _sampling is False.
        self._sampling = False
        self.window.close()
        if cancel:
            self.on_done(None, [])
            return
        try:
            res = self.stitcher.result()
            self.on_done(res.image, res.warnings)
        except ValueError:
            self.on_done(None, ["no frames captured"])


# ---------------------------------------------------------------------------
# helpers

def _pil_to_pixbuf(img: Image.Image) -> GdkPixbuf.Pixbuf:
    img = img.convert("RGBA")
    w, h = img.size
    data = GLib.Bytes.new(img.tobytes())
    return GdkPixbuf.Pixbuf.new_from_bytes(
        data, GdkPixbuf.Colorspace.RGB, True, 8, w, h, w * 4
    )
