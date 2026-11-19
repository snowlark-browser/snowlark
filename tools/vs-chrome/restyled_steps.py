# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Restyled elements per Speedometer step, in the scored windows only, ours against the reference browser's.

    SNOWLARK_PROFILE=1 SNOWLARK_SPEEDOMETER_MARKS=1 snowlark-cli --speedometer --iterations N [--suites ...] 2> err.txt
    python tools/vs-chrome/restyled_steps.py err.txt --iterations N [--trace <trace.json> --trace-iterations M]

Ours: `SNOWLARK_PROFILE`'s `[render]` lines are printed during an event loop turn, and
`SNOWLARK_SPEEDOMETER_MARKS` prints a `[turn]` line after each turn with the runner's performance marks made in it.
A turn's restyles are scored when the turn began inside a step (after `<Suite>.<Step>-start`, before its
`-async-end`) or made a step's `-start` mark: a step's sync callback and the rendering update after it run in one
turn. `travd` is what our restyles visited (restyled or not). The rest (each page load, the runner's own frames between steps) is listed apart with `--unscored`, by the
step before it.

The reference's: `UpdateLayoutTree`'s `elementCount` inside the same windows of a trace from
`tools/vs-chrome/speedometer.py --trace` (`split_style.py` counts the same per suite).
"""
import argparse, collections, os, re, sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from split_chrome import STEP, load_trace  # noqa: E402

RESTYLED = re.compile(r": restyled (\d+)")
TRAVERSED = re.compile(r"; traversed (\d+)")


def step_of(name):
    m = STEP.fullmatch(name)
    if not m or m.group(1).startswith("suite-"):
        return None
    return (m.group(1), m.group(2)), m.group(3)


def ours(path):
    """({(suite, step): restyled}, {(suite, step): traversed}, {step before: restyled}) over the run."""
    scored, traversed, unscored = collections.Counter(), collections.Counter(), collections.Counter()
    inside = last = None
    pending = []
    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            if line.startswith("[render] #"):
                m, t = RESTYLED.search(line), TRAVERSED.search(line)
                if m:
                    pending.append((int(m.group(1)), int(t.group(1)) if t else 0))
            elif line.startswith("[turn]"):
                began, started = inside, None
                for name in line[6:].split():
                    s = step_of(name)
                    if not s:
                        continue
                    if s[1] == "start":
                        started = inside = s[0]
                    elif s[1] == "async-end":
                        inside, last = None, s[0]
                key = began or started
                for n, t in pending:
                    if key:
                        scored[key] += n
                        traversed[key] += t
                    else:
                        unscored[last] += n
                pending = []
    return scored, traversed, unscored


def reference(path):
    """{(suite, step): elements restyled} over the trace's scored windows."""
    ev = load_trace(path)
    marks = [e for e in ev if "user_timing" in e.get("cat", "") and e.get("ph") in ("R", "I")
             and STEP.fullmatch(e["name"])]
    main = collections.Counter((e["pid"], e["tid"]) for e in marks).most_common(1)[0][0]
    windows, open_ = [], {}
    for e in sorted(marks, key=lambda e: e["ts"]):
        if (e["pid"], e["tid"]) != main:
            continue
        s = step_of(e["name"])
        if not s:
            continue
        if s[1] in ("start", "async-start"):
            open_[s[0]] = e["ts"]
        elif s[0] in open_:
            windows.append((open_.pop(s[0]), e["ts"], s[0]))
    out = collections.Counter()
    for e in ev:
        if (e.get("pid"), e.get("tid")) != main or e.get("name") != "UpdateLayoutTree":
            continue
        k = next((w[2] for w in windows if w[0] <= e.get("ts", 0) < w[1]), None)
        if k:
            out[k] += e.get("args", {}).get("elementCount", 0)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("stderr", help="our stderr with [render] and [turn] lines")
    ap.add_argument("--iterations", type=int, required=True)
    ap.add_argument("--trace", help="the reference's trace")
    ap.add_argument("--trace-iterations", type=int, default=1)
    ap.add_argument("--unscored", action="store_true", help="also list the restyles outside the scored windows")
    a = ap.parse_args()
    scored, traversed, unscored = ours(a.stderr)
    ref = reference(a.trace) if a.trace else collections.Counter()
    print(f'{"suite.step":60} {"ref":>7} {"ours":>7} {"x":>5} {"travd":>7}')
    totals = collections.defaultdict(lambda: [0.0, 0.0])
    for k in scored:
        o, r = scored[k] / a.iterations, ref[k] / a.trace_iterations
        totals[k[0]][0] += r
        totals[k[0]][1] += o
        t = traversed[k] / a.iterations
        print(f"{k[0] + '.' + k[1]:60} {r:7.0f} {o:7.0f} {o / max(r, 1):5.2f} {t:7.0f}")
    print("\nper suite and iteration:")
    for s, (r, o) in totals.items():
        print(f"{s:60} {r:7.0f} {o:7.0f} {o / max(r, 1):5.2f}")
    if a.unscored:
        print("\nunscored, per iteration, by the step before them (a step of the previous suite: the load):")
        for k, n in unscored.items():
            print(f"  after {k[0] + '.' + k[1] if k else '(start)'}: {n / a.iterations:.0f}")


if __name__ == "__main__":
    main()
