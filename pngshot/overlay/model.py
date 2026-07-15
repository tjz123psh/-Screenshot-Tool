"""Shared data types for the Stage 1 overlay."""
from __future__ import annotations

from dataclasses import dataclass
from enum import Enum, auto


class Mode(Enum):
    IDLE = auto()               # no selection yet
    SELECTING = auto()          # dragging to create selection
    HAS_SELECTION = auto()      # selection exists, cursor idle
    MOVING = auto()             # dragging inside selection
    RESIZING = auto()           # dragging a handle
    # placeholders for later steps
    ANNOTATE = auto()
    LONGSHOT = auto()


class HitKind(Enum):
    NONE = auto()
    OUTSIDE = auto()            # outside selection but on backdrop
    INSIDE = auto()             # inside selection (drag to move)
    HANDLE = auto()             # resize handle (see handle_index)
    TOOLBAR = auto()            # toolbar button (see button_id)


@dataclass
class Hit:
    kind: HitKind = HitKind.NONE
    handle_index: int = -1      # 0..7, see HANDLE_ORDER below
    button_id: str = ""


@dataclass
class Rect:
    x: int = 0
    y: int = 0
    w: int = 0
    h: int = 0

    @property
    def valid(self) -> bool:
        return self.w > 0 and self.h > 0

    @property
    def x2(self) -> int:
        return self.x + self.w

    @property
    def y2(self) -> int:
        return self.y + self.h

    def normalized(self) -> "Rect":
        x1, x2 = sorted((self.x, self.x + self.w))
        y1, y2 = sorted((self.y, self.y + self.h))
        return Rect(x1, y1, x2 - x1, y2 - y1)

    def contains(self, px: float, py: float) -> bool:
        return self.x <= px < self.x2 and self.y <= py < self.y2

    def clamp_to(self, w: int, h: int) -> "Rect":
        x1 = max(0, min(self.x, w))
        y1 = max(0, min(self.y, h))
        x2 = max(0, min(self.x2, w))
        y2 = max(0, min(self.y2, h))
        return Rect(x1, y1, x2 - x1, y2 - y1)


# Handle order: 0=NW 1=N 2=NE 3=E 4=SE 5=S 6=SW 7=W
HANDLE_ORDER = ("nw", "n", "ne", "e", "se", "s", "sw", "w")
HANDLE_CURSORS = {
    "nw": "nw-resize", "n": "n-resize", "ne": "ne-resize", "e": "e-resize",
    "se": "se-resize", "s": "s-resize", "sw": "sw-resize", "w": "w-resize",
}


def handle_positions(r: Rect) -> list[tuple[str, float, float]]:
    """Center coordinates of each handle for a given selection rect."""
    cx = r.x + r.w / 2
    cy = r.y + r.h / 2
    return [
        ("nw", r.x,   r.y),
        ("n",  cx,    r.y),
        ("ne", r.x2,  r.y),
        ("e",  r.x2,  cy),
        ("se", r.x2,  r.y2),
        ("s",  cx,    r.y2),
        ("sw", r.x,   r.y2),
        ("w",  r.x,   cy),
    ]
