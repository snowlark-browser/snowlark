# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Speedometer 3.1 in headless Chrome, from the same mirror on disk the window runs use, optionally traced.

    python tools/vs-chrome/speedometer.py --out <dir> [--iterations 5] [--suites A,B] [--trace] [--runs 1]

Writes `<dir>/results-<n>.json` (Speedometer's metrics, as `snowlark-cli --speedometer` writes them: each suite's
`values` per iteration, and `Score`) and with `--trace` `<dir>/trace-<n>.json`: Chrome's trace of the run, with the
categories `split_chrome.py` reads (the timeline, V8, Blink's GC, user timing marks and V8's sampling profiler).
A traced run is slower than a plain one: read shares from it, times from plain runs.

Chrome gets a fresh profile in a temp folder each run, runs headless (no window, no focus) and is ended by its PID.
Only the standard library: the DevTools protocol is spoken over a small WebSocket client below.
"""
import argparse, base64, functools, http.server, json, os, shutil, socket, struct, subprocess, sys
import tempfile, threading, time, urllib.parse, urllib.request
from pathlib import Path
import os as _os, sys as _sys  # noqa: E401
_sys.path.insert(0, _os.path.join(_os.path.dirname(_os.path.abspath(__file__)), ".."))
import devenv  # noqa: E402

CHROME = r"C:\Program Files\Google\Chrome\Application\chrome.exe"
MIRROR = str(devenv.speedometer_dir())
CATEGORIES = [
    "devtools.timeline", "disabled-by-default-devtools.timeline", "blink.user_timing", "v8", "v8.execute",
    "disabled-by-default-v8.gc", "blink_gc", "cppgc", "disabled-by-default-v8.cpu_profiler", "toplevel",
]


class WebSocket:
    """Text frames only, client side, as RFC 6455 asks (masked); enough for the DevTools protocol."""

    def __init__(self, url):
        u = urllib.parse.urlparse(url)
        self.sock = socket.create_connection((u.hostname, u.port))
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall((f"GET {u.path} HTTP/1.1\r\nHost: {u.hostname}:{u.port}\r\nUpgrade: websocket\r\n"
                           f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = self.sock.recv(1)
            if not chunk:
                raise ConnectionError("closed during the handshake")
            head += chunk
        if b" 101 " not in head.split(b"\r\n", 1)[0]:
            raise ConnectionError(head.decode(errors="replace"))

    def _read(self, n):
        buf = bytearray()
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise ConnectionError("closed")
            buf += chunk
        return bytes(buf)

    def _frame(self, opcode, payload):
        n = len(payload)
        head = bytes([0x80 | opcode])
        if n < 126:
            head += bytes([0x80 | n])
        elif n < 65536:
            head += bytes([0x80 | 126]) + struct.pack(">H", n)
        else:
            head += bytes([0x80 | 127]) + struct.pack(">Q", n)
        mask = os.urandom(4)
        self.sock.sendall(head + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def send(self, text):
        self._frame(1, text.encode())

    def recv(self):
        message = bytearray()
        while True:
            b0, b1 = self._read(2)
            n = b1 & 0x7F
            if n == 126:
                n = struct.unpack(">H", self._read(2))[0]
            elif n == 127:
                n = struct.unpack(">Q", self._read(8))[0]
            mask = self._read(4) if b1 & 0x80 else None
            payload = self._read(n)
            if mask:
                payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
            opcode = b0 & 0x0F
            if opcode == 8:
                raise ConnectionError("closed by Chrome")
            if opcode == 9:
                self._frame(10, payload)
                continue
            if opcode == 10:
                continue
            message += payload
            if b0 & 0x80:
                return message.decode()


class DevTools:
    def __init__(self, url):
        self.ws = WebSocket(url)
        self.next_id = 0
        self.events = []

    def call(self, method, params=None, session=None, timeout=60):
        self.next_id += 1
        msg = {"id": self.next_id, "method": method, "params": params or {}}
        if session:
            msg["sessionId"] = session
        self.ws.send(json.dumps(msg))
        self.ws.sock.settimeout(timeout)
        while True:
            reply = json.loads(self.ws.recv())
            if reply.get("id") == self.next_id:
                if "error" in reply:
                    raise RuntimeError(f"{method}: {reply['error']}")
                return reply.get("result", {})
            self.events.append(reply)

    def wait_event(self, method, timeout=600):
        self.ws.sock.settimeout(timeout)
        while True:
            for i, e in enumerate(self.events):
                if e.get("method") == method:
                    return self.events.pop(i)
            self.events.append(json.loads(self.ws.recv()))


def serve(mirror):
    handler = functools.partial(Quiet, directory=str(mirror))
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


class Quiet(http.server.SimpleHTTPRequestHandler):
    # (Windows' registry may say text/plain for .mjs, and a module script needs a JavaScript type)
    extensions_map = {".mjs": "text/javascript", ".js": "text/javascript"}

    def log_message(self, *args):
        pass

    def end_headers(self):
        self.send_header("Cache-Control", "no-store")
        super().end_headers()


def run_once(a, port, n):
    profile = Path(tempfile.mkdtemp(prefix="snowlark-vs-chrome-speedo-"))
    args = [a.chrome, "--headless=new", "--no-first-run", "--no-default-browser-check", f"--user-data-dir={profile}",
            "--remote-debugging-port=0", "--disable-background-timer-throttling", "--disable-renderer-backgrounding",
            "--disable-backgrounding-occluded-windows", "--disable-extensions", "--window-size=1280,1000", "about:blank"]
    chrome = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        port_file = profile / "DevToolsActivePort"
        for _ in range(200):
            if port_file.exists() and len(port_file.read_text().split()) >= 2:
                break
            time.sleep(0.05)
        dt_port, path = port_file.read_text().split()[:2]
        cdp = DevTools(f"ws://127.0.0.1:{dt_port}{path}")
        page = next(t for t in cdp.call("Target.getTargets")["targetInfos"] if t["type"] == "page")
        session = cdp.call("Target.attachToTarget", {"targetId": page["targetId"], "flatten": True})["sessionId"]
        if a.trace:
            cdp.call("Tracing.start", {"transferMode": "ReturnAsStream", "traceConfig": {
                "recordMode": "recordAsMuchAsPossible", "includedCategories": CATEGORIES}})
        url = f"http://127.0.0.1:{port}/index.html?startAutomatically&iterationCount={a.iterations}"
        if a.suites:
            url += "&suites=" + urllib.parse.quote(a.suites)
        cdp.call("Page.navigate", {"url": url}, session)
        started = time.time()
        while True:
            time.sleep(5)
            r = cdp.call("Runtime.evaluate", {"expression": "!!(globalThis.benchmarkClient && benchmarkClient._hasResults)",
                                              "returnByValue": True}, session)
            if r["result"].get("value"):
                break
            if time.time() - started > a.timeout:
                r = cdp.call("Runtime.evaluate", {"expression": "location.href + ' | ' + document.body.innerText.slice(0, 400)",
                                                  "returnByValue": True}, session)
                raise TimeoutError(f"no results after {a.timeout} s; the page: {r['result'].get('value')}")
        took = time.time() - started
        r = cdp.call("Runtime.evaluate", {"expression": "benchmarkClient._formattedJSONResult({modern: true})",
                                          "returnByValue": True}, session)
        results = json.loads(r["result"]["value"])
        (a.out / f"results-{n}.json").write_text(json.dumps(results, indent=1), encoding="utf-8")
        if a.trace:
            cdp.call("Tracing.end")
            stream = cdp.wait_event("Tracing.tracingComplete")["params"]["stream"]
            with open(a.out / f"trace-{n}.json", "wb") as f:
                while True:
                    chunk = cdp.call("IO.read", {"handle": stream, "size": 4 << 20})
                    data = chunk.get("data", "")
                    f.write(base64.b64decode(data) if chunk.get("base64Encoded") else data.encode())
                    if chunk.get("eof"):
                        break
            cdp.call("IO.close", {"handle": stream})
        score = results.get("Score", {}).get("mean")
        print(f"run {n}: score {score:.2f}, {took:.0f} s" if score else f"run {n}: no score ({took:.0f} s)", flush=True)
        try:
            cdp.call("Browser.close", timeout=5)
        except Exception:
            pass
        chrome.wait(timeout=10)
    finally:
        if chrome.poll() is None:
            chrome.kill()  # (our own child, by its handle)
        shutil.rmtree(profile, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--iterations", type=int, default=5)
    ap.add_argument("--suites", help="comma-separated suite names (Speedometer's `suites` parameter)")
    ap.add_argument("--trace", action="store_true")
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--timeout", type=int, default=1800)
    ap.add_argument("--chrome", default=CHROME)
    ap.add_argument("--mirror", default=MIRROR, type=Path)
    a = ap.parse_args()
    a.out.mkdir(parents=True, exist_ok=True)
    version = subprocess.run(["powershell", "-NoProfile", "-Command", f"(Get-Item '{a.chrome}').VersionInfo.ProductVersion"],
                             capture_output=True, text=True).stdout.strip()
    print(f"chrome {version}", flush=True)
    server = serve(a.mirror)
    try:
        for n in range(1, a.runs + 1):
            run_once(a, server.server_address[1], n)
    finally:
        server.shutdown()


if __name__ == "__main__":
    sys.exit(main())
