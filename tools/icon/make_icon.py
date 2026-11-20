# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Draws Snowlark's icon: crates/snowlark/res/snowlark.ico (the executable's and the window's), snowlark.svg (the
About page's), and on request a preview sheet and the website's favicon.svg, favicon.ico and icon-512.png. Run it
after changing the design:

    python tools/icon/make_icon.py [--preview <sheet.png>] [--web <dir>]

The mark is a snow lark in flight, rising to the right with its wings spread back: a small songbird of snowy open
country, with a short crest, a notched tail and a yellow beak. It is cut into flat triangular facets in seven
blues lit from the top left, so it reads as one crystal-like shape. The SVG and every raster size are drawn from
the same triangles. At 24 px and below the palest blue is left out (those facets take the next tone), so the
raised wing still shows against a white taskbar. Needs Pillow.
"""
import argparse
import math
import os

from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.normpath(os.path.join(HERE, '..', '..', 'crates', 'snowlark', 'res'))
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
SMALL = 24          # sizes up to this one leave out the palest tone
BOX = 256.0         # the design's coordinate box

# Tones from lit (0) to shade (6), and the beak.
TONES = ['#9FC3EE', '#6FA3E3', '#4A82CF', '#3366B4', '#2A579E', '#1F447F', '#173463']
BEAK = '#F2C04A'

# The lark facing right with its centre near the origin, before the pose turns it to rise.
POINTS = dict(
    beak=(86, -2), b1=(58, -12), b2=(58, 6), crown=(34, -24), crest=(12, -48), nape=(14, -20), chest=(40, 16),
    back=(-24, -12), belly=(-20, 24), tb1=(-62, -4), tb2=(-58, 20), t1=(-94, -12), notch=(-84, 4), t2=(-96, 18),
    # The near wing (raised and swept back, a notch in its trailing edge) and the far wing behind it.
    wr1=(24, -14), wr2=(-12, -10), wel=(-8, -60), wtip=(-56, -110), wn=(-42, -78), wtr=(-40, -42),
    fr=(42, -16), fm=(24, -62), ftip=(-2, -106), fb=(4, -30))
# Back to front; each triangle names its tone.
FACETS = [
    (('fr', 'fm', 'fb'), 2), (('fm', 'ftip', 'fb'), 3),
    (('crown', 'crest', 'nape'), 1), (('b1', 'crown', 'nape'), 0),
    (('b1', 'nape', 'chest'), 2), (('b1', 'chest', 'b2'), 3),
    (('nape', 'back', 'chest'), 3), (('back', 'belly', 'chest'), 5),
    (('back', 'tb1', 'belly'), 4), (('tb1', 'tb2', 'belly'), 5),
    (('tb1', 't1', 'notch'), 3), (('tb1', 'notch', 'tb2'), 4), (('notch', 't2', 'tb2'), 6),
    (('wr1', 'wel', 'wr2'), 1), (('wel', 'wtip', 'wn'), 0), (('wel', 'wn', 'wtr'), 1), (('wel', 'wtr', 'wr2'), 2),
    (('b1', 'beak', 'b2'), 'beak'),
]
ANGLE, SCALE, CENTRE = -30.0, 1.24, (118.0, 170.0)


def triangles(small):
    """(corners in the 256 box, colour) for each facet, back to front."""
    a = math.radians(ANGLE)
    ca, sa = math.cos(a) * SCALE, math.sin(a) * SCALE

    def place(p):
        x, y = p
        return (x * ca - y * sa + CENTRE[0], x * sa + y * ca + CENTRE[1])

    out = []
    for names, tone in FACETS:
        colour = BEAK if tone == 'beak' else TONES[max(tone, 1) if small else tone]
        out.append(([place(POINTS[n]) for n in names], colour))
    return out


def draw(size):
    scale = max(4, min(16, 2048 // size))
    big = size * scale
    im = Image.new('RGBA', (big, big), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    k = big / BOX
    for corners, colour in triangles(size <= SMALL):
        d.polygon([(x * k, y * k) for x, y in corners], fill=colour)
    # Premultiplied, so the transparent surround does not darken the edges.
    return im.convert('RGBa').resize((size, size), Image.LANCZOS).convert('RGBA')


def svg():
    k = 64 / BOX
    polys = ''.join('<polygon fill="%s" points="%s"/>' % (colour, ' '.join('%.2f,%.2f' % (x * k, y * k)
                                                                         for x, y in corners))
                    for corners, colour in triangles(False))
    return '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">%s</svg>\n' % polys


def preview(images, path):
    """Each size at its real size and the small ones enlarged, on light and dark."""
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
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    sheet.save(path)


def web(images, folder):
    """The website's favicon.svg, favicon.ico (16, 32, 48) and icon-512.png."""
    os.makedirs(folder, exist_ok=True)
    with open(os.path.join(folder, 'favicon.svg'), 'w', encoding='utf-8', newline='\n') as f:
        f.write(svg())
    images[48].save(os.path.join(folder, 'favicon.ico'), format='ICO', sizes=[(16, 16), (32, 32), (48, 48)],
                    append_images=[images[16], images[32]])
    draw(512).save(os.path.join(folder, 'icon-512.png'))


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    parser.add_argument('--preview', metavar='PNG', help='also write a preview sheet')
    parser.add_argument('--web', metavar='DIR', help="also write the website's favicons and 512 px icon")
    args = parser.parse_args()
    os.makedirs(OUT, exist_ok=True)
    images = {s: draw(s) for s in SIZES}
    images[256].save(os.path.join(OUT, 'snowlark.ico'), format='ICO', sizes=[(s, s) for s in SIZES],
                     append_images=[images[s] for s in SIZES if s != 256])
    with open(os.path.join(OUT, 'snowlark.svg'), 'w', encoding='utf-8', newline='\n') as f:
        f.write(svg())
    if args.preview:
        preview(images, args.preview)
    if args.web:
        web(images, args.web)


if __name__ == '__main__':
    main()
