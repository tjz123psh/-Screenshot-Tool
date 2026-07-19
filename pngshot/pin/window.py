"""Deskpin window (Step 3).

A borderless floating window that shows a screenshot on top of everything.

Interactions (confirmed with the user):
  - scroll wheel        -> zoom the *image content*, window size unchanged,
                           anchored at the cursor
  - Ctrl + scroll wheel -> scale the *window itself* (image follows)
  - drag anywhere       -> move the window (Wayland begin_move via WindowHandle)
  - c                   -> copy image to clipboard
  - s                   -> save image
  - 0                   -> reset zoom to 1:1
  - q / Escape          -> close
  - right click         -> small popover menu (copy / save / reset / close)

"Pin/top-most" on niri == floating layout, so right after mapping we ask niri
to move this window to the floating layout. If niri isn't there, it's still a
normal window and everything else works.
"""
from __future__ import annotations

import math

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
gi.require_version("Pango", "1.0")
gi.require_version("PangoCairo", "1.0")

from gi.repository import Gdk, Gio, GLib, Gtk, Pango, PangoCairo  # noqa: E402
from PIL import Image  # noqa: E402

from ..services import clipboard, saver
from ..util import niri
from ..util.imaging import pil_to_cairo_surface

MIN_SCALE = 0.1
MAX_SCALE = 12.0
ZOOM_STEP = 1.1          # per wheel notch (content zoom)
WIN_STEP = 1.1           # per wheel notch (window zoom, Ctrl)
APP_ID = "ai.pngshot.pin"


class PinWindow:
    def __init__(self, app: Gtk.Application, img: Image.Image) -> None:
        self.app = app
        self.img = img.convert("RGBA")
        self.iw, self.ih = self.img.size
        self.surface, self._buf = pil_to_cairo_surface(self.img)

        # display scale of the image content, and its top-left offset in window
        self.scale = 1.0
        self.off_x = 0.0
        self.off_y = 0.0
        self._niri_id: int | None = None   # our window id in niri (set on map)
        self._popover: Gtk.PopoverMenu | None = None
        self._notice = ""
        self._notice_source = 0
        self._notice_error = False

        self.win_w, self.win_h = self._initial_window_size()
        # fit image into the initial window (contain)
        self.scale = min(self.win_w / self.iw, self.win_h / self.ih, 1.0)
        # center the (possibly smaller) image
        self.off_x = (self.win_w - self.iw * self.scale) / 2
        self.off_y = (self.win_h - self.ih * self.scale) / 2

        self.window = Gtk.ApplicationWindow(application=app)
        self.window.set_decorated(False)
        self.window.set_title("pngshot-pin")
        # app-id used by niri window rules (see contrib/niri snippet)
        self.window.set_default_size(self.win_w, self.win_h)

        from ..util import theme
        theme.apply(self.window)
        self.window.add_css_class("pngshot-transparent")

        self.canvas = Gtk.DrawingArea()
        self.canvas.set_draw_func(self._on_draw)
        self.canvas.set_hexpand(True)
        self.canvas.set_vexpand(True)

        # WindowHandle makes empty drags move the window on Wayland.
        handle = Gtk.WindowHandle()
        handle.set_child(self.canvas)
        self.window.set_child(handle)

        self._install_controllers()
        self._install_actions()

        # Ask niri to float us once we're actually mapped.
        self.window.connect("map", self._on_map)

    # ------------------------------------------------------------------

    def _initial_window_size(self) -> tuple[int, int]:
        max_w, max_h = 1600, 900
        display = Gdk.Display.get_default()
        if display is not None:
            monitors = display.get_monitors()
            if monitors.get_n_items() > 0:
                mon = monitors.get_item(0)
                geo = mon.get_geometry()
                max_w = int(geo.width * 0.9)
                max_h = int(geo.height * 0.9)
        if self.iw <= max_w and self.ih <= max_h:
            return self.iw, self.ih
        ratio = min(max_w / self.iw, max_h / self.ih)
        return max(1, int(self.iw * ratio)), max(1, int(self.ih * ratio))

    # ------------------------------------------------------------------

    def present(self) -> None:
        self.window.present()

    def _on_map(self, *_a) -> None:
        # Defer slightly so the window is focused before we ask niri to float.
        GLib.timeout_add(60, self._float_now)

    def _float_now(self) -> bool:
        niri.move_focused_to_floating()
        # Remember our niri window id so Ctrl+scroll can resize the whole tile
        # via IPC (GTK4's set_default_size does nothing to a mapped window).
        import os
        self._niri_id = niri.window_id_for_pid(os.getpid())
        return False  # one-shot

    # ------------------------------------------------------------------
    # controllers

    def _install_controllers(self) -> None:
        scroll = Gtk.EventControllerScroll.new(
            Gtk.EventControllerScrollFlags.VERTICAL
        )
        scroll.connect("scroll", self._on_scroll)
        self.canvas.add_controller(scroll)

        key = Gtk.EventControllerKey.new()
        key.connect("key-pressed", self._on_key)
        self.window.add_controller(key)

        # right-click popover menu
        rc = Gtk.GestureClick.new()
        rc.set_button(Gdk.BUTTON_SECONDARY)
        rc.connect("pressed", self._on_right_click)
        self.canvas.add_controller(rc)

        menu = Gio.Menu()
        menu.append("复制", "win.copy")
        menu.append("保存", "win.save")
        menu.append("重置缩放", "win.reset")
        menu.append("关闭", "win.close")
        self._popover = Gtk.PopoverMenu.new_from_model(menu)
        self._popover.set_parent(self.canvas)

        # track pointer for anchored zoom
        self._ptr_x = self.win_w / 2
        self._ptr_y = self.win_h / 2
        motion = Gtk.EventControllerMotion.new()
        motion.connect("motion", self._on_motion)
        self.canvas.add_controller(motion)

    def _install_actions(self) -> None:
        for name, cb in (
            ("copy", self._act_copy),
            ("save", self._act_save),
            ("reset", self._act_reset),
            ("close", self._act_close),
        ):
            a = Gio.SimpleAction.new(name, None)
            a.connect("activate", lambda _a, _p, cb=cb: cb())
            self.window.add_action(a)

    # ------------------------------------------------------------------
    # events

    def _on_motion(self, _m, x: float, y: float) -> None:
        self._ptr_x = x
        self._ptr_y = y

    def _on_scroll(self, ctrl: Gtk.EventControllerScroll, _dx: float, dy: float) -> bool:
        state = ctrl.get_current_event_state()
        ctrl_held = bool(state & Gdk.ModifierType.CONTROL_MASK)
        if dy == 0:
            return False
        if ctrl_held:
            self._zoom_window(dy)
        else:
            self._zoom_content(dy)
        return True

    def _zoom_content(self, dy: float) -> None:
        factor = 1 / ZOOM_STEP if dy > 0 else ZOOM_STEP
        new_scale = max(MIN_SCALE, min(MAX_SCALE, self.scale * factor))
        if new_scale == self.scale:
            return
        # keep the image point under the cursor fixed
        img_x = (self._ptr_x - self.off_x) / self.scale
        img_y = (self._ptr_y - self.off_y) / self.scale
        self.scale = new_scale
        self.off_x = self._ptr_x - img_x * new_scale
        self.off_y = self._ptr_y - img_y * new_scale
        self._show_notice(f"缩放  {self.scale * 100:.0f}%")

    def _zoom_window(self, dy: float) -> None:
        # Whole-tile zoom: resize the *window itself* so the whole sticker
        # (frame + image) grows/shrinks as one piece, like scaling a photo.
        #
        # GTK4's set_default_size does NOT resize an already-mapped window, and
        # on niri a floating window's size is owned by the compositor anyway.
        # So we drive the resize through niri IPC (set-window-width/height by
        # id), which was verified to change floating windows precisely. The
        # image scale/offset multiply by the same factor so content keeps its
        # proportion to the window. If niri isn't available we fall back to a
        # GTK resize (works before the first map / on other compositors).
        factor = 1 / WIN_STEP if dy > 0 else WIN_STEP
        new_w = max(80, int(round(self.win_w * factor)))
        new_h = max(60, int(round(self.win_h * factor)))
        ratio_w = new_w / self.win_w  # equal to factor except at the clamp floor
        # scale image content by the same uniform ratio (undistorted)
        self.scale *= ratio_w
        self.off_x *= ratio_w
        self.off_y *= ratio_w
        self.win_w, self.win_h = new_w, new_h

        applied = False
        if self._niri_id is not None:
            applied = niri.set_window_size(self._niri_id, new_w, new_h)
        if not applied:
            # fallback: best-effort GTK resize (mainly pre-map / non-niri)
            self.window.set_default_size(new_w, new_h)
        self._show_notice(f"窗口  {new_w} × {new_h}")

    def _on_key(self, _kc, keyval: int, _kc2: int, _state) -> bool:
        name = Gdk.keyval_name(keyval) or ""
        if name in ("q", "Escape"):
            self._act_close()
            return True
        if name == "c":
            self._act_copy()
            return True
        if name == "s":
            self._act_save()
            return True
        if name in ("0", "KP_0"):
            self._act_reset()
            return True
        return False

    def _on_right_click(self, _g, _n, x: float, y: float) -> None:
        pop = self._popover
        if pop is None:
            return
        pop.set_has_arrow(True)
        rect = Gdk.Rectangle()
        rect.x, rect.y, rect.width, rect.height = int(x), int(y), 1, 1
        pop.set_pointing_to(rect)
        pop.popup()

    # ------------------------------------------------------------------
    # actions

    def _act_copy(self) -> None:
        try:
            clipboard.copy_image(self.img)
            self._show_notice("已复制到剪贴板")
        except Exception as e:  # noqa: BLE001
            print(f"[pngshot] copy failed: {e}")
            self._show_notice("复制失败", error=True)

    def _act_save(self) -> None:
        try:
            path = saver.save_image(self.img, prefix="pngshot-pin")
            print(f"saved: {path}")
            self._show_notice("截图已保存")
        except Exception as e:  # noqa: BLE001
            print(f"[pngshot] save failed: {e}")
            self._show_notice("保存失败", error=True)

    def _act_reset(self) -> None:
        self.scale = 1.0
        self.off_x = (self.win_w - self.iw) / 2
        self.off_y = (self.win_h - self.ih) / 2
        self._show_notice("缩放  100%")

    def _act_close(self) -> None:
        self.window.close()

    def _show_notice(self, text: str, *, error: bool = False) -> None:
        self._notice = text
        self._notice_error = error
        if self._notice_source:
            GLib.source_remove(self._notice_source)
        self._notice_source = GLib.timeout_add(1600, self._clear_notice)
        self.canvas.queue_draw()

    def _clear_notice(self) -> bool:
        self._notice_source = 0
        self._notice = ""
        self._notice_error = False
        self.canvas.queue_draw()
        return False

    # ------------------------------------------------------------------
    # drawing

    def _on_draw(self, _da, ctx, w: int, h: int) -> None:
        self.win_w, self.win_h = w, h
        # A restrained checkerboard makes transparent/empty image bounds clear
        # without competing with the screenshot itself.
        ctx.set_source_rgba(0.055, 0.065, 0.085, 1.0)
        ctx.paint()
        cell = 18
        ctx.set_source_rgba(1, 1, 1, 0.025)
        for yy in range(0, h, cell):
            for xx in range(0, w, cell):
                if (xx // cell + yy // cell) % 2 == 0:
                    ctx.rectangle(xx, yy, cell, cell)
        ctx.fill()

        ctx.save()
        ctx.translate(self.off_x, self.off_y)
        ctx.scale(self.scale, self.scale)
        ctx.set_source_surface(self.surface, 0, 0)
        # nearest-ish for big zoom-in stays crisp enough; default is fine
        ctx.get_source().set_filter(cairo_filter(self.scale))
        ctx.paint()
        ctx.restore()

        # niri's rule removes compositor borders for pinned images. Retain a
        # subtle inner edge so light screenshots still have a visible boundary.
        ctx.rectangle(0.5, 0.5, max(0, w - 1), max(0, h - 1))
        ctx.set_source_rgba(1, 1, 1, 0.18)
        ctx.set_line_width(1)
        ctx.stroke()

        if self._notice:
            self._draw_notice(ctx, w, h, self._notice)

    def _draw_notice(self, ctx, width: int, height: int, text: str) -> None:
        layout = PangoCairo.create_layout(ctx)
        desc = Pango.FontDescription()
        desc.set_family("Sans")
        desc.set_absolute_size(12 * Pango.SCALE)
        layout.set_font_description(desc)
        layout.set_text(text, -1)
        tw, th = layout.get_pixel_size()
        pad_x, pad_y = 12, 7
        bw, bh = tw + pad_x * 2, th + pad_y * 2
        bx = (width - bw) / 2
        by = height - bh - 18
        ctx.save()
        if self._notice_error:
            ctx.set_source_rgba(0.30, 0.09, 0.12, 0.95)
        else:
            ctx.set_source_rgba(0.075, 0.09, 0.13, 0.95)
        _rounded_rect(ctx, bx, by, bw, bh, 10)
        ctx.fill_preserve()
        ctx.set_source_rgba(1, 1, 1, 0.16)
        ctx.set_line_width(1)
        ctx.stroke()
        ctx.set_source_rgba(1, 1, 1, 0.92)
        ctx.move_to(bx + pad_x, by + pad_y)
        PangoCairo.show_layout(ctx, layout)
        ctx.restore()


def cairo_filter(scale: float):
    import cairo
    # when zoomed way in, GOOD/ nearest keeps pixels crisp; else bilinear
    return cairo.Filter.NEAREST if scale >= 3 else cairo.Filter.GOOD


def _rounded_rect(cr, x: float, y: float, w: float, h: float, radius: float) -> None:
    radius = min(radius, w / 2, h / 2)
    cr.new_sub_path()
    cr.arc(x + w - radius, y + radius, radius, -math.pi / 2, 0)
    cr.arc(x + w - radius, y + h - radius, radius, 0, math.pi / 2)
    cr.arc(x + radius, y + h - radius, radius, math.pi / 2, math.pi)
    cr.arc(x + radius, y + radius, radius, math.pi, 3 * math.pi / 2)
    cr.close_path()


# ---------------------------------------------------------------------------
# entry points

def run_pin(img: Image.Image) -> int:
    # NON_UNIQUE: each pin is a separate detached process. Without this the
    # second pin would only send `activate` to the first still-open pin (which
    # re-shows its OLD image) and exit, deleting its --cleanup temp file, so the
    # new image never pins. NON_UNIQUE lets every pin own its own window.
    app = Gtk.Application(application_id=APP_ID,
                          flags=Gio.ApplicationFlags.NON_UNIQUE)

    def on_activate(a: Gtk.Application) -> None:
        win = PinWindow(a, img)
        win.present()

    app.connect("activate", on_activate)
    app.run(None)
    return 0


def run_pin_from_clipboard() -> int:
    img = clipboard.paste_image()
    if img is None:
        print("[pngshot] clipboard has no image")
        return 1
    return run_pin(img)
