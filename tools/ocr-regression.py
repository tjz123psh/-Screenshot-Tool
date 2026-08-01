#!/usr/bin/env python3
"""Generate synthetic difficult-text scenes and score vellum's local OCR.

No user screenshots are read. Fixtures are deterministic and live in a temporary
folder unless --output-dir is supplied explicitly.
"""

from __future__ import annotations

import argparse
import json
import random
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Callable

try:
    from PIL import Image, ImageDraw, ImageFont
except ImportError as error:  # pragma: no cover - environment preflight
    print(f"unavailable: Pillow is required ({error})", file=sys.stderr)
    raise SystemExit(2)

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_PROBE = ROOT / "target/release/examples/ocr_probe"
TRUTH = "Vellum OCR 2026\n暗淡彩色文字识别"
BANNER_TRUTH = "Vellum OCR 暗淡文字 2026"
FONT_CANDIDATES = (
    Path("/usr/share/fonts/adobe-source-han-sans/SourceHanSansCN-Regular.otf"),
    Path("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc"),
)


@dataclass(frozen=True)
class Case:
    name: str
    filename: str
    truth: str
    minimum: float
    render: Callable[[Path, Path], None]


@dataclass
class Result:
    fixture: str
    status: int
    accuracy: float
    minimum: float
    elapsed_ms: int
    text: str
    stderr: str
    passed: bool


def font(path: Path, size: int):
    return ImageFont.truetype(str(path), size=size)


def draw_two_lines(
    image: Image.Image,
    font_path: Path,
    colors: tuple[tuple[int, int, int], tuple[int, int, int]] | tuple[int, int, int],
    origin: tuple[int, int] = (38, 26),
    sizes: tuple[int, int] = (38, 36),
) -> None:
    draw = ImageDraw.Draw(image)
    first_color, second_color = colors if isinstance(colors[0], tuple) else (colors, colors)
    x, y = origin
    draw.text((x, y), "Vellum OCR 2026", font=font(font_path, sizes[0]), fill=first_color)
    draw.text((x, y + 58), "暗淡彩色文字识别", font=font(font_path, sizes[1]), fill=second_color)


def clean(path: Path, font_path: Path) -> None:
    image = Image.new("RGB", (720, 170), (246, 246, 245))
    draw_two_lines(image, font_path, (30, 30, 32))
    image.save(path)


def dim_light(path: Path, font_path: Path) -> None:
    image = Image.new("RGB", (900, 250), (221, 223, 225))
    draw_two_lines(image, font_path, ((192, 195, 198), (190, 194, 197)), (58, 67))
    image.save(path)


def dim_dark(path: Path, font_path: Path) -> None:
    image = Image.new("RGB", (720, 170), (25, 30, 40))
    draw_two_lines(image, font_path, ((102, 109, 124), (98, 106, 121)))
    image.save(path)


def isoluminant(path: Path, font_path: Path) -> None:
    # Both colors round to Rec.601 luma 91; ordinary grayscale erases the text.
    image = Image.new("RGB", (720, 170), (35, 105, 170))
    draw_two_lines(image, font_path, (255, 25, 0))
    image.save(path)


def color_interference(path: Path, font_path: Path) -> None:
    width, height = 720, 190
    rng = random.Random(20260801)
    image = Image.new("RGB", (width, height), (42, 47, 55))
    pixels = image.load()
    for y in range(height):
        for x in range(width):
            if x < 18 or x >= width - 18 or y < 18 or y >= height - 18:
                if rng.random() < 0.08:
                    pixels[x, y] = (
                        rng.randrange(48, 90),
                        rng.randrange(48, 90),
                        rng.randrange(48, 90),
                    )
    draw = ImageDraw.Draw(image)
    draw.rounded_rectangle((20, 18, width - 20, height - 20), radius=28, fill=(68, 75, 88))
    for x in range(-80, width + 80, 42):
        draw.line((x, 18, x + 75, height - 18), fill=(98, 64, 115), width=1)
    draw_two_lines(image, font_path, ((145, 167, 167), (135, 160, 182)), (42, 38))
    image.save(path)


def horizontal_gradient(width: int, height: int, left: int, right: int) -> Image.Image:
    image = Image.new("RGB", (width, height))
    pixels = image.load()
    denominator = max(width - 1, 1)
    for x in range(width):
        value = round(left + (right - left) * x / denominator)
        for y in range(height):
            pixels[x, y] = (value, value, value)
    return image


def faded_gradient_dark(path: Path, font_path: Path) -> None:
    image = horizontal_gradient(760, 190, 135, 205)
    overlay = Image.new("RGBA", image.size, (0, 0, 0, 0))
    draw_two_lines(overlay, font_path, (45, 48, 52), (42, 38))
    image = Image.alpha_composite(image.convert("RGBA"), overlay).convert("RGB")
    image.save(path)


def faded_gradient_light(path: Path, font_path: Path) -> None:
    image = horizontal_gradient(760, 190, 72, 183).convert("RGBA")
    overlay = Image.new("RGBA", image.size, (0, 0, 0, 0))
    draw = ImageDraw.Draw(overlay)
    draw.text((42, 38), "Vellum OCR 2026", font=font(font_path, 38), fill=(225, 228, 232, 92))
    draw.text((42, 96), "暗淡彩色文字识别", font=font(font_path, 36), fill=(225, 228, 232, 92))
    Image.alpha_composite(image, overlay).convert("RGB").save(path)


def faded_colored_dark(path: Path, font_path: Path) -> None:
    image = Image.new("RGB", (720, 180), (48, 64, 86))
    draw_two_lines(image, font_path, (90, 153, 160), (42, 28), sizes=(32, 30))
    image.save(path)


def banner(path: Path, font_path: Path, dim: bool) -> None:
    if dim:
        image = Image.new("RGB", (720, 82), (48, 60, 82))
        color = (88, 128, 134)
    else:
        image = Image.new("RGB", (720, 82), (247, 247, 246))
        color = (32, 32, 34)
    ImageDraw.Draw(image).text((18, 14), BANNER_TRUTH, font=font(font_path, 34), fill=color)
    image.save(path)


def cases() -> list[Case]:
    return [
        Case("clean", "clean.png", TRUTH, 1.0, clean),
        Case("dim-light", "dim-light.png", TRUTH, 1.0, dim_light),
        Case("dim-dark", "dim-dark.png", TRUTH, 1.0, dim_dark),
        Case("isoluminant", "isoluminant.png", TRUTH, 1.0, isoluminant),
        Case("color-interference", "color-interference.png", TRUTH, 0.95, color_interference),
        Case("faded-gradient-dark", "faded-gradient-dark.png", TRUTH, 1.0, faded_gradient_dark),
        Case("faded-gradient-light", "faded-gradient-light.png", TRUTH, 1.0, faded_gradient_light),
        Case("faded-colored-dark", "faded-colored-dark.png", TRUTH, 1.0, faded_colored_dark),
        Case("banner-clean", "banner-clean.png", BANNER_TRUTH, 1.0, lambda p, f: banner(p, f, False)),
        Case("banner-dim-color", "banner-dim-color.png", BANNER_TRUTH, 1.0, lambda p, f: banner(p, f, True)),
    ]


def normalize(text: str) -> str:
    return "".join(character for character in text if character.isalnum())


def levenshtein(left: str, right: str) -> int:
    previous = list(range(len(right) + 1))
    for index, a in enumerate(left, 1):
        current = [index]
        for column, b in enumerate(right, 1):
            current.append(
                min(current[-1] + 1, previous[column] + 1, previous[column - 1] + (a != b))
            )
        previous = current
    return previous[-1]


def score(expected: str, actual: str) -> float:
    expected = normalize(expected)
    actual = normalize(actual)
    return 1.0 - levenshtein(expected, actual) / max(len(expected), len(actual), 1)


def locate_font(explicit: Path | None) -> Path:
    candidates = (explicit,) if explicit else FONT_CANDIDATES
    for candidate in candidates:
        if candidate and candidate.is_file():
            return candidate
    raise RuntimeError("no supported CJK font found; pass --font")


def check_tesseract() -> None:
    executable = shutil.which("tesseract")
    if not executable:
        raise RuntimeError("tesseract is not available on PATH")
    completed = subprocess.run(
        [executable, "--list-langs"], text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False
    )
    if completed.returncode != 0:
        raise RuntimeError(f"tesseract --list-langs failed: {completed.stderr.strip()}")
    available = set(completed.stdout.splitlines()[1:])
    missing = {"chi_sim", "eng"} - available
    if missing:
        raise RuntimeError("missing Tesseract languages: " + ", ".join(sorted(missing)))


def build_probe() -> Path:
    cargo = shutil.which("cargo")
    if not cargo:
        raise RuntimeError("cargo is not available on PATH")
    completed = subprocess.run(
        [
            cargo,
            "build",
            "--locked",
            "--release",
            "-p",
            "vellum-text",
            "--example",
            "ocr_probe",
        ],
        cwd=ROOT,
        check=False,
    )
    if completed.returncode != 0:
        raise RuntimeError(f"probe build failed with status {completed.returncode}")
    if not DEFAULT_PROBE.is_file():
        raise RuntimeError(f"probe build succeeded but {DEFAULT_PROBE} is missing")
    return DEFAULT_PROBE


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--font", type=Path, help="CJK font used to render deterministic fixtures")
    parser.add_argument("--probe", type=Path, help="prebuilt OCR probe; skips cargo build")
    parser.add_argument("--output-dir", type=Path, help="keep generated fixtures in this directory")
    parser.add_argument("--json", action="store_true", help="emit machine-readable results")
    return parser.parse_args()


def run() -> int:
    args = parse_args()
    try:
        font_path = locate_font(args.font)
        try:
            font(font_path, 16)
        except OSError as error:
            raise RuntimeError(f"cannot load CJK font {font_path}: {error}") from error
        check_tesseract()
        probe = args.probe.resolve() if args.probe else build_probe()
        if not probe.is_file() or not probe.stat().st_mode & 0o111:
            raise RuntimeError(f"OCR probe is not executable: {probe}")
    except (OSError, RuntimeError) as error:
        print(f"unavailable: {error}", file=sys.stderr)
        return 2

    temporary = None
    try:
        if args.output_dir:
            output_dir = args.output_dir.resolve()
            output_dir.mkdir(parents=True, exist_ok=True)
        else:
            temporary = tempfile.TemporaryDirectory(prefix="vellum-ocr-regression-")
            output_dir = Path(temporary.name)

        results: list[Result] = []
        for case in cases():
            fixture = output_dir / case.filename
            case.render(fixture, font_path)
            started = time.perf_counter()
            try:
                completed = subprocess.run(
                    [probe, fixture],
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    timeout=35,
                    check=False,
                )
                status = completed.returncode
                stdout = completed.stdout
                stderr = completed.stderr
            except subprocess.TimeoutExpired as error:
                status = 124
                stdout = error.stdout if isinstance(error.stdout, str) else ""
                stderr = error.stderr if isinstance(error.stderr, str) else "OCR probe timed out"
            elapsed_ms = round((time.perf_counter() - started) * 1000)
            accuracy = score(case.truth, stdout)
            passed = status == 0 and accuracy + 1e-9 >= case.minimum
            results.append(
                Result(
                    fixture=case.name,
                    status=status,
                    accuracy=round(accuracy * 100, 1),
                    minimum=round(case.minimum * 100, 1),
                    elapsed_ms=elapsed_ms,
                    text=stdout.strip(),
                    stderr=stderr.strip(),
                    passed=passed,
                )
            )

        if args.json:
            print(json.dumps([asdict(result) for result in results], ensure_ascii=False, indent=2))
        else:
            for result in results:
                print(
                    f"{result.fixture:24} status={result.status:<2} "
                    f"accuracy={result.accuracy:5.1f}% min={result.minimum:5.1f}% "
                    f"elapsed={result.elapsed_ms:5}ms passed={str(result.passed).lower()}"
                )
                if not result.passed:
                    print(f"  text={result.text!r} stderr={result.stderr!r}")
            if args.output_dir:
                print(f"fixtures={output_dir}")

        return 0 if all(result.passed for result in results) else 1
    except OSError as error:
        print(f"unavailable: OCR regression I/O failed ({error})", file=sys.stderr)
        return 2
    finally:
        if temporary:
            temporary.cleanup()


if __name__ == "__main__":
    raise SystemExit(run())
