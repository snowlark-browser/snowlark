# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Where a video run stalled. usage: gaps.py <base path> [threshold ms]

For each gap in what A presented: when it was (wall clock from the QPC offset given by the log's file
time), how long, whether B's writes went on (written times) or stopped too, and what the capture of
the window saw meanwhile.
"""
import datetime
import os
import sys

FREQ = 10_000_000
base = sys.argv[1]
limit = float(sys.argv[2]) if len(sys.argv) > 2 else 100.0
rows = []
with open(base + ".video.tsv") as f:
    next(f)
    for line in f:
        c = line.rstrip("\n").split("\t")
        if len(c) == 6:
            rows.append([int(x) for x in c])
end = datetime.datetime.fromtimestamp(os.path.getmtime(base + ".video.tsv"))
last = rows[-1][5]


def clock(q):
    return (end - datetime.timedelta(seconds=(last - q) / FREQ)).strftime("%H:%M:%S.%f")[:-3]


w = []
if os.path.exists(base + ".watch.tsv"):
    with open(base + ".watch.tsv") as f:
        for line in f:
            c = line.rstrip("\n").split("\t")
            if len(c) == 7:
                w.append([int(x) for x in c])
print(f"{len(rows)} frames, {len(w)} compositions, ends {end.strftime('%H:%M:%S')}")
n = 0
for a, b in zip(rows, rows[1:]):
    gap = (b[5] - a[5]) / FREQ * 1000
    if gap < limit:
        continue
    n += 1
    wrote = (b[4] - a[4]) / FREQ * 1000
    held = (b[5] - b[4]) / FREQ * 1000
    during = sum(1 for r in w if a[5] <= r[1] <= b[5])
    print(
        f"{clock(a[5])} presented gap {gap:.0f} ms; written gap {wrote:.0f} ms; next frame waited {held:.0f} ms after written; "
        f"frame {a[1]} -> {b[1]} (skipped {b[1] - a[1] - 1}); compositions during {during}"
    )
print(f"{n} gaps over {limit} ms")
