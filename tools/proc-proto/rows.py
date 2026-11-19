# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Prints the rows of a video log around a frame number. usage: rows.py <base> <frame> [frame ...]"""
import sys

FREQ = 10_000_000
rows = []
with open(sys.argv[1] + ".video.tsv") as f:
    next(f)
    for line in f:
        c = line.rstrip("\n").split("\t")
        if len(c) == 6:
            rows.append([int(x) for x in c])
t0 = rows[0][5]
print(f"rows {len(rows)}; serial,frame,wrong,found, written s, presented s (from the first frame)")
for want in sys.argv[2:]:
    want = int(want)
    i = next(i for i, r in enumerate(rows) if r[1] >= want)
    print(f"-- around frame {want} (row {i})")
    for r in rows[max(0, i - 3) : i + 5]:
        print(r[0], r[1], r[2], r[3], f"{(r[4] - t0) / FREQ:.3f}", f"{(r[5] - t0) / FREQ:.3f}")
order = sum(1 for a, b in zip(rows, rows[1:]) if b[5] < a[5])
print(f"presented out of order: {order}")
