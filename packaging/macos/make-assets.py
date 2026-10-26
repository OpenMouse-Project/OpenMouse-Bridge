#!/usr/bin/env python3
"""Regenerates the committed macOS artwork from assets/openmouse-app-icon.png:
icon-1024.png (app icon on a macOS-style tile) and the DMG window background
(dmg-background.tiff, 1x + 2x). Needs Pillow and macOS' tiffutil and Menlo.

    python3 packaging/macos/make-assets.py
"""
import subprocess
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter, ImageFont

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
SOURCE = REPO / "assets/openmouse-app-icon.png"
GREEN = (107, 208, 140)

# Content area. The Finder frame adds a 32pt title bar and, when shown, a 28pt
# path bar, so key content stays in the top 400pt and dmg-settings.py asks
# for a 460pt-tall frame.
WINDOW = (660, 428)
ICON_Y = 185  # keep in sync with dmg-settings.py
APP_X, APPS_X = 170, 490


def superellipse_mask(size, margin, n=5.0):
    """macOS-style continuous-corner tile, antialiased by 4x supersampling."""
    scale = 4
    big = size * scale
    half = (big - 2 * margin * scale) / 2
    center = big / 2
    mask = Image.new("L", (big, big), 0)
    pixels = mask.load()
    for y in range(big):
        dy = abs(y - center) / half
        if dy > 1:
            continue
        span = (1 - dy**n) ** (1 / n)
        x0 = int(center - half * span)
        x1 = int(center + half * span)
        for x in range(max(x0, 0), min(x1, big)):
            pixels[x, y] = 255
    return mask.resize((size, size), Image.LANCZOS)


def make_icon():
    size = 1024
    tile = superellipse_mask(size, margin=100)

    gradient = Image.new("RGB", (size, size))
    draw = ImageDraw.Draw(gradient)
    for y in range(size):
        t = y / (size - 1)
        draw.line(
            [(0, y), (size, y)],
            fill=(int(34 - 16 * t), int(38 - 18 * t), int(46 - 22 * t)),
        )

    icon = Image.new("RGBA", (size, size), (0, 0, 0, 0))

    shadow = Image.new("L", (size, size), 0)
    shadow.paste(tile, (0, 12))
    shadow = shadow.filter(ImageFilter.GaussianBlur(14)).point(lambda v: v * 0.45)
    icon.paste(Image.new("RGBA", (size, size), (0, 0, 0, 255)), mask=shadow)
    icon.paste(gradient, mask=tile)

    mouse = Image.open(SOURCE).convert("RGBA")
    mouse = mouse.crop(mouse.getbbox())
    height = 520
    mouse = mouse.resize((round(mouse.width * height / mouse.height), height), Image.LANCZOS)
    icon.alpha_composite(mouse, ((size - mouse.width) // 2, (size - mouse.height) // 2))
    icon.save(HERE / "icon-1024.png")
    return icon


BANNER = r"""
  ___                 __  __
 / _ \ _ __  ___ _ _ |  \/  |___ _  _ ___ ___
| (_) | '_ \/ -_) ' \| |\/| / _ \ || (_-</ -_)
 \___/| .__/\___|_||_|_|  |_\___/\_,_/__/\___|
      |_|
""".strip("\n")

MONO = "/System/Library/Fonts/Menlo.ttc"
DIM = (62, 70, 80)
TEXT = (184, 191, 201)


def background(scale):
    width, height = WINDOW[0] * scale, WINDOW[1] * scale
    image = Image.new("RGB", (width, height))
    draw = ImageDraw.Draw(image)
    for y in range(height):
        t = y / (height - 1)
        draw.line([(0, y), (width, y)], fill=(int(14 - 6 * t), int(16 - 7 * t), int(20 - 9 * t)))

    def mono(px):
        return ImageFont.truetype(MONO, int(px * scale))

    def text_width(text, f):
        return draw.textlength(text, font=f)

    def centered(text, y, f, fill):
        draw.text(((width - text_width(text, f)) / 2, y * scale), text, font=f, fill=fill)

    # Frame rules.
    rule = mono(10)
    cols = int(width / text_width("-", rule)) - 2
    border = "+" + "-" * cols + "+"
    centered(border, 10, rule, DIM)
    centered(border, 380, rule, DIM)

    # Banner, drawn line by line so the glyph grid stays aligned.
    f = mono(12)
    lines = BANNER.split("\n")
    block = max(text_width(line, f) for line in lines)
    x = (width - block) / 2
    for i, line in enumerate(lines):
        draw.text((x, (30 + i * 14.5) * scale), line, font=f, fill=GREEN)
    centered("B R I D G E   //   HID  COMPANION", 110, mono(10), DIM)

    # Arrow: chevrons brightening toward the Applications icon.
    f = mono(26)
    chevron = text_width(">", f)
    gap = 8 * scale
    count = 5
    total = count * chevron + (count - 1) * gap
    x = ((APP_X + APPS_X) * scale - total) / 2
    for i in range(count):
        t = (i + 1) / count
        color = tuple(int(DIM[c] + (GREEN[c] - DIM[c]) * t) for c in range(3))
        draw.text((x + i * (chevron + gap), (ICON_Y - 19) * scale), ">", font=f, fill=color)

    # Finder always draws DMG icon labels in black over a picture background,
    # so give them light chips to sit on (label centre is ~74pt below the icon).
    chip_w, chip_h, chip_y = 140, 22, ICON_Y + 74
    for cx in (APP_X, APPS_X):
        draw.rounded_rectangle(
            [(cx - chip_w / 2) * scale, (chip_y - chip_h / 2) * scale,
             (cx + chip_w / 2) * scale, (chip_y + chip_h / 2) * scale],
            radius=chip_h / 2 * scale,
            fill=(226, 231, 238),
        )

    # Terminal-style caption with a cursor block.
    f = mono(12)
    caption = "drag OpenMouse Bridge.app -> /Applications"
    prompt = "$ "
    cursor = 8 * scale
    total = text_width(prompt + caption, f) + 6 * scale + cursor
    x = (width - total) / 2
    y = 330 * scale
    draw.text((x, y), prompt, font=f, fill=GREEN)
    draw.text((x + text_width(prompt, f), y), caption, font=f, fill=TEXT)
    cx = x + text_width(prompt + caption, f) + 6 * scale
    draw.rectangle([cx, y + 1 * scale, cx + cursor, y + 15 * scale], fill=GREEN)
    return image


def main():
    make_icon()
    one, two = background(1), background(2)
    one.save(HERE / "bg.png", dpi=(72, 72))
    two.save(HERE / "bg@2x.png", dpi=(144, 144))
    subprocess.run(
        ["tiffutil", "-cathidpicheck", HERE / "bg.png", HERE / "bg@2x.png",
         "-out", HERE / "dmg-background.tiff"],
        check=True,
    )
    (HERE / "bg.png").unlink()
    (HERE / "bg@2x.png").unlink()


if __name__ == "__main__":
    main()
