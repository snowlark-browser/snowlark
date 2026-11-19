# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Rounds of a bench.ps1 run. usage: bench_report.py <runs dir> <prefix> <mode,mode,...>

Per mode and round: input->present p50 and p95 and present interval, counted as composite.rs counts
them; then the pooled figures, and the paired p95 difference from the first mode, round by round.
"""
import glob
import os
import sys

FREQ = 10_000_000


def pct(v, p):
    if not v:
        return 0.0
    v = sorted(v)
    return v[round((len(v) - 1) * p)]


def load(base):
    rows = []
    for path in glob.glob(base + ".frames.tsv") + glob.glob(base + "-g*.frames.tsv"):
        with open(path) as f:
            next(f)
            for line in f:
                c = line.rstrip("\n").split("\t")
                rows.append((int(c[1]), int(c[4]), int(c[5]), float(c[2]), float(c[3])))
    lat = [(d - i) / FREQ * 1000 for d, i, m, _, _ in rows if i and m]
    inter = []
    for a, b in zip(rows, rows[1:]):
        if a[2] and b[2]:
            x = (b[0] - a[0]) / FREQ * 1000
            if x < 100:
                inter.append(x)
    return lat, inter, [r[4] for r in rows if r[2]]


def main():
    runs, prefix, modes = sys.argv[1], sys.argv[2], sys.argv[3].split(",")
    # A round counts only if every mode has data in it: the paired differences need that, and an
    # empty log must never read as a latency of zero.
    numbers = set()
    for m in modes:
        for path in glob.glob(os.path.join(runs, f"{prefix}-{m}-*.stdout")):
            numbers.add(int(path[: -len(".stdout")].rsplit("-", 1)[1]))
    kept, dropped = [], []
    loaded = {}
    for n in sorted(numbers):
        here = {}
        for m in modes:
            base = os.path.join(runs, f"{prefix}-{m}-{n}")
            here[m] = load(base) if os.path.exists(base + ".stdout") else ([], [], [])
        if all(here[m][0] and here[m][1] for m in modes):
            kept.append(n)
            loaded[n] = here
        else:
            dropped.append(n)
    print(f"rounds kept {kept}; dropped for missing data {dropped}")
    data = {m: [loaded[n][m] for n in kept] for m in modes}
    for m in modes:
        print(f"-- {m}")
        lats, ints, comps = [], [], []
        for i, (lat, inter, comp) in enumerate(data[m]):
            print(
                f"  round {i + 1}: inputs {len(lat)} p50 {pct(lat, .5):.2f} p95 {pct(lat, .95):.2f} max {max(lat or [0]):.2f}; "
                f"frames {len(inter)} interval p50 {pct(inter, .5):.2f} p95 {pct(inter, .95):.2f} max {max(inter or [0]):.2f}"
            )
            lats += lat
            ints += inter
            comps += comp
        print(
            f"  pooled: inputs {len(lats)} p50 {pct(lats, .5):.2f} p95 {pct(lats, .95):.2f} p99 {pct(lats, .99):.2f} max {max(lats or [0]):.2f}; "
            f"interval p50 {pct(ints, .5):.2f} p95 {pct(ints, .95):.2f} p99 {pct(ints, .99):.2f}; compose p50 {pct(comps, .5):.3f}"
        )
    base = modes[0]
    for m in modes[1:]:
        d = [pct(b[0], 0.95) - pct(a[0], 0.95) for a, b in zip(data[base], data[m])]
        d50 = [pct(b[0], 0.5) - pct(a[0], 0.5) for a, b in zip(data[base], data[m])]
        print(
            f"{m} minus {base}: p95 by round {[round(x, 2) for x in d]} median {pct(d, .5):.2f}; "
            f"p50 by round {[round(x, 2) for x in d50]} median {pct(d50, .5):.2f}"
        )


main()
