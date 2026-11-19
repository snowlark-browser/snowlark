# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Summarize the `[render]` lines of a `SNOWLARK_PROFILE=1` run (tools/speedometer/run.ps1 -Profile).

    python tools/speedometer/render_summary.py <stderr.txt> [--top 12]

Per document (each renderer's label), over all its rendering updates: how many updates, elements
restyled, boxes built, boxes laid out against reused, and the time in style, box building and layout,
with the per-update means. Then the costliest single updates, and the forced style/layout updates by
the API that forced them. A step that costs in proportion to the page shows as many elements restyled
or boxes laid out per update where few changed.
"""

import argparse
import re
import sys
from collections import defaultdict

LINE = re.compile(
    r"\[render\] #(?P<n>\d+) (?P<at>[\d.]+)ms (?P<label>.*?): restyled (?P<restyled>\d+) "
    r"\(unchanged (?P<unchanged>\d+) repaint (?P<repaint>\d+) relayout (?P<relayout>\d+) "
    r"reconstruct (?P<reconstruct>\d+)\), builds full (?P<full>\d+) incremental (?P<incr>\d+), "
    r"boxes built (?P<built>\d+) rebuilt containers (?P<containers>\d+) restyled in place (?P<inplace>\d+), "
    r"laid out (?P<laid_out>\d+) reused (?P<reused>\d+)(?: again (?P<again>\d+))?"
    r"(?: \(flex/grid built (?P<flex_built>\d+) runs (?P<flex_runs>\d+)\))?, "
    r"shaped (?P<shaped>\d+) cached (?P<cached>\d+)"
    r"(?: \(words shaped (?P<words_shaped>\d+) cached (?P<words_cached>\d+)\))?; "
    r"style (?P<style_ms>[\d.]+)ms x(?P<restyles>\d+) build (?P<build_ms>[\d.]+)ms "
    r"layout (?P<layout_ms>[\d.]+)ms x(?P<layouts>\d+)(?: sheets (?P<sheets>\d+))?"
    r"(?:; traversed (?P<traversed>\d+) matched (?P<matched>\d+) shared (?P<shared>\d+))?"
)
FORCED = re.compile(r"\[render\] #\d+ forced: (.*)")

COUNTS = ["restyled", "reconstruct", "full", "built", "containers", "laid_out", "reused", "shaped", "sheets",
          "traversed", "matched", "shared"]
TIMES = ["style_ms", "build_ms", "layout_ms"]


def short(label):
    # (the last path segments name the suite)
    parts = label.rstrip("/").split("/")
    return "/".join(parts[-4:])[:70]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("stderr")
    ap.add_argument("--top", type=int, default=12)
    args = ap.parse_args()
    per = defaultdict(lambda: defaultdict(float))
    updates = []
    forced = []
    with open(args.stderr, encoding="utf-8", errors="replace") as f:
        for line in f:
            m = LINE.search(line)
            if m:
                d = m.groupdict()
                for k in ("sheets", "traversed", "matched", "shared"):
                    d[k] = d.get(k) or "0"
                p = per[d["label"]]
                p["updates"] += 1
                for k in COUNTS:
                    p[k] += int(d[k])
                for k in TIMES:
                    p[k] += float(d[k])
                total = sum(float(d[k]) for k in TIMES)
                updates.append((total, d))
                continue
            m = FORCED.search(line)
            if m:
                forced.append(m.group(1))
    if not per:
        print("no [render] lines (was SNOWLARK_PROFILE=1 set?)")
        return
    print("rendering updates by document (totals; per update in brackets):")
    print(
        f"  {'document':<70} {'upd':>5} {'full':>5} {'restyled':>16} {'built':>14} {'laid out':>16} {'reused':>10}"
        f" {'style ms':>16} {'build ms':>14} {'layout ms':>16}"
    )
    rows = sorted(per.items(), key=lambda kv: -sum(kv[1][k] for k in TIMES))
    for label, p in rows:
        n = max(p["updates"], 1)

        def c(k, w):
            return f"{p[k]:>{w - 9}.0f} [{p[k] / n:>6.0f}]"

        def t(k, w):
            return f"{p[k]:>{w - 9}.1f} [{p[k] / n:>6.2f}]"

        print(
            f"  {short(label):<70} {p['updates']:>5.0f} {p['full']:>5.0f} {c('restyled', 16)} {c('built', 14)}"
            f" {c('laid_out', 16)} {p['reused']:>10.0f} {t('style_ms', 16)} {t('build_ms', 14)}"
            f" {t('layout_ms', 16)}"
        )
    print(f"\ncostliest single updates (style + build + layout ms):")
    for total, d in sorted(updates, key=lambda u: -u[0])[: args.top]:
        print(
            f"  {total:7.2f} ms  #{d['n']} {short(d['label'])}: full builds {d['full']}, sheets changed {d['sheets']}, restyled {d['restyled']}"
            f" (reconstruct {d['reconstruct']}), built {d['built']}, laid out {d['laid_out']}"
            f" reused {d['reused']}; style {d['style_ms']} build {d['build_ms']} layout {d['layout_ms']}"
        )
    if forced:
        tally = defaultdict(int)
        for f in forced:
            for part in f.split(", "):
                api = part.split(" ")[0]
                tally[api] += 1
        print(f"\nupdates with forced style/layout: {len(forced)}; by API (updates naming it):")
        for api, n in sorted(tally.items(), key=lambda kv: -kv[1])[: args.top]:
            print(f"  {n:6} {api}")


if __name__ == "__main__":
    main()
