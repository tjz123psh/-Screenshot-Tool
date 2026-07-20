"""Long-screenshot recorder.

Wayland forbids synthetic scroll events, so long-shot is *semi-automatic*:
the user scrolls the target window manually while we sample the selected
region on a timer and feed frames to the Stitcher.

Design goals (after first-round usability feedback):
  - DON'T steal the keyboard. The control panel uses ON_DEMAND keyboard mode
    and anchors to a screen corner, so the target window keeps focus and the
    user can scroll / PageDown / space-to-scroll it normally.
  - Give live feedback through a compact state + height readout while keeping
    the sampled window visible. The selected rectangle itself stays outlined.
  - Make the primary actions clickable buttons (完成 / 取消), not hidden
    keyboard shortcuts — clicking a button doesn't fight the target window
    for keyboard focus.
  - Treat short matching failures as recoverable sampling noise. The recorder
    keeps collecting and only asks the user to slow down after a sustained run
    of uncertain frames; it never requires an immediate rollback.

Flow:
  1. The overlay hands us the fixed screen rect to sample.
  2. We show a small always-on-top control panel anchored bottom-center.
  3. A *background thread* grabs the region back-to-back as fast as grim allows
     (~25 fps) and hands each frame to the main thread via ``GLib.idle_add``.
     The stitcher runs on the main thread only.
  4. The readout updates live; temporary low-overlap is retried using
     recent frame history instead of interrupting the user's scroll.
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
from collections import deque
from typing import Callable

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gtk4LayerShell", "1.0")
gi.require_version("Gdk", "4.0")

from gi.repository import Gdk, GLib, Gtk, Gtk4LayerShell  # noqa: E402
from PIL import Image  # noqa: E402

from .. import capture
from ..config import LongshotConfig
from ..overlay.model import Rect
from .highlight import SelectionHighlight
from .stitcher import Stitcher

# Callback fired when recording ends. (image | None, warnings)
DoneCallback = Callable[["Image.Image | None", list], None]

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
            preview=False,
        )
        self._sampling = True
        self._last_diff = 0.0
        self._captured_height = rect.h
        # None | "recovering" | "recovered" | "slow_down" | "no_move" — drives the status
        # hint colour/text. A single bad frame is normal during scroll animation
        # and must not be turned into a user-visible error.
        self._hint: str | None = None
        self._consecutive_low = 0
        self._low_since: float | None = None
        self._recoveries = 0
        self._finished = False

        # Background capture: grim takes ~36 ms per grab, which would freeze the
        # GTK main loop if run on it at a high rate. Instead a worker thread
        # grabs frames back-to-back (~20 fps) and hands an ordered frame queue
        # to the main thread via GLib.idle_add. The higher rate keeps consecutive
        # frames overlapping so "重叠不足" stops firing on a normal scroll.
        self._capture_thread: threading.Thread | None = None
        # Keep a short ordered queue instead of only the newest frame.  A
        # latest-only slot silently dropped intermediate scroll positions when
        # GTK was busy, making the next frame jump past the stitcher's overlap
        # window and forcing the user to roll back.  A bounded queue preserves
        # continuity while keeping pathological backlogs finite.
        self._pending_frames: deque[Image.Image] = deque(maxlen=48)
        self._pending_lock = threading.Lock()
        self._pending_condition = threading.Condition(self._pending_lock)
        self._idle_queued = False
        # Always retain the newest completed grab independently of the work
        # queue. When the user clicks 完成, this closes the small race where grim
        # has captured the final scroll position but GTK has not processed it.
        self._latest_frame: Image.Image | None = None

        self._build_panel()
        self.highlight = SelectionHighlight(app, rect, screen_size)

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

        content = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10)
        content.set_margin_top(14)
        content.set_margin_bottom(14)
        content.set_margin_start(14)
        content.set_margin_end(14)
        self.root.append(content)

        header = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        live_dot = Gtk.Label(label="●")
        live_dot.add_css_class("pngshot-live-dot")
        header.append(live_dot)
        title = Gtk.Label(label="长截图")
        title.add_css_class("pngshot-title")
        title.set_xalign(0.0)
        title.set_hexpand(True)
        header.append(title)
        self.state = Gtk.Label(label="采集中")
        self.state.add_css_class("pngshot-status-chip")
        header.append(self.state)
        content.append(header)

        # Direction and measurements are separate typographic roles: the user
        # sees what to do first, then the current capture facts.
        self.status = Gtk.Label(label="保持平稳滚动，画面会自动拼接")
        self.status.add_css_class("pngshot-title")
        self.status.set_wrap(True)
        self.status.set_max_width_chars(28)
        self.status.set_xalign(0.0)
        content.append(self.status)

        self.metrics = Gtk.Label()
        self.metrics.add_css_class("pngshot-dim")
        self.metrics.set_xalign(0.0)
        content.append(self.metrics)

        # instructions
        hint = Gtk.Label(label="保持目标窗口在前台滚动；再次按长截图快捷键即可完成")
        hint.add_css_class("pngshot-caption")
        hint.set_wrap(True)
        hint.set_max_width_chars(28)
        hint.set_xalign(0.0)
        content.append(hint)

        # buttons
        btnbox = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=10)
        btnbox.set_homogeneous(True)
        btnbox.set_margin_top(2)
        cancel_btn = Gtk.Button(label="取消  Esc")
        cancel_btn.add_css_class("pngshot-quiet")
        cancel_btn.connect("clicked", lambda _b: self._finish(cancel=True))
        done_btn = Gtk.Button(label="完成  Enter")
        done_btn.add_css_class("suggested-action")
        done_btn.connect("clicked", lambda _b: self._finish(cancel=False))
        btnbox.append(cancel_btn)
        btnbox.append(done_btn)
        content.append(btnbox)

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
        # are conservative estimates of the compact readout + buttons + margins.
        panel_w = 320
        panel_h = 190
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
        self.highlight.present()
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
        """Worker thread: grab the region back-to-back into an ordered queue.

        Runs off the GTK main loop so grim's ~36 ms latency never blocks the
        UI. A short bounded queue retains intermediate scroll positions while
        the main thread is briefly busy, preventing otherwise-overlapping
        frames from being skipped. ``poll_ms`` becomes a small inter-grab pause
        (a floor on the rate) rather than the sampling period.
        """
        r = self.rect
        pause = max(self.cfg.poll_ms, 0) / 1000.0
        # Warm up grim with one throwaway grab BEFORE the real loop. The first
        # grab is ~1.7x slower (cold process/cache), which stretches the gap
        # between frame #1 and #2; at a normal scroll speed that larger gap can
        # push the two frames past the overlap threshold -> a spurious "重叠不足"
        # on the very first uses. Discarding a cold frame means the first frame
        # the stitcher actually sees is already at the steady-state latency, so
        # frame spacing is even from the start.
        try:
            capture.grab_region(r.x, r.y, r.w, r.h)
        except capture.CaptureError:
            pass  # a failed warmup is harmless; the real loop retries + reports
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
            with self._pending_condition:
                self._latest_frame = frame
                # Never discard an intermediate scroll position: one dropped
                # bridge frame is enough to force the user to roll back.  Under
                # exceptional GTK load, pause sampling until ordered work has
                # drained instead of silently punching a hole in the sequence.
                while (self._sampling and
                       len(self._pending_frames) >= self._pending_frames.maxlen):
                    self._pending_condition.wait(timeout=0.05)
                if not self._sampling:
                    break
                self._pending_frames.append(frame)
                if not self._idle_queued:
                    self._idle_queued = True
                    GLib.idle_add(self._consume_pending)
            if pause:
                time.sleep(pause)

    def _consume_pending(self) -> bool:
        """Main thread: stitch the next captured frame in order (if any)."""
        with self._pending_lock:
            frame = self._pending_frames.popleft() if self._pending_frames else None
            self._idle_queued = False
            if frame is not None:
                self._pending_condition.notify()
        if frame is None or not self._sampling:
            return False
        self._process_frame(frame)
        with self._pending_lock:
            more = bool(self._pending_frames)
            if more and not self._idle_queued and self._sampling:
                self._idle_queued = True
                GLib.idle_add(self._consume_pending)
        return False  # one-shot; queue continuation is scheduled above

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

        # Classify *why* a frame was not appended. Low-confidence samples are
        # expected during kinetic scrolling and local animation, so debounce
        # them instead of immediately telling the user to roll backward.
        # (diff is a mean signature difference; LOWER is a better match.)
        if not first and not grew:
            if self.stitcher.last_diff > self.cfg.max_diff:
                now = time.monotonic()
                self._consecutive_low += 1
                if self._low_since is None:
                    self._low_since = now
                sustained = (
                    self._consecutive_low >= 12
                    and now - self._low_since >= 0.55
                )
                self._hint = "slow_down" if sustained else "recovering"
            else:
                self._consecutive_low = 0
                self._low_since = None
                self._hint = "no_move"
        else:
            recovered = getattr(self.stitcher, "last_recovered", False)
            if recovered:
                self._recoveries += 1
            self._consecutive_low = 0
            self._low_since = None
            self._hint = "recovered" if recovered else None
            if grew or first:
                self._captured_height = new_h
        if not getattr(self, "_finished", False):
            self._update_status()

    # ------------------------------------------------------------------

    def _update_status(self) -> None:
        tail = f"{self._captured_height:,} px  ·  {self.stitcher.frames_used} 帧"
        self.metrics.set_text(tail)
        if self._hint == "slow_down":
            self.root.remove_css_class("pngshot-alert")
            self.state.set_text("请慢一些")
            self.state.remove_css_class("pngshot-error")
            self.status.set_text("减慢滚动即可，程序会继续寻找重叠")
            self.status.remove_css_class("pngshot-error")
        elif self._hint == "recovering":
            self.root.remove_css_class("pngshot-alert")
            self.state.set_text("校准中")
            self.state.remove_css_class("pngshot-error")
            self.status.set_text("正在自动寻找重叠，可继续滚动")
            self.status.remove_css_class("pngshot-error")
        elif self._hint == "recovered":
            self.root.remove_css_class("pngshot-alert")
            self.state.set_text("已恢复")
            self.state.remove_css_class("pngshot-error")
            self.status.set_text("已自动接回画面，继续滚动即可")
            self.status.remove_css_class("pngshot-error")
        elif self._hint == "no_move":
            self.root.remove_css_class("pngshot-alert")
            self.state.set_text("等待滚动")
            self.state.remove_css_class("pngshot-error")
            self.status.set_text("向上或向下滚动目标窗口")
            self.status.remove_css_class("pngshot-error")
        else:
            self.root.remove_css_class("pngshot-alert")
            self.state.set_text("采集中")
            self.state.remove_css_class("pngshot-error")
            self.status.set_text("保持平稳滚动，画面会自动拼接")
            self.status.remove_css_class("pngshot-error")

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
        with self._pending_condition:
            self._pending_condition.notify_all()
        self.highlight.close()
        self.window.close()
        if cancel:
            self.on_done(None, [])
            return
        try:
            # A grab may already be in flight when 完成 is clicked. Waiting for
            # at most a fraction of a second after closing the panel lets that
            # final frame land without leaving the saved image one scroll-step
            # short. This only happens after the UI has disappeared.
            capture_thread = getattr(self, "_capture_thread", None)
            if capture_thread is not None and capture_thread.is_alive():
                capture_thread.join(timeout=0.25)
            # The worker may have captured several frames while the main loop
            # was updating status or while the user clicked 完成. Stitch those
            # already-owned frames before taking the final
            # snapshot; otherwise the saved image silently stops short of the
            # last visible scroll position.
            self._drain_pending_frames()
            latest = getattr(self, "_latest_frame", None)
            if latest is not None:
                self._process_frame(latest)
            res = self.stitcher.result()
            self.on_done(res.image, res.warnings)
        except ValueError:
            self.on_done(None, ["no frames captured"])

    def _drain_pending_frames(self) -> None:
        """Consume frames already captured before recording was stopped."""
        while True:
            with self._pending_lock:
                if not self._pending_frames:
                    return
                frame = self._pending_frames.popleft()
                condition = getattr(self, "_pending_condition", None)
                if condition is not None:
                    condition.notify()
            self._process_frame(frame)
