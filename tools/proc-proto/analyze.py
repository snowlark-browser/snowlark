# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Reads the logs of a proc-a run. usage: analyze.py <base path> [more base paths]

base.watch.tsv (from the watcher): arrival QPC, composition time (100 ns), frame size, chrome code
(frame, w, h), content code (frame, w, h). base[.g0].frames.tsv (from the compositor): one line per
frame presented.
"""
import glob
import os
import sys

FREQ = 10_000_000  # QPC ticks per second on the measured machine


def pct(v, p):
    if not v:
        return 0.0
    v = sorted(v)
    return v[round((len(v) - 1) * p)]


def frames_of(base):
    rows = []
    for path in glob.glob(base + ".frames.tsv") + glob.glob(base + "-g*.frames.tsv"):
        with open(path) as f:
            next(f)
            for line in f:
                c = line.rstrip("\n").split("\t")
                rows.append(
                    dict(
                        frame=int(c[0]),
                        done=int(c[1]),
                        wait=float(c[2]),
                        compose=float(c[3]),
                        input=int(c[4]),
                        moving=int(c[5]),
                        vblank=int(c[6]),
                    )
                )
    return rows


def watch_of(base):
    path = base + ".watch.tsv"
    if not os.path.exists(path):
        return []
    rows = []
    with open(path) as f:
        for line in f:
            c = line.rstrip("\n").split("\t")
            rows.append([int(x) for x in c])
    return rows


def report(base):
    frames = frames_of(base)
    name = os.path.basename(base)
    print(f"== {name}")
    lat = [(r["done"] - r["input"]) / FREQ * 1000 for r in frames if r["input"] and r["moving"]]
    if lat:
        print(f"input->present n {len(lat)} p50 {pct(lat, .5):.2f} p95 {pct(lat, .95):.2f} max {max(lat):.2f} ms")
    inter = []
    for a, b in zip(frames, frames[1:]):
        if a["moving"] and b["moving"]:
            d = (b["done"] - a["done"]) / FREQ * 1000
            if d < 100:
                inter.append(d)
    if inter:
        print(f"present interval n {len(inter)} p50 {pct(inter, .5):.2f} p95 {pct(inter, .95):.2f} p99 {pct(inter, .99):.2f} max {max(inter):.2f} ms")
    rows = watch_of(base)
    if not rows:
        return
    at = [r[1] for r in rows]
    gaps = [(b - a) / 10_000 for a, b in zip(at, at[1:])]
    print(f"captured frames {len(rows)}; composition interval p50 {pct(gaps, .5):.2f} p95 {pct(gaps, .95):.2f} p99 {pct(gaps, .99):.2f} max {max(gaps):.2f} ms")
    unread = sum(1 for r in rows if r[4] < 0 or r[7] < 0)
    layer_mismatch = [r for r in rows if r[4] >= 0 and r[7] >= 0 and (r[5], r[6]) != (r[8], r[9])]
    window_mismatch = [r for r in rows if r[7] >= 0 and (r[2], r[3]) != (r[8], r[9])]
    print(f"unreadable {unread}; chrome/content size differs in {len(layer_mismatch)} frames; content/window size differs in {len(window_mismatch)} frames")
    for r in layer_mismatch[:5]:
        print("   mismatch", r[2:])
    sizes = len({(r[2], r[3]) for r in rows})
    print(f"distinct window sizes seen {sizes}")
    by_frame = {r["frame"]: r for r in frames}
    delays = []
    seen = set()
    for r in rows:
        n = r[7]
        if n in by_frame and n not in seen:
            seen.add(n)
            delays.append((r[1] * (FREQ // 10_000_000) - by_frame[n]["done"]) / FREQ * 1000)
    if delays:
        print(f"present->composition n {len(delays)} of {len(by_frame)} presented: p50 {pct(delays, .5):.2f} p95 {pct(delays, .95):.2f} max {max(delays):.2f} ms")
    # Input to the composition that first showed it.
    shown = []
    ordered = sorted(seen)
    first_at = {}
    for r in rows:
        first_at.setdefault(r[7], r[1])
    for f in frames:
        if f["input"] and f["moving"] and f["frame"] in first_at:
            shown.append((first_at[f["frame"]] - f["input"]) / FREQ * 1000)
    if shown:
        print(f"input->composition n {len(shown)}: p50 {pct(shown, .5):.2f} p95 {pct(shown, .95):.2f} max {max(shown):.2f} ms")


for b in sys.argv[1:]:
    report(b)
