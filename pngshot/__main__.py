"""pngshot CLI entrypoint.

User-facing subcommands:
  region            interactive region screenshot (Stage 1 overlay)
  long              interactive region + long-shot mode
  pin-last          open a pin window with the current clipboard image
  debug-capture     smoke-test: grab full screen, copy (+ optionally save)

Internal subcommands (spawned as detached children by the overlay; the crop
is passed as a temp PNG that the child deletes with --cleanup):
  pin-file <path>   open a pin window for an image file
  text-file <path>  run OCR (--mode ocr) or translate (--mode translate) and
                    show a result window
"""
from __future__ import annotations

import argparse
import sys


def _cmd_region(args: argparse.Namespace) -> int:
    from .overlay import app
    return app.run_region(save=args.save, copy=not args.no_copy, long_shot=False)


def _cmd_long(args: argparse.Namespace) -> int:
    from .overlay import app
    return app.run_region(save=args.save, copy=not args.no_copy, long_shot=True)


def _cmd_pin_last(_args: argparse.Namespace) -> int:
    from .pin import window
    return window.run_pin_from_clipboard()


def _cmd_pin_file(args: argparse.Namespace) -> int:
    from PIL import Image

    from .pin import window
    try:
        img = Image.open(args.path)
        img.load()
    except Exception as e:  # noqa: BLE001
        print(f"[pngshot] cannot open image: {e}", file=sys.stderr)
        return 1
    rc = window.run_pin(img)
    if args.cleanup:
        import os
        try:
            os.unlink(args.path)
        except OSError:
            pass
    return rc


def _cmd_text_file(args: argparse.Namespace) -> int:
    """Run OCR (and optionally translate) on an image file, show a result window.

    Used as a detached child process spawned by the overlay so the result
    window can own its own GTK main loop after the overlay has closed.
    """
    from PIL import Image

    from .util import result_win
    try:
        img = Image.open(args.path)
        img.load()
    except Exception as e:  # noqa: BLE001
        print(f"[pngshot] cannot open image: {e}", file=sys.stderr)
        return 1
    rc = result_win.run_text_action(img, translate=(args.mode == "translate"))
    if args.cleanup:
        import os
        try:
            os.unlink(args.path)
        except OSError:
            pass
    return rc


def _cmd_debug_capture(args: argparse.Namespace) -> int:
    from . import capture
    from .services import clipboard, saver
    img = capture.grab_full()
    print(f"grabbed: {img.size} mode={img.mode}")
    if args.save:
        path = saver.save_image(img, prefix="pngshot-debug")
        print(f"saved:   {path}")
    if not args.no_copy:
        clipboard.copy_image(img)
        print("copied:  clipboard (image/png)")
    return 0


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="pngshot")
    sub = p.add_subparsers(dest="cmd", required=True)

    r = sub.add_parser("region", help="interactive region screenshot")
    r.add_argument("--save", action="store_true", help="save to ~/Pictures/Screenshots")
    r.add_argument("--no-copy", action="store_true", help="do not copy to clipboard")
    r.set_defaults(func=_cmd_region)

    l = sub.add_parser("long", help="interactive region + long-shot")
    l.add_argument("--save", action="store_true")
    l.add_argument("--no-copy", action="store_true")
    l.set_defaults(func=_cmd_long)

    pin = sub.add_parser("pin-last", help="pin the current clipboard image")
    pin.set_defaults(func=_cmd_pin_last)

    pinf = sub.add_parser("pin-file", help="pin an image file (internal use)")
    pinf.add_argument("path")
    pinf.add_argument("--cleanup", action="store_true",
                      help="delete the file after the window closes")
    pinf.set_defaults(func=_cmd_pin_file)

    txt = sub.add_parser("text-file", help="OCR/translate an image file (internal use)")
    txt.add_argument("path")
    txt.add_argument("--mode", choices=("ocr", "translate"), default="ocr")
    txt.add_argument("--cleanup", action="store_true",
                     help="delete the file after the window closes")
    txt.set_defaults(func=_cmd_text_file)

    dbg = sub.add_parser("debug-capture", help="smoke-test the capture/clipboard pipeline")
    dbg.add_argument("--save", action="store_true")
    dbg.add_argument("--no-copy", action="store_true")
    dbg.set_defaults(func=_cmd_debug_capture)

    return p


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return args.func(args)
    except KeyboardInterrupt:
        return 130
    except Exception as e:  # noqa: BLE001
        print(f"[pngshot] error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
