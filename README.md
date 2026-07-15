# pngshot

Wayland region-screenshot tool for niri. Select a region, then act on it from a
toolbar that hovers right under the selection: copy, save, annotate, OCR,
LLM-translate, pin to the desktop, or capture a long (scrolling) screenshot.

Built for: Arch Linux + niri (Wayland), GTK4 + gtk4-layer-shell.

## Features

- **Region screenshot** — drag to select, 8 resize handles, drag-to-move.
- **Toolbar under the selection** — pixel-accurate because the whole selection
  stage is one fullscreen `wlr-layer-shell` overlay (not a separate window).
- **Confirm → clipboard** (`wl-copy -t image/png`), **save**, **cancel**.
- **OCR** — `tesseract -l chi_sim+eng`, result shown in an editable window.
- **Translate** — routes to a local opencode free model
  (`opencode run --format json`), OpenAI-compatible fallback via `OPENAI_API_KEY`.
- **Annotate** — pen / arrow / rectangle / text, color + width cycling, undo.
  Text renders through Pango so CJK works.
- **Deskpin** — a borderless floating window (niri "always on top" == floating):
  - scroll wheel = zoom the image content (window size fixed, anchored at cursor)
  - Ctrl + scroll = scale the window itself
  - drag anywhere = move the window
  - `c` copy, `s` save, `0` reset zoom, `q`/`Esc` close, right-click menu
- **Long screenshot** — semi-automatic. Wayland forbids synthetic scrolling, so
  you scroll the target window manually while pngshot samples the region on a
  timer and stitches frames with OpenCV template matching. `Space` forces a
  frame, `Enter` finishes, `Esc` cancels.

## Dependencies (all pacman)

```
grim wl-clipboard tesseract tesseract-data-chi_sim tesseract-data-eng
python-gobject python-cairo gtk4-layer-shell python-pillow python-opencv
python-numpy
```

`opencode` on PATH is used for translation (free model, no login required).

## Install

The launcher is already linked at `~/.local/bin/pngshot` →
`scripts/pngshot`, which sets `PYTHONPATH` and (importantly) `LD_PRELOAD` for
gtk4-layer-shell:

```sh
# manual run
pngshot region
```

> **Note on LD_PRELOAD**: PyGObject links libwayland before gtk4-layer-shell,
> which breaks the layer surface. The launcher preloads
> `/usr/lib/libgtk4-layer-shell.so` to fix this — see
> <https://github.com/wmww/gtk4-layer-shell/blob/main/linking.md>.

## niri integration

See `contrib/niri-pngshot.kdl` for a window-rule (keeps pins clean) and
keybinds. Nothing is applied automatically; merge what you want into
`~/.config/niri/config.kdl`.

Suggested binds:

| Key | Action |
|-----|--------|
| `Print` | `pngshot region` |
| `Shift+Print` | `pngshot long` |
| `Mod+Print` | `pngshot pin-last` |

## Config

Optional `~/.config/pngshot/config.toml` (see `config.toml.example`) overrides
the LLM model/target language, OCR languages, and long-shot tuning
(`poll_ms`, `match_thresh`, `probe_height`, `min_shift_px`).

## CLI

| Command | What |
|---------|------|
| `pngshot region` | interactive region screenshot |
| `pngshot long` | region → long-shot mode |
| `pngshot pin-last` | pin the current clipboard image |
| `pngshot debug-capture` | grab full screen, copy (smoke test) |

Internal (spawned by the overlay, not for direct use): `pin-file`,
`text-file`, both with `--cleanup`.

## Long-shot limitations

- **Vertical scroll only.** Horizontal movement breaks matching.
- **Sticky headers/footers repeat.** Frame the scrolling content, not the bars.
- **Animated content** between frames lowers the match score; low-confidence
  frames are rejected and the status bar asks you to scroll back a little.

## Architecture

```
pngshot/
  __main__.py        CLI (region / long / pin-last / pin-file / text-file / debug)
  capture.py         grim wrapper (full / output / region)
  config.py          ~/.config/pngshot/config.toml loader
  overlay/           Stage 1: fullscreen layer-shell overlay
    surface.py         layer-shell window, Cairo render, event dispatch
    selector.py        selection rect + resize/move state machine
    toolbar.py         region + annotate toolbars (Pango text)
    annotate.py        pen/arrow/rect/text strokes + bake-to-image
    model.py           Rect, Mode, handle geometry
    app.py             capture → overlay → action pipeline; long-shot handoff
  pin/window.py      Deskpin floating window (content/window zoom, move)
  longshot/
    recorder.py        timed region sampling + control bar
    stitcher.py        OpenCV vertical stitch with overlap re-check
  services/
    clipboard.py       wl-copy / wl-paste
    ocr.py             tesseract + CJK space cleanup
    llm.py             opencode run (json) + OpenAI fallback
    saver.py           ~/Pictures/Screenshots
  util/
    niri.py            niri msg action helpers (float, etc.)
    imaging.py         PIL <-> Cairo surface
    result_win.py      OCR/translate result window
```
