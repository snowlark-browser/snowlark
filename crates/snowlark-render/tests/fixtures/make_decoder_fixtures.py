# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Image decoder fixtures: a Display P3 ICC profile built from its definition, embedded in PNG, JPEG
and WebP; a PNG with gAMA 1.0; a progressive JPEG with an EXIF orientation.
Usage: make_decoder_fixtures.py <out dir>"""
import struct
import sys

import numpy as np
from PIL import Image, PngImagePlugin

out = sys.argv[1]


def s15(v):
    return struct.pack(">i", int(round(v * 65536)))


def xyz_tag(x, y, z):
    return b"XYZ " + b"\0" * 4 + s15(x) + s15(y) + s15(z)


def para_srgb():
    # parametric curve type 3: sRGB's transfer
    g, a, b, c, d = 2.4, 1 / 1.055, 0.055 / 1.055, 1 / 12.92, 0.04045
    return b"para" + b"\0" * 4 + struct.pack(">HH", 3, 0) + b"".join(s15(v) for v in (g, a, b, c, d))


def desc_tag(text):
    t = text.encode("ascii") + b"\0"
    return b"desc" + b"\0" * 4 + struct.pack(">I", len(t)) + t + b"\0" * 4 + b"\0" * 4 + b"\0" * 2 + b"\0" + b"\0" * 67


def rgb_to_xyz(primaries, white):
    xyz = lambda x, y: np.array([x / y, 1.0, (1 - x - y) / y])
    m = np.column_stack([xyz(*p) for p in primaries])
    s = np.linalg.solve(m, xyz(*white))
    return m * s


def bradford(src_white, dst_white):
    b = np.array([[0.8951, 0.2664, -0.1614], [-0.7502, 1.7135, 0.0367], [0.0389, -0.0685, 1.0296]])
    xyz = lambda x, y: np.array([x / y, 1.0, (1 - x - y) / y])
    s = b @ xyz(*src_white)
    d = b @ dst_white
    return np.linalg.inv(b) @ np.diag(d / s) @ b


def icc_profile(primaries, white, name):
    d50 = np.array([0.9642, 1.0, 0.8249])
    m = bradford(white, d50) @ rgb_to_xyz(primaries, white)
    tags = [
        (b"desc", desc_tag(name)),
        (b"wtpt", xyz_tag(*d50)),
        (b"rXYZ", xyz_tag(*m[:, 0])),
        (b"gXYZ", xyz_tag(*m[:, 1])),
        (b"bXYZ", xyz_tag(*m[:, 2])),
        (b"rTRC", para_srgb()),
        (b"gTRC", para_srgb()),
        (b"bTRC", para_srgb()),
        (b"cprt", b"text" + b"\0" * 4 + b"No copyright, use freely\0"),
    ]
    table = struct.pack(">I", len(tags))
    data = b""
    offset = 128 + 4 + 12 * len(tags)
    for sig, body in tags:
        while len(body) % 4:
            body += b"\0"
        table += sig + struct.pack(">II", offset + len(data), len(body))
        data += body
    size = offset + len(data)
    header = struct.pack(">I", size) + b"\0" * 4 + struct.pack(">I", 0x02100000) + b"mntrRGB XYZ "
    header += struct.pack(">6H", 2026, 11, 24, 0, 0, 0) + b"acsp" + b"\0" * 4 + b"\0" * 4
    header += b"\0" * 8 + b"\0" * 8 + struct.pack(">I", 0) + s15(0.9642) + s15(1.0) + s15(0.8249)
    header += b"\0" * 4 + b"\0" * 16 + b"\0" * 28
    assert len(header) == 128, len(header)
    return header + table + data


P3 = icc_profile([(0.680, 0.320), (0.265, 0.690), (0.150, 0.060)], (0.3127, 0.3290), "Display P3 (test)")

# four 8 x 8 patches: P3 red, P3 green, a mid colour, gray
PATCHES = [(255, 0, 0), (0, 255, 0), (200, 100, 100), (128, 128, 128)]
img = Image.new("RGB", (32, 8))
for i, c in enumerate(PATCHES):
    img.paste(c, (i * 8, 0, i * 8 + 8, 8))
img.save(f"{out}/p3-patches.png", icc_profile=P3)
img.save(f"{out}/p3-patches.jpg", icc_profile=P3, quality=100, subsampling=0)
img.save(f"{out}/p3-patches.webp", icc_profile=P3, lossless=True)

# gAMA 1.0 (linear) without cHRM: 50% gray stored as 128
info = PngImagePlugin.PngInfo()
info.add(b"gAMA", struct.pack(">I", 100000))
Image.new("RGB", (8, 8), (128, 128, 128)).save(f"{out}/gamma-1.png", pnginfo=info)

# A progressive JPEG stored 16 x 8 (red left half, blue right half) with EXIF orientation 6
# (rotate 90 degrees clockwise to show): shown 8 x 16, red on top.
stored = Image.new("RGB", (16, 8), (0, 0, 255))
stored.paste((255, 0, 0), (0, 0, 8, 8))
exif = Image.Exif()
exif[0x0112] = 6
stored.save(f"{out}/orient-6.jpg", quality=95, subsampling=0, progressive=True, exif=exif)

# A CMYK JPEG (Pillow writes Adobe's inverted CMYK): two 8 x 8 patches, cyan (C 255) and
# 50% black (K 128), no colour profile.
cmyk = Image.new("CMYK", (16, 8), (255, 0, 0, 0))
cmyk.paste((0, 0, 0, 128), (8, 0, 16, 8))
cmyk.save(f"{out}/cmyk.jpg", quality=100)

# AVIF (4:4:4 at the highest quality, so the colours survive): the P3 patches with the ICC profile,
# and again with an nclx colour description of P3 primaries (12) instead, patched into the 'colr'
# box Pillow writes (BT.709 primaries, sRGB transfer, BT.601 matrix).
img.save(f"{out}/p3-patches.avif", icc_profile=P3, quality=100, subsampling="4:4:4")
img.save(f"{out}/p3-nclx.avif", quality=100, subsampling="4:4:4")
with open(f"{out}/p3-nclx.avif", "rb") as f:
    data = bytearray(f.read())
at = data.index(b"colrnclx") + 8
assert struct.unpack(">HHH", data[at : at + 6]) == (1, 13, 6), data[at : at + 6]
data[at : at + 2] = struct.pack(">H", 12)
with open(f"{out}/p3-nclx.avif", "wb") as f:
    f.write(data)

# Alpha: 16 x 8, the left half red at alpha 128, the right half lime, fully transparent in its
# right quarter.
rgba = Image.new("RGBA", (16, 8), (0, 255, 0, 255))
rgba.paste((255, 0, 0, 128), (0, 0, 8, 8))
rgba.paste((0, 255, 0, 0), (12, 0, 16, 8))
rgba.save(f"{out}/alpha.avif", quality=100, subsampling="4:4:4")

# An odd size with 4:2:0 chroma: 7 x 5, blue with a yellow last column and row.
odd = Image.new("RGB", (7, 5), (0, 0, 255))
odd.paste((255, 255, 0), (6, 0, 7, 5))
odd.paste((255, 255, 0), (0, 4, 7, 5))
odd.save(f"{out}/odd-7x5.avif", quality=95, subsampling="4:2:0")

# Stored 16 x 8 (red left half, blue right half) with EXIF orientation 6, which Pillow writes as
# 'irot' and 'imir': shown 8 x 16, red on top.
stored.save(f"{out}/orient-6.avif", quality=95, subsampling="4:4:4", exif=exif)

# An image sequence: red 100 ms, lime 200 ms, blue 300 ms, 16 x 16.
frames = [Image.new("RGB", (16, 16), c) for c in [(255, 0, 0), (0, 255, 0), (0, 0, 255)]]
frames[0].save(
    f"{out}/anim-rgb.avif",
    save_all=True,
    append_images=frames[1:],
    duration=[100, 200, 300],
    quality=95,
    subsampling="4:4:4",
)
print("ok", len(P3))
