#!/usr/bin/env python3
"""Render TestBackend's LAYOUT_SPANS records as PNGs for visual inspection.

Usage: python3 scripts/preview.py test-output.log output-directory
Pillow is a development-only dependency. Cell positions/colours come from Rust.
"""
import json
from pathlib import Path
import re
import sys
from PIL import Image, ImageDraw, ImageFont

source, destination = map(Path, sys.argv[1:])
destination.mkdir(parents=True, exist_ok=True)
font = ImageFont.truetype("/usr/share/fonts/noto/NotoSansMono-Regular.ttf", 14)


def color(text, fallback):
    match = re.fullmatch(r"Rgb\((\d+), (\d+), (\d+)\)", text)
    return tuple(map(int, match.groups())) if match else fallback


for line in source.read_text().splitlines():
    if not line.startswith("LAYOUT_SPANS "):
        continue
    _, size, encoded = line.split(" ", 2)
    columns, rows = map(int, size.split("x"))
    cells = json.loads(encoded)
    image = Image.new("RGB", (columns * 10 + 24, rows * 20 + 40), (18, 18, 24))
    draw = ImageDraw.Draw(image)
    draw.text((12, 3), size, font=font, fill=(150, 150, 160))
    for y, row in enumerate(cells):
        for x, (symbol, fg, bg, modifiers) in enumerate(row):
            left, top = 12 + x * 10, 28 + y * 20
            foreground = color(fg, (200, 200, 210))
            background = color(bg, (18, 18, 24))
            draw.rectangle((left, top, left + 9, top + 19), fill=background)
            if len(symbol) == 1 and 0x2800 <= ord(symbol) <= 0x28ff:
                mask = ord(symbol) - 0x2800
                for dx, bits in enumerate(((0, 1, 2, 6), (3, 4, 5, 7))):
                    for dy, bit in enumerate(bits):
                        if mask & (1 << bit):
                            xx, yy = left + 1 + dx * 5, top + 2 + dy * 4
                            draw.ellipse((xx, yy, xx + 1, yy + 1), fill=foreground)
            else:
                draw.text((left, top), symbol, font=font, fill=foreground,
                          stroke_width=0)
    output = destination / (size + ".png")
    image.save(output)
    print(output)
