#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""A drag across YouTube's seek bar, in Snowlark's window and in Chrome's, both at `SNOWLARK_TEST_WINDOW_POS`.

    python tools/scrub/youtube.py snowlark <snowlark-cli.exe> [--paused] [--rate 1000] [--secs 6] [--env K=V]
    python tools/scrub/youtube.py chrome [--paused]

The page (yt.html) embeds a public video at 1280x720, muted (nobody signs in; an embed has no consent
page). Moves are posted to the window (`WM_MOUSEMOVE`, the left button down after `WM_LBUTTONDOWN` on
the bar), so both browsers take them through their own input path; the mouse is never moved. A
window that takes the foreground stops the run.

Snowlark: the engine's lines (`SNOWLARK_TAB_STATS`, `SNOWLARK_GPU_STATS`) summarised as in run.py.
Chrome: started with a DevTools port, and the measuring snippet (snippet_a.js next to this file) runs
inside the player's frame: pointermoves, coalesced moves and animation frames a second, seeks, what
the drag shows and changes.
"""
import argparse
import ctypes
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "layout-diff"))
sys.path.insert(0, str(HERE))
import run as scrub  # noqa: E402

user32 = scrub.user32
CHROME = r"C:\Program Files\Google\Chrome\Application\chrome.exe"
# The seek bar in the 1280x720 embed (CSS px, from Chrome): its ends and its middle's height.
BAR = (30, 1250, 646)


def find_window(pid, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        h = scrub.window_of(pid)
        if h:
            return h
        time.sleep(0.05)
    raise RuntimeError("no window")


def wait_title(hwnd, pred, timeout, guard):
    end = time.time() + timeout
    while time.time() < end:
        guard()
        t = scrub.title_of(hwnd)
        if pred(t):
            return t
        time.sleep(0.1)
    raise RuntimeError("title never matched: " + scrub.title_of(hwnd))


def drive(hwnd, args, guard):
    """Finds the page in the window, waits for playback, then drags. Returns when the button is up."""
    try:
        t = wait_title(hwnd, lambda t: " state=1 " in t or (args.paused and " state=" in t and "state=-2" not in t), 40, guard)
    except RuntimeError as e:
        print("  not playing:", e)
    time.sleep(3)
    cal = (1300, 760)
    user32.PostMessageW(hwnd, scrub.WM_MOUSEMOVE, 0, scrub.lp(*cal))
    t = wait_title(hwnd, lambda t: "at=none" not in t, 10, guard)
    at = t.split("at=")[1].split()[0]
    dpr = float(t.split("dpr=")[1].split()[0])
    ax, ay = (int(v) for v in at.split(","))
    ox, oy = cal[0] - ax * dpr, cal[1] - ay * dpr
    x0, x1, y = (ox + (BAR[0] + 20) * dpr, ox + (BAR[1] - 20) * dpr, oy + BAR[2] * dpr)
    print(f"  page at ({ox:.0f}, {oy:.0f}), dpr {dpr}; bar {x0:.0f}..{x1:.0f} at {y:.0f}; {t}")
    # (a move over the player shows its controls)
    for i in range(20):
        user32.PostMessageW(hwnd, scrub.WM_MOUSEMOVE, 0, scrub.lp(ox + 640 * dpr + i, oy + 400 * dpr))
        time.sleep(0.02)
    for i in range(10):
        user32.PostMessageW(hwnd, scrub.WM_MOUSEMOVE, 0, scrub.lp(x0, y - 10 + i))
        time.sleep(0.02)
    return x0, x1, y


def snowlark(args):
    folder = Path(tempfile.mkdtemp(prefix="scrub-yt-"))
    profile = folder / "profile"
    profile.mkdir()
    (profile / "window.json").write_text(
        json.dumps({"x": scrub.X, "y": scrub.Y, "width": scrub.W, "height": scrub.H, "maximized": False, "dpi": 96})
    )
    env = {k: v for k, v in os.environ.items() if not k.startswith("SNOWLARK_")}
    env.update(SNOWLARK_PROFILE_DIR=str(profile), SNOWLARK_NO_ACTIVATE="1", SNOWLARK_TAB_STATS="1", SNOWLARK_GPU_STATS="1")
    for kv in args.env:
        k, _, v = kv.partition("=")
        env[k] = v.replace("{run}", str(folder))
    info = subprocess.STARTUPINFO()
    info.dwFlags = subprocess.STARTF_USESHOWWINDOW
    info.wShowWindow = scrub.SW_SHOWNOACTIVATE
    err_path = folder / "stderr.txt"
    with open(err_path, "wb") as err:
        p = subprocess.Popen([args.exe, args.url], env=env, startupinfo=info, creationflags=subprocess.CREATE_NO_WINDOW,
                             stdout=subprocess.DEVNULL, stderr=err)
        print(f"snowlark pid {p.pid}, output {folder}")
        try:
            hwnd = find_window(p.pid)

            def guard():
                if user32.GetForegroundWindow() == hwnd:
                    raise RuntimeError("the test window took the foreground")

            x0, x1, y = drive(hwnd, args, guard)
            mark = err_path.stat().st_size
            user32.PostMessageW(hwnd, scrub.WM_LBUTTONDOWN, scrub.MK_LBUTTON, scrub.lp(x0, y))
            n = scrub.sweep(lambda x, yy: user32.PostMessageW(hwnd, scrub.WM_MOUSEMOVE, scrub.MK_LBUTTON, scrub.lp(x, yy)),
                            x0, x1, y, args.rate, args.secs)
            user32.PostMessageW(hwnd, scrub.WM_LBUTTONUP, 0, scrub.lp(x1, y))
            time.sleep(1)
            guard()
            err.flush()
            text = err_path.read_bytes()[mark:].decode("utf-8", "replace")
            if args.save:
                Path(args.save).write_text(text, encoding="utf-8")
            print(f"  posted {n} moves in {args.secs} s; {scrub.title_of(hwnd)}")
            scrub.summarise_engine(text, args.secs)
            user32.PostMessageW(hwnd, scrub.WM_CLOSE, 0, 0)
            try:
                p.wait(20)
            except subprocess.TimeoutExpired:
                pass
        finally:
            if p.poll() is None:
                p.kill()
                p.wait(10)
    if not args.keep:
        shutil.rmtree(folder, ignore_errors=True)


def chrome(args):
    from chrome import WebSocket  # noqa: E402

    folder = Path(tempfile.mkdtemp(prefix="scrub-yt-chrome-"))
    cmd = [CHROME, f"--user-data-dir={folder}", "--no-first-run", "--no-default-browser-check",
           f"--window-position={scrub.X},{scrub.Y}", f"--window-size={scrub.W},{scrub.H}",
           "--remote-debugging-port=0", "--autoplay-policy=no-user-gesture-required", "--disable-extensions", "--disable-backgrounding-occluded-windows", "--disable-renderer-backgrounding",
           "--disable-background-timer-throttling",
           f"--app={args.url}"]
    info = subprocess.STARTUPINFO()
    info.dwFlags = subprocess.STARTF_USESHOWWINDOW
    info.wShowWindow = scrub.SW_SHOWNOACTIVATE
    p = subprocess.Popen(cmd, startupinfo=info, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    print(f"chrome pid {p.pid}, profile {folder}")
    try:
        hwnd = find_window(p.pid)

        def guard():
            if user32.GetForegroundWindow() == hwnd:
                raise RuntimeError("the test window took the foreground")

        port_file = folder / "DevToolsActivePort"
        while not port_file.exists():
            time.sleep(0.1)
        port, path = port_file.read_text().split()[:2]
        ws = WebSocket(f"ws://127.0.0.1:{port}{path}")
        ids = iter(range(1, 1 << 30))

        def call(method, params=None, session=None):
            mid = next(ids)
            msg = {"id": mid, "method": method, "params": params or {}}
            if session:
                msg["sessionId"] = session
            ws.send(json.dumps(msg))
            while True:
                r = json.loads(ws.recv(30))
                if r.get("id") == mid:
                    if "error" in r:
                        raise RuntimeError(f"{method}: {r['error']}")
                    return r.get("result", {})

        x0, x1, y = drive(hwnd, args, guard)
        frames = [t for t in call("Target.getTargets")["targetInfos"] if t["type"] == "iframe" and "youtube" in t["url"]]
        if not frames:
            raise RuntimeError("no player frame target")
        session = call("Target.attachToTarget", {"targetId": frames[0]["targetId"], "flatten": True})["sessionId"]
        bar = call("Runtime.evaluate", {"expression": "(() => { const r = document.querySelector('.ytp-progress-bar').getBoundingClientRect(); return [r.left, r.right, r.top, r.bottom]; })()", "returnByValue": True}, session)
        print("  the bar in the player:", bar["result"].get("value"))
        err = call("Runtime.evaluate", {"expression": "(document.querySelector('.ytp-error') || {}).innerText + ' | ' + document.querySelector('video')?.readyState + ' ' + document.querySelector('video')?.src", "returnByValue": True}, session)
        print("  player:", err["result"].get("value"))
        cls = call("Runtime.evaluate", {"expression": "[...document.querySelectorAll('[class*=progress]')].slice(0, 12).map(e => e.className + ' ' + JSON.stringify(e.getBoundingClientRect())).join('\\n') + ' || ' + innerWidth + 'x' + innerHeight", "returnByValue": True}, session)
        print("  progress elements:", cls["result"].get("value"))
        snippet = (HERE / "snippet_a.js").read_text()
        call("Runtime.evaluate", {"expression": snippet}, session)
        user32.PostMessageW(hwnd, scrub.WM_LBUTTONDOWN, scrub.MK_LBUTTON, scrub.lp(x0, y))
        n = scrub.sweep(lambda x, yy: user32.PostMessageW(hwnd, scrub.WM_MOUSEMOVE, scrub.MK_LBUTTON, scrub.lp(x, yy)),
                        x0, x1, y, args.rate, 5.3)
        user32.PostMessageW(hwnd, scrub.WM_LBUTTONUP, 0, scrub.lp(x1, y))
        time.sleep(0.5)
        guard()
        a = call("Runtime.evaluate", {"expression": "window.__A", "returnByValue": True}, session)["result"].get("value")
        print(f"  posted {n} moves; {scrub.title_of(hwnd)}")
        print(json.dumps(a, indent=None)[:3000])
        ws.close()
        user32.PostMessageW(hwnd, scrub.WM_CLOSE, 0, 0)
        try:
            p.wait(20)
        except subprocess.TimeoutExpired:
            pass
    finally:
        if p.poll() is None:
            p.kill()
            p.wait(10)
        shutil.rmtree(folder, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("browser", choices=["snowlark", "chrome"])
    ap.add_argument("exe", nargs="?")
    ap.add_argument("--video", default="aqz-KE-bpKQ")
    ap.add_argument("--paused", action="store_true")
    ap.add_argument("--rate", type=int, default=1000)
    ap.add_argument("--secs", type=float, default=6)
    ap.add_argument("--env", action="append", default=[])
    ap.add_argument("--save", help="write the engine's lines of the drag to this file")
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()
    httpd, port = scrub.serve()
    args.url = f"http://localhost:{port}/yt.html?v={args.video}" + ("&paused" if args.paused else "")
    try:
        snowlark(args) if args.browser == "snowlark" else chrome(args)
    finally:
        httpd.shutdown()


if __name__ == "__main__":
    main()
