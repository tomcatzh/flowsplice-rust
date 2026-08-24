#!/usr/bin/env python3
"""Generate FlowSplice raster and browser icon exports from the approved geometry."""

from __future__ import annotations

import math
import shutil
from pathlib import Path

from PIL import Image, ImageDraw


ROOT = Path(__file__).resolve().parents[1]
INK = "#07110F"
MINT = "#70D8B6"
MASTER = 108


def cubic(p0, p1, p2, p3, steps=20):
    points = []
    for index in range(1, steps + 1):
        t = index / steps
        u = 1 - t
        points.append(
            (
                u**3 * p0[0] + 3 * u**2 * t * p1[0] + 3 * u * t**2 * p2[0] + t**3 * p3[0],
                u**3 * p0[1] + 3 * u**2 * t * p1[1] + 3 * u * t**2 * p2[1] + t**3 * p3[1],
            )
        )
    return points


def route_points():
    points = [(81, 30), (51, 30)]
    points += cubic(points[-1], (36, 30), (27, 36), (27, 46))
    points += cubic(points[-1], (27, 55), (35, 59), (48, 59))
    points.append((60, 59))
    points += cubic(points[-1], (73, 59), (81, 63), (81, 72))
    points += cubic(points[-1], (81, 81), (72, 82), (57, 82))
    points.append((27, 82))
    return points


def render(size: int) -> Image.Image:
    scale = max(4, math.ceil(size / MASTER) * 4)
    canvas_size = size * scale
    image = Image.new("RGB", (canvas_size, canvas_size), INK)
    draw = ImageDraw.Draw(image)
    factor = canvas_size / MASTER
    points = [(round(x * factor), round(y * factor)) for x, y in route_points()]
    width = round(12 * factor)
    draw.line(points, fill=MINT, width=width, joint="curve")
    radius = width / 2
    for x, y in (points[0], points[-1]):
        draw.ellipse((x - radius, y - radius, x + radius, y + radius), fill=MINT)
    return image.resize((size, size), Image.Resampling.LANCZOS)


def save_png(path: Path, size: int) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    render(size).save(path, format="PNG", optimize=True)


def export_web(public: Path) -> None:
    public.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(ROOT / "assets/brand/flowsplice-icon.svg", public / "favicon.svg")
    shutil.copyfile(ROOT / "assets/brand/flowsplice-monochrome.svg", public / "safari-pinned-tab.svg")
    for size in (16, 32, 48):
        save_png(public / f"favicon-{size}x{size}.png", size)
    icon = render(512)
    icon.save(public / "favicon.ico", format="ICO", sizes=[(16, 16), (32, 32), (48, 48)])
    for size in (152, 167, 180):
        save_png(public / f"apple-touch-icon-{size}x{size}.png", size)
    shutil.copyfile(public / "apple-touch-icon-180x180.png", public / "apple-touch-icon.png")
    for size in (192, 512):
        save_png(public / f"icon-{size}x{size}.png", size)
        save_png(public / f"icon-maskable-{size}x{size}.png", size)


def export_android_legacy() -> None:
    res = ROOT / "travel-android/app/src/main/res"
    densities = {"mdpi": 48, "hdpi": 72, "xhdpi": 96, "xxhdpi": 144, "xxxhdpi": 192}
    for density, size in densities.items():
        folder = res / f"mipmap-{density}"
        folder.mkdir(parents=True, exist_ok=True)
        image = render(size)
        for name in ("ic_launcher.webp", "ic_launcher_round.webp"):
            image.save(folder / name, format="WEBP", lossless=True, method=6)


def main() -> None:
    for app in ("homeagent", "travelagent"):
        export_web(ROOT / app / "web/public")
    export_android_legacy()


if __name__ == "__main__":
    main()
