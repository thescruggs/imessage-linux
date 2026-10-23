#!/usr/bin/env python3
"""UI icons for the portable client (crates/imsg-app/assets/icons/).

The Slint software renderer has no icon theme and no SVG path support in
our build, so the toolbar / composer / transcript icons are small
white-on-transparent PNGs that the UI tints with `colorize` (light and
dark theme alike). 48x48 (shown at 16-24 px, so @2x), drawn at 8x and
downsampled for smooth edges. Sibling of tools/gen-tapback-icons.py.

    python3 tools/gen-ui-icons.py        # needs Pillow
"""

import math
import os

from PIL import Image, ImageDraw

SIZE = 48
SS = 8  # supersampling
W = SIZE * SS
WHITE = (255, 255, 255, 255)
CLEAR = (0, 0, 0, 0)

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "crates", "imsg-app", "assets", "icons")


def canvas():
    img = Image.new("RGBA", (W, W), CLEAR)
    return img, ImageDraw.Draw(img)


def u(v):
    """Design units (0-48) -> supersampled pixels."""
    return round(v * SS)


def pt(x, y):
    return (u(x), u(y))


def finish(img, name):
    img = img.resize((SIZE, SIZE), Image.LANCZOS)
    img.save(os.path.join(OUT, f"{name}.png"), optimize=True)


def stroke(d, pts, width, fill=WHITE):
    """A polyline with round joins and caps."""
    w = u(width)
    pts = [pt(*p) for p in pts]
    d.line(pts, fill=fill, width=w, joint="curve")
    for p in (pts[0], pts[-1]):
        d.ellipse((p[0] - w / 2, p[1] - w / 2, p[0] + w / 2, p[1] + w / 2), fill=fill)


def arc_pts(cx, cy, r, a0, a1, n=48):
    """Points on a circle arc, angles in degrees (0 = right, 90 = down)."""
    return [(cx + r * math.cos(math.radians(a0 + (a1 - a0) * i / n)), cy + r * math.sin(math.radians(a0 + (a1 - a0) * i / n))) for i in range(n + 1)]


def paperclip():
    img, d = canvas()
    # A paperclip as one continuous stroke, drawn upright then rotated.
    pts = []
    pts += [(29, 16), (29, 33)]
    pts += arc_pts(24, 33, 5, 0, 180)[1:]
    pts += [(19, 13)]
    pts += arc_pts(26.5, 13, 7.5, 180, 360)[1:]
    pts += [(34, 35)]
    pts += arc_pts(24, 35, 10, 0, 180)[1:]
    pts += [(14, 18)]
    stroke(d, pts, 3.2)
    return img.rotate(-40, resample=Image.BICUBIC, center=(W / 2, W / 2))


def send():
    """Paper plane pointing right (as Breeze's document-send)."""
    img, d = canvas()
    d.polygon([pt(5, 7), pt(44, 24), pt(5, 41), pt(10, 24)], fill=WHITE)
    # The fold: a thin transparent line from the tail to the nose.
    d.line([pt(10, 24), pt(30, 24)], fill=CLEAR, width=u(2.6))
    return img


def compose():
    """A pencil (new message)."""
    img, d = canvas()
    # Upright pencil, then rotated 45 degrees.
    d.rounded_rectangle((u(19), u(4), u(29), u(10)), radius=u(2), fill=WHITE)
    d.rectangle((u(19), u(12), u(29), u(34)), fill=WHITE)
    d.polygon([pt(19, 36), pt(29, 36), pt(24, 45)], fill=WHITE)
    img = img.rotate(-45, resample=Image.BICUBIC, center=(W / 2, W / 2))
    d = ImageDraw.Draw(img)
    # Base line.
    d.rounded_rectangle((u(6), u(42), u(24), u(45)), radius=u(1.5), fill=WHITE)
    return img


def settings():
    """Two slider lines with knobs (kmessages' configure icon)."""
    img, d = canvas()
    for y, x in ((15, 31), (33, 17)):
        stroke(d, [(6, y), (42, y)], 3)
        r = 5.5
        d.ellipse((u(x - r), u(y - r), u(x + r), u(y + r)), fill=WHITE)
        r2 = 2.6
        d.ellipse((u(x - r2), u(y - r2), u(x + r2), u(y + r2)), fill=CLEAR)
    return img


def pin():
    img, d = canvas()
    # Upright push pin, then tilted.
    d.rounded_rectangle((u(16), u(5), u(32), u(9)), radius=u(2), fill=WHITE)
    d.polygon([pt(19, 8), pt(29, 8), pt(31, 22), pt(17, 22)], fill=WHITE)
    d.rounded_rectangle((u(11), u(21), u(37), u(26)), radius=u(2.5), fill=WHITE)
    stroke(d, [(24, 26), (24, 43)], 3)
    return img.rotate(-35, resample=Image.BICUBIC, center=(W / 2, W / 2))


def info():
    img, d = canvas()
    d.ellipse((u(4), u(4), u(44), u(44)), outline=WHITE, width=u(3.4))
    d.ellipse((u(21.5), u(11.5), u(26.5), u(16.5)), fill=WHITE)
    d.rounded_rectangle((u(21.8), u(20), u(26.2), u(36)), radius=u(2), fill=WHITE)
    return img


def download():
    img, d = canvas()
    stroke(d, [(24, 5), (24, 30)], 3.4)
    stroke(d, [(13, 20), (24, 31), (35, 20)], 3.4)
    stroke(d, [(7, 33), (7, 41), (41, 41), (41, 33)], 3.4)
    return img


def file_icon():
    img, d = canvas()
    d.polygon([pt(9, 3), pt(29, 3), pt(40, 14), pt(40, 45), pt(9, 45)], fill=WHITE)
    # The folded corner and a few text lines, cut out.
    d.polygon([pt(28, 3), pt(28, 15), pt(40, 15)], fill=CLEAR)
    d.polygon([pt(30.5, 6), pt(30.5, 12.5), pt(37, 12.5)], fill=WHITE)
    for y in (22, 28, 34):
        d.rectangle((u(14), u(y), u(35), u(y + 2.2)), fill=CLEAR)
    return img


def person(img=None, d=None, cx=24, top=6, scale=1.0):
    own = img is None
    if own:
        img, d = canvas()
    r = 8 * scale
    d.ellipse((u(cx - r), u(top), u(cx + r), u(top + 2 * r)), fill=WHITE)
    w, h = 15 * scale, 13 * scale
    y = top + 2 * r + 3 * scale
    d.pieslice((u(cx - w), u(y), u(cx + w), u(y + 2 * h)), 180, 360, fill=WHITE)
    return img


def group():
    img, d = canvas()
    # Two people, the back one smaller and offset (Breeze system-users).
    person(img, d, cx=31, top=7, scale=0.8)
    # Separate the front person from the back one with a clear outline.
    big = Image.new("RGBA", (W, W), CLEAR)
    bd = ImageDraw.Draw(big)
    person(big, bd, cx=19, top=12, scale=1.0)
    mask = big.split()[3].point(lambda a: 255 if a > 0 else 0)
    grown = mask.resize((W, W))
    from PIL import ImageFilter
    grown = grown.filter(ImageFilter.MaxFilter(size=u(3) | 1))
    img.paste(CLEAR, (0, 0), grown)
    img.paste(WHITE, (0, 0), mask)
    return img


def globe():
    img, d = canvas()
    d.ellipse((u(4), u(4), u(44), u(44)), outline=WHITE, width=u(3))
    d.ellipse((u(15), u(4), u(33), u(44)), outline=WHITE, width=u(2.6))
    d.line([pt(5, 24), pt(43, 24)], fill=WHITE, width=u(2.6))
    d.line([pt(9, 14), pt(39, 14)], fill=WHITE, width=u(2.4))
    d.line([pt(9, 34), pt(39, 34)], fill=WHITE, width=u(2.4))
    return img


def search():
    img, d = canvas()
    d.ellipse((u(5), u(5), u(33), u(33)), outline=WHITE, width=u(3.6))
    stroke(d, [(29, 29), (42, 42)], 4.2)
    return img


def warning():
    img, d = canvas()
    d.polygon([pt(24, 4), pt(45, 42), pt(3, 42)], fill=WHITE)
    d.rounded_rectangle((u(22), u(16), u(26), u(31)), radius=u(2), fill=CLEAR)
    d.ellipse((u(21.8), u(33.5), u(26.2), u(37.9)), fill=CLEAR)
    return img


def main():
    os.makedirs(OUT, exist_ok=True)
    finish(paperclip(), "paperclip")
    finish(send(), "send")
    finish(compose(), "compose")
    finish(settings(), "settings")
    finish(pin(), "pin")
    finish(info(), "info")
    finish(download(), "download")
    finish(file_icon(), "file")
    finish(person(), "person")
    finish(group(), "group")
    finish(globe(), "globe")
    finish(search(), "search")
    finish(warning(), "warning")
    print(f"wrote {OUT}")


if __name__ == "__main__":
    main()
