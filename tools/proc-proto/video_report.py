# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""A video run's logs. usage: video_report.py <base path> [more]

base.video.tsv: every frame A read back from the texture (serial, frame number, blocks that disagree,
readable, written, presented). base.watch.tsv: what DWM composed for the window (arrival, composition
time, size, readable, frame number, blocks that disagree), at half size.
"""
import os
import sys

FREQ = 10_000_000


def pct(v, p):
    if not v:
        return 0.0
    v = sorted(v)
    return v[round((len(v) - 1) * p)]


def report(base):
    print(f"== {os.path.basename(base)}")
    rows = []
    with open(base + ".video.tsv") as f:
        next(f)
        for line in f:
            c = line.rstrip("\n").split("\t")
            if len(c) == 6:
                rows.append([int(x) for x in c])
    if not rows:
        print("no frames")
        return
    torn = [r for r in rows if r[3] and r[2] > 0]
    unread = sum(1 for r in rows if not r[3])
    numbers = [r[1] for r in rows if r[3]]
    lost = sum(b - a - 1 for a, b in zip(numbers, numbers[1:]) if b > a + 1)
    back = sum(1 for a, b in zip(numbers, numbers[1:]) if b < a)
    repeats = sum(1 for a, b in zip(numbers, numbers[1:]) if b == a)
    span = (rows[-1][5] - rows[0][5]) / FREQ
    print(
        f"texture: {len(rows)} frames over {span:.1f} s ({len(rows) / span:.2f} per s), first {numbers[0]} last {numbers[-1]}; "
        f"torn {len(torn)}, unreadable {unread}, lost {lost}, repeats {repeats}, backwards {back}"
    )
    for r in torn[:5]:
        print("   torn", r[:4])
    gaps = [(b[5] - a[5]) / FREQ * 1000 for a, b in zip(rows, rows[1:])]
    print(
        f"presented interval p50 {pct(gaps, .5):.2f} p95 {pct(gaps, .95):.2f} p99 {pct(gaps, .99):.2f} max {max(gaps):.2f} ms; "
        f"over 25 ms: {sum(1 for g in gaps if g > 25)}"
    )
    lat = [(r[5] - r[4]) / FREQ * 1000 for r in rows]
    print(f"written->presented p50 {pct(lat, .5):.2f} p95 {pct(lat, .95):.2f} max {max(lat):.2f} ms")
    path = base + ".watch.tsv"
    if os.path.exists(path):
        w = []
        with open(path) as f:
            for line in f:
                c = line.rstrip("\n").split("\t")
                if len(c) == 7:
                    w.append([int(x) for x in c])
        seen = {r[5] for r in w if r[4]}
        wrong = [r for r in w if r[4] and r[6] > 0]
        print(
            f"screen: {len(w)} compositions, {len(seen)} distinct frames, unreadable {sum(1 for r in w if not r[4])}, "
            f"with disagreeing blocks {len(wrong)}"
        )
        for r in wrong[:5]:
            print("   screen", r[2:])


for b in sys.argv[1:]:
    report(b)
