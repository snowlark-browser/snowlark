# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Resize rounds. usage: resize_report.py <runs dir> <prefix> <mode,mode,...>

Per mode, over its rounds: compositions captured, how many showed chrome and content at different
sizes, how many showed the content at a size other than the window's, and the WM_SIZE acks.
"""
import glob
import os
import re
import sys

runs, prefix, modes = sys.argv[1], sys.argv[2], sys.argv[3].split(",")
for m in modes:
    frames = layer = window = unread = resizes = timeouts = 0
    per_round = []
    acks = []
    stall = 0.0
    for path in sorted(glob.glob(os.path.join(runs, f"{prefix}-{m}-*.watch.tsv"))):
        base = path[: -len(".watch.tsv")]
        n = bad = 0
        with open(path) as f:
            for line in f:
                c = [int(x) for x in line.rstrip("\n").split("\t")]
                n += 1
                if c[4] < 0 or c[7] < 0:
                    unread += 1
                    continue
                if (c[5], c[6]) != (c[8], c[9]):
                    bad += 1
                if (c[2], c[3]) != (c[8], c[9]):
                    window += 1
        frames += n
        layer += bad
        per_round.append(bad)
        a = open(base + ".a.txt", errors="replace").read()
        g = re.search(r"worst stall ([\d.]+) ms, resize acks timed out (\d+), resizes (\d+)", a)
        if g:
            stall = max(stall, float(g.group(1)))
            timeouts += int(g.group(2))
            resizes += int(g.group(3))
        for s in glob.glob(base + ".summary.txt") + glob.glob(base + "-g0.summary.txt"):
            g = re.search(r"resize acks (\d+) p50 ([\d.]+) p95 ([\d.]+) max ([\d.]+)", open(s).read())
            if g:
                acks.append((float(g.group(2)), float(g.group(3)), float(g.group(4))))
    if not per_round:
        continue
    print(
        f"{m}: rounds {len(per_round)}, compositions {frames}, chrome/content differ {layer} {per_round} "
        f"({100 * layer / frames:.2f}%), content/window differ {window} ({100 * window / frames:.1f}%), unreadable {unread}; "
        f"resizes {resizes}, ack timeouts {timeouts}, ack p50 {sorted(a[0] for a in acks)[len(acks) // 2]:.2f} "
        f"p95 {sorted(a[1] for a in acks)[len(acks) // 2]:.2f} max {max(a[2] for a in acks):.2f} ms; window thread worst stall {stall:.1f} ms"
    )
