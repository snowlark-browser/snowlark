#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Times a drag across a video player's seek bar (tools/scrub/scrub.html), in Snowlark's window or in
headless Chrome.

    python tools/scrub/run.py snowlark <snowlark-cli.exe> [--query preview] [--rate 1000] [--secs 6]
    python tools/scrub/run.py chrome [--query preview] [--rate 1000] [--secs 6]

Snowlark: a fresh window at `SNOWLARK_TEST_WINDOW_POS` with a profile of its own, `SNOWLARK_NO_ACTIVATE=1`,
`SNOWLARK_TAB_STATS=1` and `SNOWLARK_GPU_STATS=1`. The mouse is never moved: `WM_MOUSEMOVE` (with the
left button down after a `WM_LBUTTONDOWN` on the bar) is posted to the window at `--rate` per second.
A run that takes the foreground stops at once. Chrome: headless, the same moves sent over the
DevTools protocol (`Input.dispatchMouseEvent`) without waiting for each reply.

The page reports each second in its title: pointermoves, animation frames, frames in which the thumb
moved, its listener's time and the 95th percentile frame gap. For Snowlark the engine's own lines are
summarised too: rasters (ms, tiles painted and reused), the tab's input batches, and the
compositor's frames. `--env NAME=VALUE` adds a variable to the window's environment.
"""
import argparse
import ctypes
import ctypes.wintypes as wt
import functools
import http.server
import json
import os
import re
import shutil
import statistics as st
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
import os as _os, sys as _sys  # noqa: E401
_sys.path.insert(0, _os.path.join(_os.path.dirname(_os.path.abspath(__file__)), ".."))
import devenv  # noqa: E402

HERE = Path(__file__).resolve().parent
user32 = ctypes.WinDLL("user32", use_last_error=True)
user32.PostMessageW.argtypes = [wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM]
user32.GetForegroundWindow.restype = wt.HWND
user32.GetWindowTextW.argtypes = [wt.HWND, wt.LPWSTR, ctypes.c_int]
ENUM = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
ctypes.WinDLL("winmm").timeBeginPeriod(1)
WM_MOUSEMOVE, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_CLOSE = 0x0200, 0x0201, 0x0202, 0x0010
MK_LBUTTON = 1
SW_SHOWNOACTIVATE = 4
X, Y = devenv.window_pos()
W, H = 1320, 900
TITLE = re.compile(
    r"scrub t=(\d+) pm=(\d+) raf=(\d+) mv=(\d+) ms=([\d.]+) gap=([\d.]+) dpr=([\d.]+) at=(\S+) bar=(\d+),(\d+),(\d+)"
)


class Server(http.server.SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass


class Quiet(http.server.ThreadingHTTPServer):
    def handle_error(self, request, client_address):
        pass


def serve():
    httpd = Quiet(("127.0.0.1", 0), functools.partial(Server, directory=str(HERE)))
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd, httpd.server_address[1]


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


def title_of(hwnd):
    buf = ctypes.create_unicode_buffer(512)
    user32.GetWindowTextW(hwnd, buf, 512)
    return buf.value


def lp(x, y):
    return (int(y) & 0xFFFF) << 16 | (int(x) & 0xFFFF)


def sweep(post, x0, x1, y, rate, secs):
    """Posts moves from x0 to x1 and back, `rate` a second for `secs`. Returns how many."""
    n = int(rate * secs)
    period = 1.0 / rate
    t0 = time.perf_counter()
    span = x1 - x0
    for i in range(n):
        # (one sweep each way per 1.5 s)
        phase = (i * period / 1.5) % 2.0
        f = phase if phase < 1.0 else 2.0 - phase
        post(x0 + f * span, y)
        due = t0 + (i + 1) * period
        while True:
            left = due - time.perf_counter()
            if left <= 0:
                break
            if left > 0.002:
                time.sleep(left - 0.0015)
    return n


def summarise_page(lines, label):
    # (the first second and the last are partial)
    rows = [tuple(float(v) for v in m.groups()[1:6]) for m in lines[1:-1]]
    if not rows:
        print(f"{label}: no page lines")
        return
    pm, raf, mv, ms, gap = (st.median(c) for c in zip(*rows))
    print(
        f"{label}: per second (median of {len(rows)}): pointermove {pm:.0f}, rAF {raf:.0f}, frames with a new"
        f" thumb position {mv:.0f}, listener {ms:.1f} ms ({ms / max(pm, 1) * 1000:.0f} us a move), p95 frame gap {gap:.1f} ms"
    )


RASTER = re.compile(r"raster (\d+)x(\d+) band [^:]*: ([\d.]+) ms \((\d+) tiles, (\d+) painted, (\d+) reused, (\d+) layers.*lists (\d+) KB, encode ([\d.]+) ms, decode ([\d.]+) ms")
SPLIT = re.compile(r"decode [\d.]+ ms; prepare ([\d.]+) ms, paint ([\d.]+) ms, copy ([\d.]+) ms")
INPUT = re.compile(r"input (\d+) \(moves (\d+), handled (\d+)\): commands ([\d.]+) ms, update ([\d.]+) ms, display list ([\d.]+) ms, total ([\d.]+) ms")
FRAME = re.compile(r"\[gpu\] frame \d+ interval ([\d.]+)")


def numbers(regex, text):
    # (the renderer's and the browser's lines can interleave on the shared stderr: those are skipped)
    out = []
    for m in regex.finditer(text):
        try:
            out.append(tuple(float(v) for v in m.groups()))
        except ValueError:
            pass
    return out


def summarise_engine(text, secs):
    r = numbers(RASTER, text)
    i = numbers(INPUT, text)
    f = [x[0] for x in numbers(FRAME, text)]
    if r:
        ms = [x[2] for x in r]
        print(
            f"  rasters: {len(r) / secs:.0f}/s, {st.median(ms):.2f} ms median ({sum(ms) / secs:.0f} ms a second),"
            f" painted {st.mean(x[4] for x in r):.1f}, reused {st.mean(x[5] for x in r):.1f}, layers {st.mean(x[6] for x in r):.1f},"
            f" lists {st.median(x[7] for x in r):.0f} KB, encode {st.median(x[8] for x in r):.2f} ms, decode {st.median(x[9] for x in r):.2f} ms"
        )
        split = numbers(SPLIT, text)
        if split:
            p, d, c = (sum(col) / secs for col in zip(*split))
            print(
                f"  raster thread a second: prepare {p:.0f} ms, paint {d:.0f} ms, copy {c:.0f} ms,"
                f" the rest {sum(ms) / secs - p - d - c:.0f} ms"
            )
    if i:
        moves = sum(x[1] for x in i)
        handled = sum(x[2] for x in i)
        cmd = sum(x[3] for x in i)
        upd = sum(x[4] for x in i)
        lst = sum(x[5] for x in i)
        tot = sum(x[6] for x in i)
        print(
            f"  tab: {len(i) / secs:.0f} batches/s, moves {moves / secs:.0f}/s handled {handled / secs:.0f}/s;"
            f" a second: commands {cmd / secs:.0f} ms, update {upd / secs:.0f} ms, display list {lst / secs:.0f} ms,"
            f" total {tot / secs:.0f} ms ({cmd / max(handled, 1) * 1000:.0f} us a handled move)"
        )
    if f:
        print(f"  compositor: {len(f) / secs:.0f} frames/s, interval median {st.median(f):.2f} ms")


def run_snowlark(exe, url, args):
    folder = Path(tempfile.mkdtemp(prefix="scrub-"))
    profile = folder / "profile"
    profile.mkdir()
    (profile / "window.json").write_text(
        json.dumps({"x": X, "y": Y, "width": W, "height": H, "maximized": False, "dpi": 96})
    )
    env = {k: v for k, v in os.environ.items() if not k.startswith("SNOWLARK_")}
    env.update(
        SNOWLARK_PROFILE_DIR=str(profile), SNOWLARK_NO_ACTIVATE="1", SNOWLARK_TAB_STATS="1", SNOWLARK_GPU_STATS="1"
    )
    for kv in args.env:
        k, _, v = kv.partition("=")
        env[k] = v
    info = subprocess.STARTUPINFO()
    info.dwFlags = subprocess.STARTF_USESHOWWINDOW
    info.wShowWindow = SW_SHOWNOACTIVATE
    err_path = folder / "stderr.txt"
    seen = {}
    with open(err_path, "wb") as err:
        p = subprocess.Popen(
            [str(exe), url], env=env, startupinfo=info, creationflags=subprocess.CREATE_NO_WINDOW,
            stdout=subprocess.DEVNULL, stderr=err,
        )
        print(f"snowlark pid {p.pid}, output {folder}")
        try:
            hwnd = None
            end = time.time() + 30
            while hwnd is None and time.time() < end:
                hwnd = window_of(p.pid)
                time.sleep(0.05)
            if hwnd is None:
                raise RuntimeError("no window")

            def guard():
                if user32.GetForegroundWindow() == hwnd:
                    raise RuntimeError("the test window took the foreground")

            def wait_line(after_t, timeout=20):
                end = time.time() + timeout
                while time.time() < end:
                    guard()
                    m = TITLE.search(title_of(hwnd))
                    if m and int(m.group(1)) > after_t:
                        return m
                    time.sleep(0.05)
                raise RuntimeError("no title line from the page: " + title_of(hwnd))

            m = wait_line(2, 40)
            # Where the page is in the client area: a move to a known point, read back.
            cal = (400, 500)
            user32.PostMessageW(hwnd, WM_MOUSEMOVE, 0, lp(*cal))
            m = wait_line(int(m.group(1)))
            m = wait_line(int(m.group(1)))
            dpr = float(m.group(7))
            ax, ay = (int(v) for v in m.group(8).split(","))
            ox, oy = cal[0] - ax * dpr, cal[1] - ay * dpr
            x0, x1, y = (int(m.group(i)) for i in (9, 10, 11))
            dx0, dx1, dy = ox + (x0 + 4) * dpr, ox + (x1 - 4) * dpr, oy + y * dpr
            print(f"  dpr {dpr}, content at ({ox:.0f}, {oy:.0f}); bar {dx0:.0f}..{dx1:.0f} at y {dy:.0f}")
            start_t = int(m.group(1))
            user32.PostMessageW(hwnd, WM_MOUSEMOVE, 0, lp(dx0, dy))
            user32.PostMessageW(hwnd, WM_LBUTTONDOWN, MK_LBUTTON, lp(dx0, dy))
            mark = len(err_path.read_bytes()) if err_path.exists() else 0
            stop = threading.Event()

            def watch():
                while not stop.is_set():
                    mm = TITLE.search(title_of(hwnd))
                    if mm:
                        seen[int(mm.group(1))] = mm
                    time.sleep(0.1)

            w = threading.Thread(target=watch, daemon=True)
            w.start()
            n = sweep(
                lambda x, yy: user32.PostMessageW(hwnd, WM_MOUSEMOVE, MK_LBUTTON, lp(x, yy)),
                dx0, dx1, dy, args.rate, args.secs,
            )
            user32.PostMessageW(hwnd, WM_LBUTTONUP, 0, lp(dx1, dy))
            time.sleep(1.2)
            stop.set()
            w.join()
            guard()
            err.flush()
            text = err_path.read_bytes()[mark:].decode("utf-8", "replace")
            lines = [seen[t] for t in sorted(seen) if t > start_t]
            vt = re.search(r"vt=(\S+ vs=\S+)", title_of(hwnd))
            print(f"  posted {n} moves in {args.secs} s; video time {vt.group(1) if vt else '?'}")
            summarise_page(lines, f"snowlark {args.query or '-'}")
            summarise_engine(text, args.secs)
            user32.PostMessageW(hwnd, WM_CLOSE, 0, 0)
            try:
                p.wait(20)
            except subprocess.TimeoutExpired:
                pass
        finally:
            if p.poll() is None:
                p.kill()  # (this child, by its handle)
                p.wait(10)
    if not args.keep:
        shutil.rmtree(folder, ignore_errors=True)


def run_chrome(url, args):
    sys.path.insert(0, str(HERE.parent / "layout-diff"))
    from chrome import Chrome  # noqa: E402

    c = Chrome(width=1320, height=800)
    try:
        s = c.new_page(1320, 800)
        c.navigate(s, url)
        c.drain(3)
        bar = c.evaluate(s, "(() => { const r = document.querySelector('.bar').getBoundingClientRect(); return [r.left, r.right, r.top + r.height / 2]; })()")
        x0, x1, y = bar[0] + 4, bar[1] - 4, bar[2]

        def send(method, params):
            c.next_id += 1
            c.ws.send(json.dumps({"id": c.next_id, "method": method, "params": params, "sessionId": s}))

        send("Input.dispatchMouseEvent", {"type": "mouseMoved", "x": x0, "y": y})
        send("Input.dispatchMouseEvent", {"type": "mousePressed", "x": x0, "y": y, "button": "left", "buttons": 1, "clickCount": 1})
        start = c.evaluate(s, "(window.scrubLog || []).length")
        # (replies are read meanwhile, so the socket never backs up)
        stop = threading.Event()

        def drain():
            while not stop.is_set():
                try:
                    c.ws.recv(0.2)
                except Exception:
                    pass

        d = threading.Thread(target=drain, daemon=True)
        d.start()
        n = sweep(
            lambda x, yy: send("Input.dispatchMouseEvent", {"type": "mouseMoved", "x": x, "y": yy, "button": "left", "buttons": 1}),
            x0, x1, y, args.rate, args.secs,
        )
        send("Input.dispatchMouseEvent", {"type": "mouseReleased", "x": x1, "y": y, "button": "left", "buttons": 0, "clickCount": 1})
        time.sleep(1.2)
        stop.set()
        d.join()
        log = c.evaluate(s, "window.scrubLog || []")
        lines = [m for m in (TITLE.search(l) for l in log[start:]) if m]
        print(f"  posted {n} moves in {args.secs} s")
        summarise_page(lines, f"chrome {args.query or '-'}")
    finally:
        c.close()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("browser", choices=["snowlark", "chrome"])
    ap.add_argument("exe", nargs="?")
    ap.add_argument("--query", default="", help="the page's switches, for example preview&left")
    ap.add_argument("--rate", type=int, default=1000)
    ap.add_argument("--secs", type=float, default=6)
    ap.add_argument("--env", action="append", default=[])
    ap.add_argument("--keep", action="store_true", help="keep the run's folder (stderr.txt)")
    ap.add_argument("--video", default="http://localhost:8000/media/movie_300.mp4")
    args = ap.parse_args()
    httpd, port = serve()
    # (the video from the WPT server, which serves media as browsers expect)
    query = "&".join(q for q in [args.query, f"src={args.video}"] if q)
    url = f"http://127.0.0.1:{port}/scrub.html?{query}"
    try:
        if args.browser == "snowlark":
            run_snowlark(Path(args.exe), url, args)
        else:
            run_chrome(url, args)
    finally:
        httpd.shutdown()


if __name__ == "__main__":
    main()
