"""pngshot CLI entrypoint.

User-facing subcommands:
  region            interactive region screenshot (Stage 1 overlay)
  long              interactive region + long-shot mode
  pin-last          open a pin window with the current clipboard image
  tray              run the compact system tray
  status/doctor     service state and runtime health checks
  restart/logs      service recovery and recent activity
  debug-capture     smoke-test: grab full screen, copy (+ optionally save)

Internal subcommands (spawned as detached children by the overlay; the crop
is passed as a temp PNG that the child loads and deletes with --cleanup):
  pin-file <path>   open a pin window for an image file
  text-file <path>  run OCR (--mode ocr) or translate (--mode translate) and
                    show a result window
"""
from __future__ import annotations

import argparse
import json
import os
import sys


def _load_image_file(path: str, *, cleanup: bool):
    """Load an image fully, then remove an internal temp file immediately."""
    from PIL import Image

    try:
        with Image.open(path) as source:
            return source.copy()
    except Exception as e:  # noqa: BLE001
        print(f"[pngshot] cannot open image: {e}", file=sys.stderr)
        return None
    finally:
        if cleanup:
            try:
                os.unlink(path)
            except OSError:
                pass


def _cmd_region(args: argparse.Namespace) -> int:
    from .overlay import app
    from .controller import route_action
    forwarded = []
    if not args.save:
        forwarded.append("--no-save")
    if args.no_copy:
        forwarded.append("--no-copy")
    handled, code = route_action("region", forwarded)
    if handled:
        return code
    return app.run_region(save=args.save, copy=not args.no_copy, long_shot=False)


def _cmd_long(args: argparse.Namespace) -> int:
    from .overlay import app
    from .controller import route_action
    forwarded = []
    if not args.save:
        forwarded.append("--no-save")
    if args.no_copy:
        forwarded.append("--no-copy")
    handled, code = route_action("long", forwarded)
    if handled:
        return code
    return app.run_region(save=args.save, copy=not args.no_copy, long_shot=True)


def _cmd_pin_last(_args: argparse.Namespace) -> int:
    from .controller import route_action
    handled, code = route_action("pin-last", [])
    if handled:
        return code
    from .pin import window
    return window.run_pin_from_clipboard()


def _cmd_daemon(_args: argparse.Namespace) -> int:
    from .controller import run_daemon
    return run_daemon()


def _cmd_status(args: argparse.Namespace) -> int:
    from .controller import service_status
    status = service_status()
    if args.json:
        print(json.dumps(status, ensure_ascii=False))
    elif status.get("running"):
        action_names = {"region": "区域截图", "long": "长截图"}
        state = (
            "正在" + action_names.get(status.get("action"), str(status.get("action")))
            if status.get("action") else "空闲"
        )
        print(f"Pngshot 服务已就绪 · PID {status.get('pid')} · {state}")
        print(f"最近活动：{status.get('last_event')}")
    else:
        print("Pngshot 服务未运行（执行截图时会自动启动）")
    return 0 if status.get("running") else 3


def _cmd_doctor(args: argparse.Namespace) -> int:
    from .diagnostics import summary
    report = summary()
    if args.json:
        print(json.dumps(report, ensure_ascii=False))
    else:
        marks = {"ok": "✓", "warning": "!", "error": "✗"}
        for check in report["checks"]:
            print(f"{marks[check['status']]} {check['title']}: {check['detail']}")
        print(
            f"\n诊断完成：{report['errors']} 个错误，"
            f"{report['warnings']} 个提醒"
        )
    return 0 if report["healthy"] else 1


def _cmd_restart(_args: argparse.Namespace) -> int:
    from .controller import restart_service
    if restart_service():
        print("Pngshot 服务已重新启动")
        return 0
    print("Pngshot 服务重启失败", file=sys.stderr)
    return 1


def _cmd_logs(args: argparse.Namespace) -> int:
    from .controller import log_path, tail_log
    content = tail_log(args.lines)
    if content:
        print(content)
    else:
        print(f"暂无日志：{log_path()}")
    return 0


def _cmd_tray(_args: argparse.Namespace) -> int:
    from .tray import run
    return run()


def _cmd_pin_file(args: argparse.Namespace) -> int:
    from .pin import window
    img = _load_image_file(args.path, cleanup=args.cleanup)
    if img is None:
        return 1
    return window.run_pin(img)


def _cmd_text_file(args: argparse.Namespace) -> int:
    """Run OCR (and optionally translate) on an image file, show a result window.

    Used as a detached child process spawned by the overlay so the result
    window can own its own GTK main loop after the overlay has closed.
    """
    from .util import result_win
    img = _load_image_file(args.path, cleanup=args.cleanup)
    if img is None:
        return 1
    return result_win.run_text_action(img, translate=(args.mode == "translate"))


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
    r.add_argument("--save", dest="save", action=argparse.BooleanOptionalAction,
                   default=True, help="save to ~/Pictures/Screenshots (default)")
    r.add_argument("--no-copy", action="store_true", help="do not copy to clipboard")
    r.set_defaults(func=_cmd_region)

    l = sub.add_parser("long", help="interactive region + long-shot")
    l.add_argument("--save", dest="save", action=argparse.BooleanOptionalAction,
                   default=True, help="save to ~/Pictures/Screenshots (default)")
    l.add_argument("--no-copy", action="store_true", help="do not copy to clipboard")
    l.set_defaults(func=_cmd_long)

    pin = sub.add_parser("pin-last", help="pin the current clipboard image")
    pin.set_defaults(func=_cmd_pin_last)

    pinf = sub.add_parser("pin-file", help="pin an image file (internal use)")
    pinf.add_argument("path")
    pinf.add_argument("--cleanup", action="store_true",
                      help="delete the file after loading it")
    pinf.set_defaults(func=_cmd_pin_file)

    txt = sub.add_parser("text-file", help="OCR/translate an image file (internal use)")
    txt.add_argument("path")
    txt.add_argument("--mode", choices=("ocr", "translate"), default="ocr")
    txt.add_argument("--cleanup", action="store_true",
                     help="delete the file after loading it")
    txt.set_defaults(func=_cmd_text_file)

    dbg = sub.add_parser("debug-capture", help="smoke-test the capture/clipboard pipeline")
    dbg.add_argument("--save", action="store_true")
    dbg.add_argument("--no-copy", action="store_true")
    dbg.set_defaults(func=_cmd_debug_capture)

    daemon = sub.add_parser("daemon", help="run the pngshot control service")
    daemon.set_defaults(func=_cmd_daemon)

    status = sub.add_parser("status", help="show control-service status")
    status.add_argument("--json", action="store_true", help="machine-readable status")
    status.set_defaults(func=_cmd_status)

    doctor = sub.add_parser("doctor", help="check screenshot dependencies and setup")
    doctor.add_argument("--json", action="store_true", help="machine-readable report")
    doctor.set_defaults(func=_cmd_doctor)

    restart = sub.add_parser("restart", help="restart the control service")
    restart.set_defaults(func=_cmd_restart)

    logs = sub.add_parser("logs", help="show recent service logs")
    logs.add_argument("--lines", type=int, default=50)
    logs.set_defaults(func=_cmd_logs)

    tray = sub.add_parser("tray", help="run the Pngshot system tray")
    tray.set_defaults(func=_cmd_tray)

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
