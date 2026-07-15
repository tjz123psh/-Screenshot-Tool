"""Shared imaging helpers."""
from __future__ import annotations

import cairo
from PIL import Image


def pil_to_cairo_surface(img: Image.Image) -> tuple[cairo.ImageSurface, bytearray]:
    """PIL RGBA -> Cairo ARGB32 (BGRA in memory on little-endian).

    Returns ``(surface, backing_buffer)``. The caller MUST keep a reference to
    the buffer alive as long as the surface is used — ``create_for_data`` does
    not copy the underlying bytes.
    """
    if img.mode != "RGBA":
        img = img.convert("RGBA")
    w, h = img.size
    r, g, b, a = img.split()
    bgra_bytes = Image.merge("RGBA", (b, g, r, a)).tobytes()
    stride = cairo.ImageSurface.format_stride_for_width(cairo.FORMAT_ARGB32, w)
    if stride == w * 4:
        data = bytearray(bgra_bytes)
    else:
        data = bytearray(stride * h)
        row_bytes = w * 4
        for row in range(h):
            src = row * row_bytes
            dst = row * stride
            data[dst:dst + row_bytes] = bgra_bytes[src:src + row_bytes]
    surface = cairo.ImageSurface.create_for_data(
        memoryview(data), cairo.FORMAT_ARGB32, w, h, stride
    )
    return surface, data
