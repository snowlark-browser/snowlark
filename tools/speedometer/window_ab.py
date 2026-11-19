#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Alternating Speedometer runs in the real window, for what headless cannot show (raster, compositor).

    python tools/speedometer/window_ab.py --out <folder> --rounds 4 NAME=<snowlark-cli.exe> NAME=<snowlark-cli.exe> ...

Each round runs every arm once, the order rotating by round, each a fresh window of that binary with
a fresh profile. The first arm is the base of the comparison. Speedometer 3.1 is served by this script
on a free port, from a mirror on disk (`--mirror`) that fills from browserbench.org as files are first
asked for, so a run after the first touches no network: run one throwaway round to fill it. The local
clone (`../speedometer`) is 4.0 alpha, another benchmark. The server adds one listener to `index.html`:
when the run is over it posts the scores, after everything measured. So any binary can be run, with
nothing changed in it.

The window opens at `SNOWLARK_TEST_WINDOW_POS` (a monitor you are not working on) with `SNOWLARK_NO_ACTIVATE=1`, and a run that takes the foreground
stops everything. No other `SNOWLARK_` variable reaches it but those given with `--env` (for example
`--env SNOWLARK_PROFILE=1 --env SNOWLARK_PROFILE_FOLDED={run}/stacks.folded`), and `--arm-env NAME:SNOWLARK_X=V` gives
one to arm NAME only (two arms of one binary in two process models: `--arm-env tab:SNOWLARK_PROCESS_MODEL=tab`). `--suites` runs only those
suites. Each run also leaves `steps.json`, every step's sync and async times by iteration.
Each run leaves `<out>/<arm>-<round>/report.txt`
in the format of `snowlark-cli --speedometer`, and `stderr.txt`. The machine must be otherwise idle.
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
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
import os as _os, sys as _sys  # noqa: E401
_sys.path.insert(0, _os.path.join(_os.path.dirname(_os.path.abspath(__file__)), ".."))
import devenv  # noqa: E402

user32 = ctypes.WinDLL("user32", use_last_error=True)
user32.GetForegroundWindow.restype = wt.HWND
user32.PostMessageW.argtypes = [wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM]
ENUM = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
WM_CLOSE = 0x10
SW_SHOWNOACTIVATE = 4
X, Y = devenv.window_pos()
W, H = 1280, 1000

SITE = "https://browserbench.org/Speedometer3.1"

REPORTER = b"""<script>
addEventListener("DOMContentLoaded", () => {
  const finish = benchmarkClient.didFinishLastIteration;
  benchmarkClient.didFinishLastIteration = function (metrics) {
    finish.call(this, metrics);
    report(metrics);
  };
});
function report(m) {
  const fmt = (v, d) => v.toFixed(d).padStart(8);
  const values = (k) => (m[k] && m[k].values) || [];
  const out = ["score by iteration:" + values("Score").map((v) => fmt(v, 2)).join(""), "",
               "suite totals by iteration (ms), last/first:"];
  for (const s of Object.keys(m).filter((k) => !k.includes("/") && !/^(Iteration-|Geomean|Score)/.test(k))) {
    const v = values(s);
    out.push(s.padEnd(45) + v.map((x) => fmt(x, 1)).join("") + "   x" + (v[v.length - 1] / v[0]).toFixed(2));
  }
  const steps = Object.fromEntries(Object.keys(m).filter((k) => k.includes("/")).map((k) => [k, values(k)]));
  fetch("/__results", { method: "POST", body: JSON.stringify(steps) }).finally(() =>
    fetch("/__report", { method: "POST", body: out.join("\\n") }));
}
</script>
"""


class Server(http.server.SimpleHTTPRequestHandler):
    # (Windows' registry may say text/plain for .mjs, and a module script needs a JavaScript type)
    extensions_map = {".mjs": "text/javascript", ".js": "text/javascript"}
    protocol_version = "HTTP/1.1"
    report = None
    results = None
    arrived = threading.Event()

    def log_message(self, *args):
        pass

    def fill(self, path):
        """Puts the file in the mirror if it is not there. False when the site has no such file."""
        local = Path(self.directory) / urllib.parse.unquote(path).lstrip("/")
        if local.exists() or ".." in path:
            return True
        request = urllib.request.Request(SITE + path, headers={"User-Agent": "speedometer-mirror"})
        try:
            with urllib.request.urlopen(request, timeout=60) as r:
                body = r.read()
        except urllib.error.HTTPError:
            return False
        local.parent.mkdir(parents=True, exist_ok=True)
        part = local.with_name(f"{local.name}.{threading.get_ident()}.part")
        part.write_bytes(body)
        os.replace(part, local)
        return True

    def do_GET(self):
        path = urllib.parse.urlsplit(self.path).path
        if path.endswith("/"):
            path += "index.html"
        if not self.fill(path):
            return self.send_error(404)
        if path != "/index.html":
            self.path = path
            return super().do_GET()
        body = (Path(self.directory) / "index.html").read_bytes().replace(b"</head>", REPORTER + b"</head>", 1)
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        self.send_response(204)
        self.send_header("Content-Length", "0")
        self.end_headers()
        if urllib.parse.urlsplit(self.path).path == "/__results":
            Server.results = body.decode("utf-8")
            return
        Server.report = body.decode("utf-8")
        Server.arrived.set()


class Quiet(http.server.ThreadingHTTPServer):
    def handle_error(self, request, client_address):
        pass  # (a window closing resets its connections)


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


def old_names(env):
    """Each SNOWLARK_<name> also as OSMIUM_<name>: a build from before the rename, given for comparison, reads
    only the old name (its window must not take the foreground either)."""
    for k in [k for k in env if k.startswith("SNOWLARK_")]:
        env["OSMIUM_" + k[len("SNOWLARK_"):]] = env[k]
    return env


def run_once(exe, folder, url, timeout, extra_env):
    """One window until the runner reports. Returns the scores by iteration, or None."""
    profile = folder / "profile"
    shutil.rmtree(folder, ignore_errors=True)
    profile.mkdir(parents=True)
    (profile / "window.json").write_text(
        json.dumps({"x": X, "y": Y, "width": W, "height": H, "maximized": False, "dpi": 96})
    )
    env = {k: v for k, v in os.environ.items() if not k.startswith(("SNOWLARK_", "OSMIUM_"))}
    env["SNOWLARK_PROFILE_DIR"] = str(profile)
    env["SNOWLARK_NO_ACTIVATE"] = "1"
    for k, v in extra_env:
        env[k] = v.replace("{run}", str(folder))
    old_names(env)
    info = subprocess.STARTUPINFO()
    info.dwFlags = subprocess.STARTF_USESHOWWINDOW
    info.wShowWindow = SW_SHOWNOACTIVATE
    Server.report = None
    Server.results = None
    Server.arrived.clear()
    with open(folder / "stderr.txt", "wb") as err:
        p = subprocess.Popen(
            [str(exe), url],
            env=env,
            startupinfo=info,
            creationflags=subprocess.CREATE_NO_WINDOW,
            stdout=subprocess.DEVNULL,
            stderr=err,
        )
        hwnd = None
        try:
            end = time.time() + 30
            while hwnd is None and time.time() < end:
                hwnd = window_of(p.pid)
                time.sleep(0.05)
            if hwnd is None:
                raise RuntimeError("no window")
            end = time.time() + timeout
            while not Server.arrived.wait(1):
                if user32.GetForegroundWindow() == hwnd:
                    raise RuntimeError("the test window took the foreground")
                if p.poll() is not None:
                    print(f"  exited with {p.returncode} before a score")
                    return None
                if time.time() > end:
                    print("  timed out")
                    return None
            user32.PostMessageW(hwnd, WM_CLOSE, 0, 0)
            try:
                p.wait(20)
            except subprocess.TimeoutExpired:
                pass
        finally:
            if p.poll() is None:
                p.kill()  # (this child, by its handle)
                p.wait(10)
    shutil.rmtree(profile, ignore_errors=True)
    (folder / "report.txt").write_text(Server.report, encoding="utf-8")
    if Server.results:
        (folder / "steps.json").write_text(Server.results, encoding="utf-8")
    return scores_in(Server.report)


def scores_in(report):
    return [float(x) for x in re.findall(r"[\d.]+", report.split("\n")[0].split(":")[1])]


def suites_in(report):
    out = {}
    for line in report.split("\n")[3:]:
        m = re.match(r"(\S+)\s+((?:[\d.]+\s+)+)x", line)
        if m:
            out[m.group(1)] = [float(x) for x in m.group(2).split()]
    return out


def summary(arms, out, rounds):
    runs = {}
    for name, _ in arms:
        runs[name] = [
            (out / f"{name}-{r}" / "report.txt").read_text(encoding="utf-8")
            for r in range(1, rounds + 1)
            if (out / f"{name}-{r}" / "report.txt").exists()
        ]
    lines = []
    means = {}
    for name, _ in arms:
        per_run = [st.mean(scores_in(t)) for t in runs[name]]
        if not per_run:
            lines.append(f"{name:24} no runs")
            continue
        means[name] = st.mean(per_run)
        spread = st.stdev(per_run) if len(per_run) > 1 else 0.0
        shown = " ".join(f"{v:.2f}" for v in per_run)
        lines.append(f"{name:24} mean {means[name]:.2f}  sd {spread:.2f}  runs {shown}")
    base = arms[0][0]
    for name, _ in arms[1:]:
        if base not in means or name not in means:
            continue
        lines.append("")
        lines.append(f"{name} against {base}: {(means[name] / means[base] - 1) * 100:+.1f}%")
        a = [suites_in(t) for t in runs[base]]
        b = [suites_in(t) for t in runs[name]]
        rows = []
        for suite in a[0]:
            x = st.mean(v for s in a for v in s[suite])
            y = st.mean(v for s in b for v in s[suite])
            rows.append((y / x - 1, suite, x, y))
        for c, suite, x, y in sorted(rows):
            lines.append(f"  {suite:46} {x:8.1f} {y:8.1f} {c * 100:+6.1f}%")
    text = "\n".join(lines)
    (out / "summary.txt").write_text(text + "\n", encoding="utf-8")
    print(text)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("arms", nargs="+", help="NAME=path to snowlark-cli.exe; the first is the base")
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--mirror", default=devenv.speedometer_dir(), type=Path)
    ap.add_argument("--rounds", type=int, default=4)
    ap.add_argument("--iterations", type=int, default=5)
    ap.add_argument("--timeout", type=int, default=900, help="seconds for one run")
    ap.add_argument("--suites", help="the runner's own `suites` parameter (names as in its results, comma-separated)")
    ap.add_argument("--env", action="append", default=[], metavar="SNOWLARK_X=V",
                    help="an environment variable for every window ({run} is the run's folder); repeatable")
    ap.add_argument("--arm-env", action="append", default=[], metavar="NAME:SNOWLARK_X=V",
                    help="an environment variable for arm NAME's windows only; repeatable")
    a = ap.parse_args()
    arms = [tuple(x.split("=", 1)) for x in a.arms]
    a.out.mkdir(parents=True, exist_ok=True)

    a.mirror.mkdir(parents=True, exist_ok=True)
    handler = functools.partial(Server, directory=str(a.mirror))
    server = Quiet(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://localhost:{server.server_address[1]}/index.html?iterationCount={a.iterations}&startAutomatically="
    if a.suites:
        url += "&suites=" + urllib.parse.quote(a.suites, safe=",")
    extra_env = [tuple(x.split("=", 1)) for x in a.env]
    arm_env = {}
    for x in a.arm_env:
        name, kv = x.split(":", 1)
        arm_env.setdefault(name, []).append(tuple(kv.split("=", 1)))

    for r in range(1, a.rounds + 1):
        shift = (r - 1) % len(arms)
        for name, exe in arms[shift:] + arms[:shift]:
            started = time.time()
            env = extra_env + arm_env.get(name, [])
            scores = run_once(exe, a.out / f"{name}-{r}", url, a.timeout, env)
            took = time.time() - started
            shown = " ".join(f"{s:.2f}" for s in scores) if scores else "no score"
            print(f"round {r} {name}: {shown}  ({took:.0f} s)", flush=True)
            time.sleep(5)
    print()
    summary(arms, a.out, a.rounds)


if __name__ == "__main__":
    main()
