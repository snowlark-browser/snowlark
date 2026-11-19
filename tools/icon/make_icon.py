# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Draws Snowlark's icon: crates/snowlark/res/snowlark.ico (the executable's and the window's), snowlark.svg (the
About page's) and a preview sheet. Run it after changing the design:

    python tools/icon/make_icon.py [<preview.png>]

The mark is a hexagonal crystal with a hole, an O: snowlark is a blue-grey metal that crystallizes in a
hexagonal lattice. Six facets, lit from the top left, keep it readable down to 16 px, and mid-tone
blues keep it visible on light and dark taskbars. Needs Pillow.
"""
import math
import os
import sys

from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.normpath(os.path.join(HERE, '..', '..', 'crates', 'snowlark', 'res'))
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
# Facets from the top, clockwise: lit from the top left.
FACETS = ['#9FC3EE', '#5E93D6', '#2F62AD', '#1F4A8C', '#3C74C2', '#C9DFF7']
OUTER, INNER = 0.49, 0.235   # radii, as fractions of the side
SMALL_INNER = 0.20           # a smaller hole at small sizes: the ring stays thick


def hexagon(radius, cx=0.5, cy=0.5):
    """A pointy-top hexagon's corners, clockwise from the top."""
    return [(cx + radius * math.sin(math.radians(60 * i)), cy - radius * math.cos(math.radians(60 * i)))
            for i in range(6)]


def facets(inner):
    outer, hole = hexagon(OUTER), hexagon(inner)
    return [([outer[i], outer[(i + 1) % 6], hole[(i + 1) % 6], hole[i]], FACETS[i]) for i in range(6)]


def draw(size):
    scale = 8
    big = size * scale
    im = Image.new('RGBA', (big, big), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    for quad, color in facets(SMALL_INNER if size <= 24 else INNER):
        d.polygon([(x * big, y * big) for x, y in quad], fill=color)
    return im.resize((size, size), Image.LANCZOS)


def svg():
    polys = ''.join(
        '<polygon fill="%s" points="%s"/>' % (color, ' '.join('%.2f,%.2f' % (x * 64, y * 64) for x, y in quad))
        for quad, color in facets(INNER))
    return '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">%s</svg>\n' % polys


def main():
    os.makedirs(OUT, exist_ok=True)
    images = {s: draw(s) for s in SIZES}
    images[256].save(os.path.join(OUT, 'snowlark.ico'), format='ICO', sizes=[(s, s) for s in SIZES],
                     append_images=[images[s] for s in SIZES if s != 256])
    with open(os.path.join(OUT, 'snowlark.svg'), 'w', encoding='utf-8', newline='\n') as f:
        f.write(svg())
    if len(sys.argv) > 1:
        # Each size at its real size and enlarged, on light and dark.
        sheet = Image.new('RGB', (900, 560), '#ffffff')
        d = ImageDraw.Draw(sheet)
        d.rectangle([0, 280, 900, 560], fill='#202124')
        for top in (0, 280):
            x = 16
            for s in SIZES[:-1]:
                sheet.paste(images[s], (x, top + 16), images[s])
                x += s + 12
            sheet.paste(images[256], (620, top + 12), images[256])
            x = 16
            for s in (16, 24, 32):
                zoom = images[s].resize((s * 4, s * 4), Image.NEAREST)
                sheet.paste(zoom, (x, top + 150), zoom)
                x += s * 4 + 16
        sheet.save(sys.argv[1])


if __name__ == '__main__':
    main()
