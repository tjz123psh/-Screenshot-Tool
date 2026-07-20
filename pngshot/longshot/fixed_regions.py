"""Multi-frame detection for viewport-fixed edge regions."""
from __future__ import annotations

from collections import deque
from dataclasses import dataclass

import numpy as np


@dataclass(frozen=True)
class FixedBands:
    top: int = 0
    bottom: int = 0
    left: int = 0
    right: int = 0

    @property
    def any(self) -> bool:
        return any((self.top, self.bottom, self.left, self.right))


class FixedRegionDetector:
    """Detect stable bands touching the four viewport edges.

    Inputs are sparse RGB viewport samples with shape ``[height, columns, 3]``.
    A band is exposed only after three confirmed scrolling transitions and must
    remain stable in at least 80% of the recent observations. This avoids
    classifying a coincidentally similar pair of page frames as fixed chrome.
    """

    def __init__(self, height: int, sample_columns: int) -> None:
        self.height = max(0, height)
        self.sample_columns = max(0, sample_columns)
        self._row_observations: deque[np.ndarray] = deque(maxlen=6)
        self._column_observations: deque[np.ndarray] = deque(maxlen=6)

    @property
    def ready(self) -> bool:
        return len(self._row_observations) >= 3

    def observe(self, previous: np.ndarray, current: np.ndarray) -> None:
        if previous.shape != current.shape or previous.ndim != 3:
            return
        if previous.shape[:2] != (self.height, self.sample_columns):
            return
        delta = np.abs(
            previous.astype(np.int16) - current.astype(np.int16)
        )
        unchanged = delta.max(axis=2) <= 4
        # Rows need nearly all sparse columns to agree. Columns use a slightly
        # lower threshold so full-height sidebars survive fixed headers/footers
        # and small animated controls within the sidebar.
        self._row_observations.append(unchanged.mean(axis=1) >= 0.97)
        self._column_observations.append(unchanged.mean(axis=0) >= 0.90)

    def row_mask(self) -> np.ndarray:
        if not self.ready:
            return np.zeros(self.height, dtype=bool)
        consensus = np.mean(np.stack(self._row_observations), axis=0) >= 0.80
        return _edge_mask(consensus, min_band=4)

    def column_mask(self) -> np.ndarray:
        if not self.ready:
            return np.zeros(self.sample_columns, dtype=bool)
        consensus = np.mean(
            np.stack(self._column_observations), axis=0
        ) >= 0.80
        return _edge_mask(consensus, min_band=2)

    def bands(self, width: int) -> FixedBands:
        rows = self.row_mask()
        columns = self.column_mask()
        top = _leading_count(rows)
        bottom = _leading_count(rows[::-1])
        left_samples = _leading_count(columns)
        right_samples = _leading_count(columns[::-1])
        return FixedBands(
            top=top,
            bottom=bottom,
            left=_sample_boundary(width, self.sample_columns, left_samples),
            right=_sample_boundary(width, self.sample_columns, right_samples),
        )


def _edge_mask(values: np.ndarray, *, min_band: int) -> np.ndarray:
    mask = np.zeros_like(values, dtype=bool)
    if values.size == 0:
        return mask
    leading = _leading_count(values)
    trailing = _leading_count(values[::-1])
    # Never classify most of the viewport as fixed. A mostly stable screen is
    # a pause in scrolling, not four giant pieces of window chrome.
    max_band = max(min_band, int(values.size * 0.45))
    if min_band <= leading <= max_band:
        mask[:leading] = True
    if min_band <= trailing <= max_band:
        mask[values.size - trailing:] = True
    if mask.all():
        mask[:] = False
    return mask


def _leading_count(values: np.ndarray) -> int:
    count = 0
    for value in values:
        if not bool(value):
            break
        count += 1
    return count


def _sample_boundary(width: int, samples: int, count: int) -> int:
    if width <= 0 or samples <= 0 or count <= 0:
        return 0
    if count >= samples:
        return width
    xs = (
        np.arange(samples, dtype=np.int64) * (width - 1) // max(samples - 1, 1)
    )
    return int((xs[count - 1] + xs[count] + 1) // 2)
