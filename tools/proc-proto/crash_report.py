# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""What the screen showed around each kill. usage: crash_report.py <base path>

Frame numbers carry the generation (n // 100000), so the capture shows where each B's frames end
and the next one's begin, and what was composed in between.
"""
import sys

rows = []
with open(sys.argv[1] + ".watch.tsv") as f:
    for line in f:
        rows.append([int(x) for x in line.rstrip("\n").split("\t")])
print(f"captured {len(rows)}")
prev = None
gaps = []
for i, r in enumerate(rows):
    readable = r[7] >= 0
    gen = r[7] // 100000 if readable else None
    if prev is not None:
        pgen, pat, pi = prev
        if readable and gen != pgen:
            between = i - pi - 1
            gap = (r[1] - pat) / 10000
            gaps.append(gap)
            print(f"gen {pgen} -> {gen}: last frame to first frame {gap:.1f} ms, {between} unreadable compositions between")
    if readable:
        prev = (gen, r[1], i)
    else:
        if i and rows[i - 1][7] >= 0:
            print(f"  unreadable composition at {(r[1] - rows[0][1]) / 1e7:.2f} s: chrome code {r[4:7]}, content code {r[7:10]}")
if gaps:
    gaps.sort()
    print(f"gap p50 {gaps[len(gaps) // 2]:.1f} max {gaps[-1]:.1f} ms")
