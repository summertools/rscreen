#!/usr/bin/env python3
"""Render a frame dumped by examples/snapshot.rs into a PNG.

Usage: render_screenshot.py frame.json out.png [--scale 1]
Requires Pillow and DejaVu Sans Mono.
"""
import json
import sys
from PIL import Image, ImageDraw, ImageFont

CW, CH = 10, 22
FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
FONT_BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf"
FONT_ITALIC = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Oblique.ttf"
DEFAULT_FG = (205, 214, 244)
DEFAULT_BG = (30, 30, 46)
CURSOR = (137, 180, 250)

BASE16 = [
    (69, 71, 90), (243, 139, 168), (166, 227, 161), (249, 226, 175),
    (137, 180, 250), (203, 166, 247), (148, 226, 213), (186, 194, 222),
    (88, 91, 112), (243, 139, 168), (166, 227, 161), (249, 226, 175),
    (137, 180, 250), (203, 166, 247), (148, 226, 213), (205, 214, 244),
]


def indexed(i):
    if i < 16:
        return BASE16[i]
    if 16 <= i <= 231:
        i -= 16
        r = i // 36
        g = (i % 36) // 6
        b = i % 6
        conv = lambda v: 0 if v == 0 else 55 + v * 40
        return (conv(r), conv(g), conv(b))
    v = 8 + (i - 232) * 10
    return (v, v, v)


def color(v, default):
    if v is None:
        return default
    if isinstance(v, str) and v.startswith("#"):
        return tuple(int(v[i:i + 2], 16) for i in (1, 3, 5))
    if isinstance(v, str) and v.startswith("idx:"):
        return indexed(int(v[4:]))
    return default


def main():
    frame_path, out_path = sys.argv[1], sys.argv[2]
    scale = 1
    if "--scale" in sys.argv:
        scale = int(sys.argv[sys.argv.index("--scale") + 1])
    frame = json.load(open(frame_path))
    cols, rows = frame["cols"], frame["rows"]
    cw, ch = CW * scale, CH * scale
    img = Image.new("RGB", (cols * cw, rows * ch), DEFAULT_BG)
    draw = ImageDraw.Draw(img)
    fonts = {}
    for size in (15 * scale, 16 * scale):
        fonts[size] = ImageFont.truetype(FONT, size)
        fonts[(size, "b")] = ImageFont.truetype(FONT_BOLD, size)
        fonts[(size, "i")] = ImageFont.truetype(FONT_ITALIC, size)
    font = fonts[15 * scale]

    for y, row in enumerate(frame["cells"]):
        x = 0
        while x < cols:
            cell = row[x]
            if cell == 0:
                x += 1
                continue
            fg = color(cell["fg"], DEFAULT_FG)
            bg = color(cell["bg"], DEFAULT_BG)
            attrs = cell.get("a", 0)
            if attrs & 32:  # reverse
                fg, bg = bg, fg
            draw.rectangle(
                [x * cw, y * ch, (x + 2) * cw - 1, (y + 1) * ch - 1],
                fill=bg,
            )
            text = cell["c"] + cell.get("x", "")
            if not (attrs & 64):  # hidden
                weight = "b" if attrs & 1 else ("i" if attrs & 4 else None)
                f = fonts.get((15 * scale, weight), font) if weight else font
                draw.text((x * cw, y * ch + 2 * scale), text, font=f, fill=fg)
            if attrs & 8 or attrs & 256 or attrs & 512:
                draw.line(
                    [x * cw, (y + 1) * ch - 3 * scale, (x + 2) * cw, (y + 1) * ch - 3 * scale],
                    fill=fg,
                    width=max(1, scale),
                )
            x += 2 if cell.get("w") else 1

    cur = frame.get("cursor")
    if cur:
        cx, cy = cur
        draw.rectangle(
            [cx * cw, cy * ch, (cx + 1) * cw - 1, (cy + 1) * ch - 1],
            outline=CURSOR,
            width=max(2, scale * 2),
        )
    img.save(out_path)
    print(out_path)


if __name__ == "__main__":
    main()
