# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Stage 1: the web-exposed surface of Chrome and Snowlark, and its diff.

Both engines load the same page (`surface.html`) from a local HTTP server on 127.0.0.1, which is a
secure context as the sites are; the page enumerates its window and a dedicated worker with
`surface.js` and POSTs the result back to the server.
"""

from __future__ import annotations

import fnmatch
import http.server
import json
import os
import subprocess
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent

# ECMAScript globals (and WebAssembly, V8's own): their gaps are V8 version differences, reported
# apart from the web platform's.
ECMASCRIPT_GLOBALS = {
    "Object", "Function", "Array", "Number", "parseFloat", "parseInt", "Infinity", "NaN", "undefined",
    "Boolean", "String", "Symbol", "Date", "Promise", "RegExp", "Error", "AggregateError", "EvalError",
    "RangeError", "ReferenceError", "SyntaxError", "TypeError", "URIError", "SuppressedError",
    "globalThis", "JSON", "Math", "Intl", "ArrayBuffer", "SharedArrayBuffer", "Atomics", "Uint8Array",
    "Int8Array", "Uint16Array", "Int16Array", "Uint32Array", "Int32Array", "Float16Array",
    "Float32Array", "Float64Array", "Uint8ClampedArray", "BigUint64Array", "BigInt64Array", "DataView",
    "Map", "BigInt", "Set", "WeakMap", "WeakSet", "Proxy", "Reflect", "FinalizationRegistry", "WeakRef",
    "decodeURI", "decodeURIComponent", "encodeURI", "encodeURIComponent", "escape", "unescape", "eval",
    "isFinite", "isNaN", "Iterator", "DisposableStack", "AsyncDisposableStack", "Temporal",
    "WebAssembly",
}


def head(key: str) -> str:
    """The global name a key belongs to: "ReadableStream.prototype.read" -> "ReadableStream"."""
    return key.split(".", 1)[0]


def is_ecmascript(key: str) -> bool:
    h = head(key)
    if h in ("window", "self"):
        return key.split(".", 1)[1] in ECMASCRIPT_GLOBALS
    return h in ECMASCRIPT_GLOBALS


def member_name(key: str) -> str:
    """The last segment: what a script writes after the dot."""
    return key.rsplit(".", 1)[-1]


# ----- collecting ----------------------------------------------------------------------------------


class _Results:
    def __init__(self):
        self.lock = threading.Lock()
        self.by_engine: dict[str, dict] = {}
        self.event = threading.Condition(self.lock)


def _handler(results: _Results):
    surface_js = (HERE / "surface.js").read_bytes()
    files = {
        "/surface.html": ((HERE / "surface.html").read_bytes(), "text/html; charset=utf-8"),
        "/surface.js": (surface_js, "text/javascript"),
        "/surface-worker.js": (
            surface_js + b"\npostMessage(JSON.stringify(censusSurface()));\n",
            "text/javascript",
        ),
    }

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            path = self.path.split("?", 1)[0]
            if path not in files:
                self.send_error(404)
                return
            body, ctype = files[path]
            self.send_response(200)
            self.send_header("content-type", ctype)
            self.send_header("content-length", str(len(body)))
            self.send_header("cache-control", "no-store")
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            if not self.path.startswith("/result"):
                self.send_error(404)
                return
            engine = self.path.split("engine=", 1)[-1].split("&")[0] or "unknown"
            body = self.rfile.read(int(self.headers.get("content-length", "0")))
            with results.lock:
                results.by_engine[engine] = json.loads(body)
                results.event.notify_all()
            self.send_response(204)
            self.end_headers()

    return Handler


class SurfaceServer:
    """The local server for surface.html (on a free port, stopped on exit)."""

    def __enter__(self):
        self.results = _Results()
        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _handler(self.results))
        self.port = self.httpd.server_address[1]
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()
        return self

    def __exit__(self, *exc):
        self.httpd.shutdown()
        self.httpd.server_close()

    def url(self, engine: str) -> str:
        return f"http://127.0.0.1:{self.port}/surface.html?engine={engine}"

    def wait(self, engine: str, timeout: float) -> dict | None:
        deadline = time.time() + timeout
        with self.results.lock:
            while engine not in self.results.by_engine:
                left = deadline - time.time()
                if left <= 0:
                    return None
                self.results.event.wait(left)
            return self.results.by_engine[engine]


def chrome_surface(chrome, server: SurfaceServer) -> dict:
    target, session = chrome.new_page()
    try:
        chrome.call("Page.navigate", {"url": server.url("chrome")}, session)
        result = server.wait("chrome", 60)
    finally:
        chrome.close_page(target)
    if result is None:
        raise RuntimeError("Chrome did not post its surface")
    return result


def snowlark_surface(snowlark: str, server: SurfaceServer, workdir: Path) -> dict:
    png = workdir / "surface-snowlark.png"
    log = workdir / "surface-snowlark.log"
    env = dict(os.environ, SNOWLARK_CONSOLE="tagged", SNOWLARK_RENDER_TIMEOUT_MS="60000")
    env.pop("SNOWLARK_BLOCK", None)
    with open(log, "wb") as out:
        proc = subprocess.Popen(
            [snowlark, "--render", server.url("snowlark"), str(png)],
            stdout=out,
            stderr=subprocess.STDOUT,
            env=env,
        )
        result = server.wait("snowlark", 90)
        try:
            proc.wait(timeout=90)
        except subprocess.TimeoutExpired:
            _kill(proc)
    if result is None:
        raise RuntimeError(f"Snowlark did not post its surface (see {log})")
    return result


def _kill(proc: subprocess.Popen) -> None:
    """Stops a process this tool started, by PID (with its children)."""
    subprocess.run(
        ["taskkill", "/PID", str(proc.pid), "/T", "/F"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        pass


# ----- diffing -------------------------------------------------------------------------------------


def load_ignore(path: Path) -> list[str]:
    """fnmatch patterns over keys, one per line; '#' starts a comment."""
    patterns = []
    if path.exists():
        for line in path.read_text(encoding="utf-8").splitlines():
            line = line.split("#", 1)[0].strip()
            if line:
                patterns.append(line)
    return patterns


def ignored(key: str, patterns: list[str]) -> bool:
    return any(fnmatch.fnmatchcase(key, p) for p in patterns)


def diff_members(chrome: dict, snowlark: dict, patterns: list[str]) -> dict:
    """Members Chrome has and Snowlark lacks (`missing`; a missing interface or namespace stands for
    all its members, counted in `members_of`), members only Snowlark has (`extra`, collapsed the same
    way), members whose kind differs (`kind`), and getters whose value is an object or function in
    Chrome but undefined or null in Snowlark (`stubs`)."""
    cm, om = chrome["members"], snowlark["members"]

    def collapse(a: dict, b: dict) -> tuple[list[str], dict[str, int]]:
        absent_heads = {k for k, v in a.items() if v in ("interface", "namespace") and k not in b}
        keys, members_of = [], {}
        for k in sorted(a):
            if k in b or ignored(k, patterns):
                continue
            h = head(k)
            if k != h and h in absent_heads:
                members_of[h] = members_of.get(h, 0) + 1
                continue
            keys.append(k)
        return keys, members_of

    missing, missing_members = collapse(cm, om)
    extra, extra_members = collapse(om, cm)
    kind = sorted(
        (k, cm[k], om[k]) for k in cm if k in om and cm[k] != om[k] and not ignored(k, patterns)
    )
    cv, ov = chrome.get("values", {}), snowlark.get("values", {})
    stubs = sorted(
        (k, cv[k], ov[k])
        for k in cv
        if k in ov
        and cv[k] in ("object", "function")
        and ov[k] in ("undefined", "null")
        and not ignored(k, patterns)
    )
    return {
        "missing": missing,
        "missing_members_of": missing_members,
        "extra": extra,
        "extra_members_of": extra_members,
        "kind": kind,
        "stubs": stubs,
    }


def diff_surfaces(chrome: dict, snowlark: dict, patterns: list[str]) -> dict:
    """The window and worker diffs (a side that failed to enumerate gives no diff)."""
    out = {}
    for scope in ("window", "worker"):
        c, o = chrome.get(scope), snowlark.get(scope)
        if c and o:
            out[scope] = diff_members(c, o, patterns)
        else:
            out[scope] = None
    return out
