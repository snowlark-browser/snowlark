# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Which runs' logs can be trusted. usage: check_runs.py <runs dir> [name prefix ...]

E: filled up during the work, and the first builds of the logger ignored write errors, so a log can
be missing or cut short with nothing said. A run is good when: it finished; its frame log exists,
every line of it is whole, and it has at least as many lines as the summary counted frames; and,
with a capture, the capture log's lines are whole too. Prints the bad ones.
"""
import glob
import os
import re
import sys


def whole(path, fields):
    """(lines, every line whole and the file ends in a newline)."""
    with open(path, "rb") as f:
        data = f.read()
    if not data:
        return 0, False
    lines = data.split(b"\n")
    ok = lines[-1] == b""
    lines = lines[:-1]
    bad = [l for l in lines if l.count(b"\t") != fields - 1]
    return len(lines), ok and not bad


def check(runs, base):
    problems = []
    stdout = open(base + ".stdout", errors="replace").read()
    if "TIMEOUT" in stdout or "failed" in stdout or "did not exit" in stdout:
        problems.append("stopped oddly")
    if "log write failed" in stdout:
        problems.append("reported a failed log write")
    if "[a] mode" not in stdout:
        problems.append("no end of run line")
    frames = glob.glob(base + ".frames.tsv") + glob.glob(base + "-g*.frames.tsv")
    summaries = glob.glob(base + ".summary.txt") + glob.glob(base + "-g*.summary.txt")
    if not frames:
        problems.append("no frame log")
    if not summaries:
        problems.append("no summary")
    counted = 0
    for s in summaries:
        m = re.search(r"\] (\d+) frames", open(s, errors="replace").read())
        counted += int(m.group(1)) if m else 0
    lines = 0
    for f in frames:
        n, ok = whole(f, 7)
        lines += n - 1
        if not ok:
            problems.append(f"cut short: {os.path.basename(f)}")
    if frames and lines < counted:
        problems.append(f"frame log has {lines} lines for {counted} counted frames")
    if summaries and counted == 0 and "resize" not in os.path.basename(base) and not os.path.basename(base).startswith(("rs", "rz", "hang")):
        problems.append("no frames counted: the compositor was blocked for the whole run")
    if "--watch" in open(base + ".args", errors="replace").read() if os.path.exists(base + ".args") else False:
        pass
    w = base + ".watch.tsv"
    if os.path.exists(w):
        n, ok = whole(w, 10)
        if not ok:
            problems.append("capture log cut short")
    return problems


def main():
    runs = sys.argv[1]
    prefixes = sys.argv[2:]
    good = bad = 0
    for path in sorted(glob.glob(os.path.join(runs, "*.stdout"))):
        name = os.path.basename(path)[: -len(".stdout")]
        if prefixes and not any(name.startswith(p) for p in prefixes):
            continue
        p = check(runs, path[: -len(".stdout")])
        if p:
            bad += 1
            print(f"BAD {name}: {'; '.join(p)}")
        else:
            good += 1
    print(f"{good} good, {bad} bad")


main()
