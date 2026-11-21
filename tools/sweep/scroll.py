# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Part 3: scrolling each corpus site in Snowlark's own window, counting pinned rasters and blank rows.

The window opens at `SNOWLARK_TEST_WINDOW_POS` (see `tools/devenv.py`) with a profile of its own,
`SNOWLARK_NO_ACTIVATE=1` and every tab on the browser's process (so one stderr has every line). After the
page has had `--load` seconds, wheel messages are posted to the window (down for 40, up for 40, ...) for
`--secs` seconds; nothing is captured from the screen and the window never takes the foreground (the run
stops if it does). `SNOWLARK_GPU_STATS=1` and `SNOWLARK_TAB_STATS=1` report what happened:
- a raster of a **pinned** display list must be repainted at every scroll position, so until it is, the
  compositor shows the last frame moved by the scroll (fixed and sticky content slides, then jumps back);
- an **UNCOVERED** frame showed rows of the page area that no tile covered (a blank flash).
`parse_log` reads those lines; `rank` orders sites by pinned share, then blank rows.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))

RASTER = re.compile(r"\] raster \d+x\d+")
FRAME = re.compile(r"\[gpu\] frame \d+ .*?(?: page \d+x(\d+) area (\d+)x(\d+))?"
                   r"(?: UNCOVERED (\d+) rows at scroll (-?\d+))?(?: protected \d+)?\s*$")


def parse_log(lines: list[str]) -> dict:
    """Counts in the stderr lines of a scroll: rasters (pinned), frames, uncovered frames and rows."""
    rasters = pinned = frames = uncovered = rows = area_rows = 0
    worst = 0
    example = ""
    for line in lines:
        if RASTER.search(line):
            rasters += 1
            if ", pinned" in line:
                pinned += 1
                if not example:
                    example = line.strip()[:240]
            continue
        m = FRAME.search(line)
        if m:
            frames += 1
            if m.group(3):
                area_rows += int(m.group(3))
            if m.group(4):
                uncovered += 1
                rows += int(m.group(4))
                worst = max(worst, int(m.group(4)))
    return {
        "rasters": rasters,
        "pinned": pinned,
        "pinned_share": round(pinned / rasters, 3) if rasters else 0.0,
        "frames": frames,
        "uncovered_frames": uncovered,
        "uncovered_rows": rows,
        "uncovered_share": round(rows / area_rows, 4) if area_rows else 0.0,
        "worst_rows": worst,
        "pinned_example": example,
    }


def rank(results: list[dict]) -> list[dict]:
    """Sites ordered by pinned share, then the share of blank rows."""
    ok = [r for r in results if not r.get("error")]
    return sorted(ok, key=lambda r: (-r["pinned_share"], -r["uncovered_share"], r["site"]))


def scroll_site(exe: str, url: str, scale: float, log_path: Path, load: float = 10, secs: float = 12,
                width: int = 1440, height: int = 900) -> dict:
    """Opens `url` in a window of its own, posts wheel bursts, closes it; returns `parse_log`'s counts."""
    import ctypes
    import ctypes.wintypes as wt

    import devenv

    user32 = ctypes.WinDLL("user32", use_last_error=True)
    WM_MOUSEWHEEL, WM_CLOSE, SW_SHOWNOACTIVATE = 0x020A, 0x0010, 4
    user32.PostMessageW.argtypes = [wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM]
    user32.GetForegroundWindow.restype = wt.HWND
    enum_proc = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
    x0, y0 = devenv.window_pos()

    def window_of(pid):
        found = []

        def each(hwnd, _):
            owner = wt.DWORD()
            user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
            if owner.value == pid and user32.IsWindowVisible(hwnd):
                found.append(hwnd)
            return True

        user32.EnumWindows(enum_proc(each), 0)
        return found[0] if found else None

    profile = Path(tempfile.mkdtemp(prefix="snowlark-sweep-"))
    env = {k: v for k, v in os.environ.items() if not k.startswith(("SNOWLARK_", "OSMIUM_"))}
    env.update(SNOWLARK_GPU_STATS="1", SNOWLARK_TAB_STATS="1", SNOWLARK_PROFILE_DIR=str(profile),
               SNOWLARK_NO_ACTIVATE="1", SNOWLARK_PROCESS_MODEL="single", SNOWLARK_NO_SESSION="1",
               SNOWLARK_SCALE=str(scale))
    (profile / "window.json").write_text(json.dumps({"x": x0, "y": y0, "width": width, "height": height,
                                                     "maximized": False, "dpi": 96}))
    info = subprocess.STARTUPINFO()
    info.dwFlags = subprocess.STARTF_USESHOWWINDOW
    info.wShowWindow = SW_SHOWNOACTIVATE
    p = subprocess.Popen([exe, url], env=env, startupinfo=info, creationflags=subprocess.CREATE_NO_WINDOW,
                         stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    lines: list[str] = []
    marks: list[int] = []
    reader = threading.Thread(target=lambda: lines.extend(l.decode("utf-8", "replace") for l in p.stderr),
                              daemon=True)
    reader.start()
    error = None
    try:
        hwnd = None
        end = time.time() + 30
        while hwnd is None and time.time() < end:
            hwnd = window_of(p.pid)
            time.sleep(0.05)
        if hwnd is None:
            raise RuntimeError("no window")
        time.sleep(load)
        if user32.GetForegroundWindow() == hwnd:
            raise RuntimeError("the test window took the foreground")
        lp = ((y0 + height // 2) << 16) | (x0 + width // 2)
        marks.append(len(lines))
        stop = time.time() + secs
        n = 0
        while time.time() < stop:
            delta = (((-120) & 0xFFFF) << 16) if (n // 40) % 2 == 0 else (120 << 16)
            user32.PostMessageW(hwnd, WM_MOUSEWHEEL, delta, lp)
            time.sleep(0.03)
            n += 1
        time.sleep(0.5)
        marks.append(len(lines))
        user32.PostMessageW(hwnd, WM_CLOSE, 0, 0)
        try:
            p.wait(20)
        except subprocess.TimeoutExpired:
            pass
        if user32.GetForegroundWindow() == hwnd:
            raise RuntimeError("the test window took the foreground")
    except RuntimeError as e:
        error = str(e)
    finally:
        if p.poll() is None:
            subprocess.run(["taskkill", "/PID", str(p.pid), "/T", "/F"], capture_output=True)
            try:
                p.wait(10)
            except subprocess.TimeoutExpired:
                pass
        reader.join(5)
        shutil.rmtree(profile, ignore_errors=True)
    log_path.write_text("".join(lines), encoding="utf-8")
    during = lines[marks[0]:marks[1]] if len(marks) == 2 else lines
    out = parse_log(during)
    if error:
        out["error"] = error
    return out
