"""Stage 1 overlay entry point.

Wires: capture → OverlaySurface → result callback → action handling.

Most terminal actions (pin / OCR / translate) are handed to a detached child
process so their window outlives this one-shot screenshot process. Two actions
stay in-process because they need the live GTK loop and the on-screen region:

  - long   : hides the overlay, then drives the LongshotRecorder which samples
             the *screen rect* while the user scrolls. The stitched image then
             re-enters the normal action pipeline (copy/save/pin/…).
"""
from __future__ import annotations

import gi

gi.require_version("Gtk", "4.0")

from gi.repository import Gio, GLib, Gtk  # noqa: E402
from PIL import Image  # noqa: E402

from .. import capture
from ..config import load as load_config
from ..overlay.model import Rect
from ..services import clipboard, saver


def run_region(*, save: bool = True, copy: bool = True, long_shot: bool = False) -> int:
    """Launch the Stage 1 overlay and process the user's chosen action.

    Returns 0 on success, non-zero on cancel or error.
    """
    try:
        bg = capture.grab_full()
    except capture.CaptureError as e:
        print(f"[pngshot] capture failed: {e}")
        return 1

    # NON_UNIQUE: every hotkey press launches a fresh, one-shot region process.
    # With the default single-instance flags a second launch only forwards an
    # `activate` to the first process and exits — and if that first process is
    # stuck (its overlay never finished, e.g. focus/workspace changed mid-drag)
    # the compositor re-presents the stale overlay and yanks the user to its
    # workspace instead of starting a new screenshot. NON_UNIQUE makes each
    # launch its own primary instance, so a hung one can never hijack later ones.
    app = Gtk.Application(application_id="ai.pngshot.overlay",
                          flags=Gio.ApplicationFlags.NON_UNIQUE)
    state: dict = {
        "action": "cancel",
        "cropped": None,
        "rect": None,
        "save": save,
        "copy": copy,
        "long_shot": long_shot,
        "screen_size": bg.size,  # (w, h) — lets the recorder park its panel off the selection
        "exit_code": 0,
    }

    def on_result(action: str, cropped: Image.Image | None, rect: Rect | None) -> None:
        # If the user confirmed a plain region but we were launched in long-shot
        # mode, promote the action to "long" so scrolling capture kicks in.
        if action == "confirm" and long_shot:
            action = "long"
        state["action"] = action
        state["cropped"] = cropped
        state["rect"] = rect

        if action == "long" and rect is not None and rect.valid:
            # Keep the app alive; hand off to the recorder after the overlay
            # surface is gone so we don't sample our own dimming overlay.
            # hold() prevents the app from quitting during the window-less gap.
            app.hold()
            _close_overlay(app)
            GLib.timeout_add(250, lambda: _begin_longshot(app, rect, state))
            return

        _close_overlay(app)

    def on_activate(a: Gtk.Application) -> None:
        from .surface import OverlaySurface
        surface = OverlaySurface(a, bg, on_result, long_shot=long_shot)
        surface.present()

    app.connect("activate", on_activate)
    app.run(None)

    return _handle_action(state)


def _close_overlay(app: Gtk.Application) -> None:
    for win in app.get_windows():
        # only close the overlay; recorder bar (if any) manages its own life
        from .surface import OverlaySurface  # noqa: F401
        win.close()


# ---------------------------------------------------------------------------
# long-shot integration (in-process)

def _begin_longshot(app: Gtk.Application, rect: Rect, state: dict) -> bool:
    from ..longshot.recorder import LongshotRecorder
    cfg = load_config()
    screen_size = state.get("screen_size")

    def on_done(img: Image.Image | None, warnings: list) -> None:
        state["cropped"] = img
        state["action"] = "long_done"
        state["warnings"] = warnings
        # everything else (copy/save) happens after the loop exits
        for win in app.get_windows():
            win.close()
        # release the hold() taken before the window-less gap so the main
        # loop can finally quit once the recorder bar is gone.
        app.release()

    try:
        rec = LongshotRecorder(app, rect, cfg.longshot, on_done,
                               screen_size=screen_size)
        rec.present()
    except Exception as e:  # noqa: BLE001
        print(f"[pngshot] long-shot failed to start: {e}")
        state["action"] = "cancel"
        app.release()
        for win in app.get_windows():
            win.close()
    return False  # one-shot timer


# ---------------------------------------------------------------------------

def _handle_action(state: dict) -> int:
    action = state["action"]
    cropped: Image.Image | None = state["cropped"]

    if action == "cancel":
        print("[pngshot] cancelled")
        return 130

    if action == "long_done":
        if cropped is None:
            print("[pngshot] long-shot cancelled / no frames")
            return 130
        warnings = state.get("warnings") or []
        for w in warnings:
            print(f"[pngshot] warning: {w}")
        # Saving is enabled by default for parity with the interactive confirm
        # action, while both legs can be disabled from the CLI.
        return _keep_image(cropped, state, prefix="pngshot-long", long_shot=True)

    if cropped is None:
        print("[pngshot] cancelled")
        return 130

    if action == "pin":
        return _spawn_detached(cropped, ["pin-file", "--cleanup"], "pinned")

    if action == "ocr":
        return _spawn_detached(
            cropped, ["text-file", "--mode", "ocr", "--cleanup"], "ocr started"
        )

    if action == "translate":
        return _spawn_detached(
            cropped, ["text-file", "--mode", "translate", "--cleanup"], "translate started"
        )

    if action == "annotate":
        # Annotation happens inside the overlay before a terminal action, so
        # reaching here means it was chosen as a no-op; fall through to the
        # default keep behaviour (save + copy).
        pass

    # Default "keep" behaviour for confirm/annotate.  The interactive action
    # defaults to both save and copy, but detached callers can opt out of either
    # leg through the CLI state.
    return _keep_image(cropped, state)


def _keep_image(cropped: Image.Image, state: dict, *,
                prefix: str = "pngshot", long_shot: bool = False) -> int:
    """Attempt save and clipboard independently so one failure loses no data."""
    failed = False
    if state["save"]:
        try:
            path = saver.save_image(cropped, prefix=prefix)
            print(f"saved: {path}")
        except Exception as e:  # noqa: BLE001
            failed = True
            print(f"[pngshot] save failed: {e}")
    if state["copy"]:
        try:
            clipboard.copy_image(cropped)
            if long_shot:
                print(f"long-shot done: {cropped.size[0]}x{cropped.size[1]} copied")
            else:
                print(f"copied: {cropped.size[0]}x{cropped.size[1]} to clipboard")
        except Exception as e:  # noqa: BLE001
            failed = True
            print(f"[pngshot] clipboard failed: {e}")
    return 1 if failed else 0


def _spawn_detached(cropped: Image.Image, subcmd: list[str], done_msg: str) -> int:
    """Write the crop to a temp PNG and launch a detached child process.

    Used for actions whose window must outlive this one-shot screenshot
    process (pin / OCR / translate). The child gets the temp file path as its
    last argument and deletes it (``--cleanup``) after loading it into memory.
    """
    import os
    import subprocess
    import sys
    import tempfile

    fd, path = tempfile.mkstemp(prefix="pngshot-", suffix=".png")
    os.close(fd)
    try:
        cropped.save(path, format="PNG")
        cmd = [sys.executable, "-m", "pngshot", *subcmd, path]
        subprocess.Popen(
            cmd,
            start_new_session=True,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env=os.environ.copy(),
        )
    except Exception as e:  # noqa: BLE001
        try:
            os.unlink(path)
        except OSError:
            pass
        print(f"[pngshot] failed to prepare or launch child: {e}")
        return 1
    print(done_msg)
    return 0
