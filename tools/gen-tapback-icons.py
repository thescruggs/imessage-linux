#!/usr/bin/env python3
"""Tapback icons for the portable client (crates/imsg-app/assets/tapbacks/).

Slint's software renderer cannot draw colour emoji, so the six classic
tapbacks (and the "react" button) are small white-on-transparent PNGs
that the UI tints with `colorize`. 48x48 (shown at 16-24 px, so @2x).
Drawn at 8x and downsampled for smooth edges; text glyphs use the
embedded Noto Sans Bold so the output does not depend on system fonts.

    python3 tools/gen-tapback-icons.py        # needs Pillow
"""

import math
import os
import sys

from PIL import Image, ImageDraw, ImageFont

SIZE = 48
SS = 8  # supersampling
W = SIZE * SS
WHITE = (255, 255, 255, 255)

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "crates", "imsg-app", "assets", "tapbacks")
BOLD = os.path.join(ROOT, "crates", "imsg-app", "assets", "fonts", "NotoSans-Bold.ttf")


def canvas():
    img = Image.new("RGBA", (W, W), (0, 0, 0, 0))
    return img, ImageDraw.Draw(img)


def u(v):
    """Design units (0-48) -> supersampled pixels."""
    return round(v * SS)


def finish(img, name):
    img = img.resize((SIZE, SIZE), Image.LANCZOS)
    img.save(os.path.join(OUT, f"{name}.png"), optimize=True)


def heart():
    img, d = canvas()
    pts = []
    for i in range(720):
        t = 2 * math.pi * i / 720
        x = 16 * math.sin(t) ** 3
        y = 13 * math.cos(t) - 5 * math.cos(2 * t) - 2 * math.cos(3 * t) - math.cos(4 * t)
        pts.append((u(24 + x * 1.25), u(24.5 - y * 1.25)))
    d.polygon(pts, fill=WHITE)
    return img


def thumb(down=False):
    img, d = canvas()
    # Cuff.
    d.rounded_rectangle((u(5), u(21), u(13), u(43)), radius=u(2), fill=WHITE)
    # Palm.
    d.rounded_rectangle((u(15), u(20), u(31), u(43)), radius=u(4), fill=WHITE)
    # Thumb: a thick rounded stroke up and to the right.
    a, b, r = (u(18), u(23)), (u(25), u(6.5)), u(4.6)
    d.line((a, b), fill=WHITE, width=2 * r)
    for c in (a, b):
        d.ellipse((c[0] - r, c[1] - r, c[0] + r, c[1] + r), fill=WHITE)
    d.polygon([(u(15), u(24)), (u(22), u(14)), (u(31), u(20)), (u(31), u(26))], fill=WHITE)
    # Four fingers, separated by thin gaps.
    for i, (x1, y) in enumerate([(41, 20), (42.5, 26), (41.5, 32), (39, 38)]):
        d.rounded_rectangle((u(24), u(y), u(x1), u(y + 5.2)), radius=u(2.6), fill=WHITE)
    for y in (25.6, 31.6, 37.6):
        d.line(((u(30), u(y)), (u(44), u(y))), fill=(0, 0, 0, 0), width=u(0.9))
    if down:
        img = img.transpose(Image.FLIP_TOP_BOTTOM)
    return img


def text(lines, size, spacing=0.0):
    img, d = canvas()
    font = ImageFont.truetype(BOLD, u(size))
    boxes = [d.textbbox((0, 0), s, font=font) for s in lines]
    heights = [b[3] - b[1] for b in boxes]
    total = sum(heights) + u(spacing) * (len(lines) - 1)
    y = (W - total) / 2
    for s, b, h in zip(lines, boxes, heights):
        x = (W - (b[2] - b[0])) / 2 - b[0]
        d.text((x, y - b[1]), s, font=font, fill=WHITE)
        y += h + u(spacing)
    return img


def smiley():
    """The hover "React" button: a smiley outline."""
    img, d = canvas()
    d.ellipse((u(5), u(5), u(43), u(43)), outline=WHITE, width=u(3.6))
    for x in (17.5, 30.5):
        d.ellipse((u(x - 3), u(15.5), u(x + 3), u(21.5)), fill=WHITE)
    d.arc((u(14), u(16), u(34), u(35)), start=20, end=160, fill=WHITE, width=u(3.6))
    return img


def main():
    if not os.path.exists(BOLD):
        sys.exit(f"missing {BOLD}")
    os.makedirs(OUT, exist_ok=True)
    finish(heart(), "love")
    finish(thumb(), "like")
    finish(thumb(down=True), "dislike")
    finish(text(["HA", "HA"], 22, spacing=2.5), "laugh")
    finish(text(["!!"], 44), "emphasize")
    finish(text(["?"], 46), "question")
    finish(smiley(), "react")
    print(f"wrote {OUT}")


if __name__ == "__main__":
    main()
