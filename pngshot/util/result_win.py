"""Result window for OCR / translation output.

A small floating GTK window with an editable text view plus action buttons.
Runs as its own process (spawned detached from the overlay, like the pin
window) so it outlives the one-shot screenshot process.

Modes:
  - "ocr":       show recognised text; buttons [复制] [翻译] [关闭]
  - "translate": show translated text;  buttons [复制] [关闭]

For "translate" launched directly, the text passed in is already the
translation. For the [翻译] button inside an OCR window, we translate the
*current* (possibly user-edited) text and open a new translate window.
"""
from __future__ import annotations

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")

from gi.repository import Gdk, Gio, GLib, Gtk  # noqa: E402

from PIL import Image  # noqa: E402

from ..config import load as load_config
from ..services import clipboard
from ..util import niri

APP_ID = "ai.pngshot.result"


class ResultWindow:
    def __init__(self, app: Gtk.Application, mode: str, text: str,
                 source_img: "Image.Image | None" = None,
                 auto_translate: bool = False) -> None:
        self.app = app
        self.mode = mode
        self.source_img = source_img      # kept so [翻译] can re-run if needed
        self.window = Gtk.ApplicationWindow(application=app)
        self.window.set_title("pngshot-result")
        self.window.set_default_size(560, 400)

        from ..util import theme
        theme.apply(self.window)
        self.window.add_css_class("pngshot-window")

        root = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10)
        root.set_margin_top(16)
        root.set_margin_bottom(16)
        root.set_margin_start(18)
        root.set_margin_end(18)
        self.window.set_child(root)

        # header
        title = "OCR 结果" if mode == "ocr" else "翻译结果"
        header = Gtk.Label(label=title)
        header.add_css_class("pngshot-title")
        header.set_xalign(0.0)
        root.append(header)

        # text area (editable, scrollable) inside a rounded card
        scroller = Gtk.ScrolledWindow()
        scroller.set_vexpand(True)
        scroller.set_hexpand(True)
        scroller.add_css_class("pngshot-textview")
        self.textview = Gtk.TextView()
        self.textview.set_wrap_mode(Gtk.WrapMode.WORD_CHAR)
        self.textview.get_buffer().set_text(text)
        self.textview.set_monospace(False)
        self.textview.set_left_margin(4)
        self.textview.set_right_margin(4)
        self.textview.set_top_margin(4)
        self.textview.set_bottom_margin(4)
        scroller.set_child(self.textview)
        root.append(scroller)

        # status line (for translate progress / errors)
        self.status = Gtk.Label(label="")
        self.status.add_css_class("pngshot-dim")
        self.status.set_xalign(0.0)
        self.status.set_visible(False)
        root.append(self.status)

        # button row
        btnbox = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=10)
        btnbox.set_halign(Gtk.Align.END)
        btnbox.set_margin_top(2)

        copy_btn = Gtk.Button(label="复制")
        copy_btn.connect("clicked", self._on_copy)
        btnbox.append(copy_btn)

        if mode == "ocr":
            trans_btn = Gtk.Button(label="翻译")
            trans_btn.add_css_class("suggested-action")
            trans_btn.connect("clicked", self._on_translate)
            btnbox.append(trans_btn)

        close_btn = Gtk.Button(label="关闭")
        close_btn.add_css_class("pngshot-quiet")
        close_btn.connect("clicked", lambda _b: self.window.close())
        btnbox.append(close_btn)

        root.append(btnbox)

        # Esc closes
        key = Gtk.EventControllerKey.new()
        key.connect("key-pressed", self._on_key)
        self.window.add_controller(key)

        self.window.connect("map", self._on_map)

    # ------------------------------------------------------------------

    def present(self) -> None:
        self.window.present()

    def _on_map(self, *_a) -> None:
        GLib.timeout_add(60, lambda: (niri.move_focused_to_floating(), False)[1])

    def _current_text(self) -> str:
        buf = self.textview.get_buffer()
        start, end = buf.get_bounds()
        return buf.get_text(start, end, False)

    def set_result(self, text: str, status: str = "") -> bool:
        """Replace the text view contents (called from a worker via idle_add)."""
        self.textview.get_buffer().set_text(text)
        self._flash(status)
        return False  # one-shot idle callback

    # ------------------------------------------------------------------
    # actions

    def _on_copy(self, _btn) -> None:
        try:
            clipboard.copy_text(self._current_text())
            self._flash("已复制到剪贴板")
        except Exception as e:  # noqa: BLE001
            self._flash(f"复制失败: {e}")

    def _on_translate(self, _btn) -> None:
        text = self._current_text().strip()
        if not text:
            self._flash("没有文本可翻译")
            return
        self._flash("翻译中…")

        # Run translation off the main loop so the UI stays responsive.
        import threading

        def worker() -> None:
            from ..services import llm
            cfg = load_config()
            try:
                out = llm.translate(text, cfg.llm)
                GLib.idle_add(self._open_translation, out)
            except Exception as e:  # noqa: BLE001
                GLib.idle_add(self._flash, f"翻译失败: {e}")

        threading.Thread(target=worker, daemon=True).start()

    def _open_translation(self, translated: str) -> bool:
        self._flash("")
        win = ResultWindow(self.app, "translate", translated)
        win.present()
        return False

    def _on_key(self, _kc, keyval: int, _kc2: int, _state) -> bool:
        if Gdk.keyval_name(keyval) == "Escape":
            self.window.close()
            return True
        return False

    def _flash(self, msg: str) -> bool:
        self.status.set_text(msg)
        self.status.set_visible(bool(msg))
        return False


# ---------------------------------------------------------------------------
# entry point

def run_result(mode: str, text: str) -> int:
    # NON_UNIQUE: each screenshot spawns its own detached result process. With
    # the default single-instance flags the second process would only send an
    # `activate` to the first (which then re-shows its OLD text) and exit,
    # taking its --cleanup temp file with it — so the new capture never gets
    # OCR'd and the stale window lingers. NON_UNIQUE makes every process its
    # own primary instance, so each capture opens its own window.
    app = Gtk.Application(application_id=APP_ID,
                          flags=Gio.ApplicationFlags.NON_UNIQUE)

    def on_activate(a: Gtk.Application) -> None:
        win = ResultWindow(a, mode, text)
        win.present()

    app.connect("activate", on_activate)
    app.run(None)
    return 0


def run_text_action(img, translate: bool) -> int:
    """OCR an image (optionally translate) and show the result window.

    Called from the detached ``text-file`` subprocess. The window opens
    immediately in a "processing" state; OCR (and translation) run on a
    background thread so the UI is responsive and any error is shown in the
    window instead of silently killing the process.
    """
    # NON_UNIQUE: see run_result — each detached OCR/translate process must own
    # its window rather than defer to an earlier still-open one.
    app = Gtk.Application(application_id=APP_ID,
                          flags=Gio.ApplicationFlags.NON_UNIQUE)
    mode = "translate" if translate else "ocr"

    def on_activate(a: Gtk.Application) -> None:
        placeholder = "识别中…" if not translate else "识别并翻译中…"
        win = ResultWindow(a, mode, placeholder, source_img=img)
        win.present()

        import threading

        def worker() -> None:
            from ..config import load as _load
            from ..services import llm, ocr
            cfg = _load()
            try:
                text = ocr.recognize(img, cfg.ocr.langs)
                if not text.strip():
                    GLib.idle_add(win.set_result, "（未识别到文字）", "")
                    return
                if translate:
                    GLib.idle_add(win.set_result, text, "翻译中…")
                    out = llm.translate(text, cfg.llm)
                    GLib.idle_add(win.set_result, out, "")
                else:
                    GLib.idle_add(win.set_result, text, "")
            except Exception as e:  # noqa: BLE001
                GLib.idle_add(win.set_result, f"[错误] {e}", "")

        threading.Thread(target=worker, daemon=True).start()

    app.connect("activate", on_activate)
    app.run(None)
    return 0
