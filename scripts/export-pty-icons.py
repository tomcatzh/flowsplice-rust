#!/usr/bin/env python3
"""Resize generated PTY artwork into platform assets; requires Pillow.

This only packages the checked-in masters. It does not generate or redraw artwork.
Run from the repository root: python3 scripts/export-pty-icons.py
"""
import json
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
ART = ROOT / "assets/brand/pty"
CATALOG = ROOT / "pty-apple/Assets.xcassets"
ANDROID = ROOT / "pty-android/app/src/main/res"


def metadata(path, images=None):
    data = {"info": {"author": "xcode", "version": 1}}
    if images is not None:
        data["images"] = images
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2) + "\n")


def export(source, path, size, mode):
    path.parent.mkdir(parents=True, exist_ok=True)
    source.convert(mode).resize((size, size), Image.Resampling.LANCZOS).save(path)


def main():
    with Image.open(ART / "pty-mark.png") as mark, Image.open(ART / "pty-icon.png") as icon:
        if mark.width != mark.height or icon.width != icon.height:
            raise ValueError("PTY artwork must be square")
        if icon.convert("RGBA").getchannel("A").getextrema() != (255, 255):
            raise ValueError("The iOS master must be opaque; edit it with imagegen first")

        # Adaptive launcher: 108 dp canvas at 4x, with inset for Android's safe zone.
        # Letterboxing is platform sizing only; the generated mark stays unchanged.
        foreground = Image.new("RGBA", (432, 432))
        inset_mark = mark.convert("RGBA").resize((336, 336), Image.Resampling.LANCZOS)
        foreground.paste(inset_mark, (48, 48))
        foreground_path = ANDROID / "drawable-nodpi/pty_icon_foreground.png"
        foreground_path.parent.mkdir(parents=True, exist_ok=True)
        foreground.save(foreground_path)
        metadata(CATALOG / "Contents.json")
        ios = CATALOG / "AppIcon-iOS.appiconset"
        export(icon, ios / "AppIcon.png", 1024, "RGB")
        metadata(ios / "Contents.json", [{
            "filename": "AppIcon.png", "idiom": "universal",
            "platform": "ios", "size": "1024x1024",
        }])

        mac = CATALOG / "AppIcon-macOS.appiconset"
        images = []
        for points in (16, 32, 128, 256, 512):
            for scale in (1, 2):
                filename = f"AppIcon-{points}{'@2x' if scale == 2 else ''}.png"
                export(mark, mac / filename, points * scale, "RGBA")
                images.append({"filename": filename, "idiom": "mac", "size": f"{points}x{points}",
                               "scale": f"{scale}x"})
        metadata(mac / "Contents.json", images)
    print("Exported the shared PTY identity for Android, iOS/iPadOS and macOS")


if __name__ == "__main__":
    main()
