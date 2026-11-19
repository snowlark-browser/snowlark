#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Interleaved before and after for scrolling in the real window.

    python tools/scroll-ab/run.py <snowlark-cli.exe> --rounds 10 --secs 10 --a NAME=VALUE --b NAME=VALUE

Each round runs arm A then arm B, or B then A on odd rounds, each a fresh window of one binary with
`SNOWLARK_GPU_STATS=1` and its own environment variable, so what the machine is doing falls on both
arms alike. Wheel messages are posted to the window (no input reaches the desktop), and the engine's
own `input→present` lines are the measure. Reports each round's pair and the paired difference.

The window opens at `SNOWLARK_TEST_WINDOW_POS` (a monitor you are not working on) with a profile of its own and `SNOWLARK_NO_ACTIVATE=1`, so it never
takes the foreground. The foreground window is still checked around every run, and a test that takes
it stops at once.
"""
import argparse
import ctypes
import ctypes.wintypes as wt
import json
import os
import random
import re
import shutil
import statistics
import subprocess
import tempfile
import threading
import time
from pathlib import Path
import os as _os, sys as _sys  # noqa: E401
_sys.path.insert(0, _os.path.join(_os.path.dirname(_os.path.abspath(__file__)), ".."))
import devenv  # noqa: E402

user32 = ctypes.WinDLL("user32", use_last_error=True)
WM_MOUSEWHEEL = 0x020A
WM_CLOSE = 0x0010
SW_SHOWNOACTIVATE = 4
X, Y = devenv.window_pos()
W, H = 1000, 800
LINE = re.compile(r"input→present ([\d.]+) ms")
FRAME = re.compile(r"\[gpu\] frame \d+ interval ([\d.]+)")

user32.PostMessageW.argtypes = [wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM]
user32.GetForegroundWindow.restype = wt.HWND
ENUM = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)


def window_of(pid):
    found = []

    def each(hwnd, _):
        owner = wt.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value == pid and user32.IsWindowVisible(hwnd):
            found.append(hwnd)
        return True

    user32.EnumWindows(ENUM(each), 0)
    return found[0] if found else None


def page(path):
    rows = "".join(
        f"<p>Row {i}: scrolling text for the compositor to carry, with enough words to fill a line of the page.</p>"
        for i in range(4000)
    )
    path.write_text(f"<!doctype html><title>scroll</title><body>{rows}", encoding="utf-8")


def pct(v, p):
    if not v:
        return 0.0
    v = sorted(v)
    return v[round((len(v) - 1) * p)]


def old_names(env):
    """Each SNOWLARK_<name> also as OSMIUM_<name>: a build from before the rename, given for comparison, reads
    only the old name (its window must not take the foreground either)."""
    for k in [k for k in env if k.startswith("SNOWLARK_")]:
        env["OSMIUM_" + k[len("SNOWLARK_"):]] = env[k]
    return env


def run_once(exe, profile, url, env_pair, secs, seed):
    env = {k: v for k, v in os.environ.items() if not k.startswith(("SNOWLARK_", "OSMIUM_"))}
    env["SNOWLARK_GPU_STATS"] = "1"
    env["SNOWLARK_PROFILE_DIR"] = str(profile)
    env["SNOWLARK_NO_ACTIVATE"] = "1"
    if env_pair:
        name, value = env_pair.split("=", 1)
        env[name] = value
    old_names(env)
    shutil.rmtree(profile, ignore_errors=True)
    profile.mkdir(parents=True)
    (profile / "window.json").write_text(
        json.dumps({"x": X, "y": Y, "width": W, "height": H, "maximized": False, "dpi": 96})
    )
    info = subprocess.STARTUPINFO()
    info.dwFlags = subprocess.STARTF_USESHOWWINDOW
    info.wShowWindow = SW_SHOWNOACTIVATE
    before = user32.GetForegroundWindow()
    p = subprocess.Popen(
        [str(exe), url],
        env=env,
        startupinfo=info,
        creationflags=subprocess.CREATE_NO_WINDOW,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    lines = []
    reader = threading.Thread(
        target=lambda: lines.extend(
            l.decode("utf-8", "replace") for l in p.stderr
        ),
        daemon=True,
    )
    reader.start()
    try:
        hwnd = None
        end = time.time() + 30
        while hwnd is None and time.time() < end:
            hwnd = window_of(p.pid)
            time.sleep(0.05)
        if hwnd is None:
            raise RuntimeError("no window")
        time.sleep(3)  # the page loads and paints
        fg = user32.GetForegroundWindow()
        if fg == hwnd or fg != before and window_of(p.pid) == fg:
            raise RuntimeError("the test window took the foreground")
        rng = random.Random(seed)
        x, y = X + W // 2, Y + H // 2
        lp = (y << 16) | x
        down = ((-120) & 0xFFFF) << 16
        up = 120 << 16
        stop = time.time() + secs
        n = 0
        while time.time() < stop:
            burst = time.time() + 1.5
            delta = down if n % 8 < 6 else up
            while time.time() < burst and time.time() < stop:
                user32.PostMessageW(hwnd, WM_MOUSEWHEEL, delta, lp)
                time.sleep(rng.uniform(0.02, 0.04))
            n += 1
            time.sleep(0.4)
        user32.PostMessageW(hwnd, WM_CLOSE, 0, 0)
        try:
            p.wait(20)
        except subprocess.TimeoutExpired:
            pass
        fg = user32.GetForegroundWindow()
        if fg == hwnd:
            raise RuntimeError("the test window took the foreground")
    finally:
        if p.poll() is None:
            p.kill()  # (this child, by its handle)
            p.wait(10)
    reader.join(5)
    for l in [l for l in lines if l.startswith("[ipc]")][:5]:
        print(l.rstrip())
    lat, gaps = [], []
    for l in lines:
        m = LINE.search(l)
        if m:
            lat.append(float(m.group(1)))
        m = FRAME.match(l)
        if m:
            gaps.append(float(m.group(1)))
    return lat, gaps


def main():
    a = argparse.ArgumentParser()
    a.add_argument("exe", type=Path)
    a.add_argument("--rounds", type=int, default=10)
    a.add_argument("--secs", type=float, default=10)
    a.add_argument("--a", default="")
    a.add_argument("--b", default="")
    a.add_argument("--smoke", action="store_true", help="one run of arm A, and what it saw")
    args = a.parse_args()
    root = Path(tempfile.mkdtemp(prefix="snowlark-scroll-ab-"))
    html = root / "page.html"
    page(html)
    url = html.as_uri()
    if args.smoke:
        lat, gaps = run_once(args.exe, root / "profile", url, args.a, args.secs, 1)
        print(f"smoke: {len(lat)} inputs, p50 {pct(lat, .5):.2f} p95 {pct(lat, .95):.2f} ms; {len(gaps)} frames")
        shutil.rmtree(root, ignore_errors=True)
        return
    arms = {"A": [], "B": []}
    rows = []
    for r in range(args.rounds):
        order = ["A", "B"] if r % 2 == 0 else ["B", "A"]
        got = {}
        for arm in order:
            pair = args.a if arm == "A" else args.b
            got[arm] = run_once(args.exe, root / "profile", url, pair, args.secs, r)
            arms[arm].append(got[arm])
        pa, pb = pct(got["A"][0], 0.95), pct(got["B"][0], 0.95)
        rows.append(pb - pa)
        print(
            f"round {r + 1} ({order[0]} first): A p50 {pct(got['A'][0], .5):.2f} p95 {pa:.2f} ({len(got['A'][0])} inputs); "
            f"B p50 {pct(got['B'][0], .5):.2f} p95 {pb:.2f} ({len(got['B'][0])}); B-A {pb - pa:+.2f}",
            flush=True,
        )
    for arm, pair in (("A", args.a), ("B", args.b)):
        lat = [x for r in arms[arm] for x in r[0]]
        gaps = [x for r in arms[arm] for x in r[1] if x < 100]
        print(
            f"{arm} [{pair or 'as built'}]: {len(lat)} inputs p50 {pct(lat, .5):.2f} p95 {pct(lat, .95):.2f} "
            f"p99 {pct(lat, .99):.2f} max {max(lat or [0]):.2f} ms; frame interval p50 {pct(gaps, .5):.2f} p95 {pct(gaps, .95):.2f} ms"
        )
    print(
        f"paired p95 B-A by round {[round(x, 2) for x in rows]}; median {statistics.median(rows):+.2f}; "
        f"B better in {sum(1 for x in rows if x < 0)} of {len(rows)}"
    )
    shutil.rmtree(root, ignore_errors=True)


main()
