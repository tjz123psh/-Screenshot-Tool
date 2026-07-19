"""Persistent preferences for the lightweight system tray.

This module deliberately has no GTK imports.  The screenshot UI uses GTK 4
while the AppIndicator process uses GTK 3, so configuration code must remain
safe to import from tests and other non-UI processes.
"""
from __future__ import annotations

import json
import os
from pathlib import Path


DEFAULTS = {"save": True, "copy": True}


def preference_path() -> Path:
    base = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))
    return base / "pngshot/tray.json"


def load_preferences() -> dict[str, bool]:
    try:
        value = json.loads(preference_path().read_text())
    except (OSError, json.JSONDecodeError):
        return DEFAULTS.copy()
    if not isinstance(value, dict):
        return DEFAULTS.copy()
    return {
        key: value.get(key) if isinstance(value.get(key), bool) else default
        for key, default in DEFAULTS.items()
    }


def save_preferences(preferences: dict[str, bool]) -> None:
    path = preference_path()
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    path.write_text(json.dumps(preferences, ensure_ascii=False, indent=2) + "\n")
