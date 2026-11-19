# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Headless Chrome over the DevTools protocol, standard library only.

A minimal WebSocket client (RFC 6455: text frames, fragmentation, ping/pong, close) is enough for CDP:
the browser endpoint comes from the DevToolsActivePort file Chrome writes into its profile when started
with --remote-debugging-port=0 (so the port is always free), and pages are driven through flattened
target sessions on that one connection.
"""

import base64
import json
import os
import shutil
import socket
import struct
import subprocess
import tempfile
import time
import urllib.parse

CHROME = r"C:\Program Files\Google\Chrome\Application\chrome.exe"


class WebSocket:
    def __init__(self, url, timeout=60.0):
        u = urllib.parse.urlparse(url)
        self.sock = socket.create_connection((u.hostname, u.port or 80), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        path = u.path + ("?" + u.query if u.query else "")
        request = (
            f"GET {path} HTTP/1.1\r\nHost: {u.hostname}:{u.port}\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(request.encode())
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise ConnectionError("websocket handshake: connection closed")
            head += chunk
        head, self.buffer = head.split(b"\r\n\r\n", 1)
        self.partial = b""
        if b" 101 " not in head.split(b"\r\n", 1)[0]:
            raise ConnectionError("websocket handshake failed: " + head.decode(errors="replace"))

    def _fill(self, n):
        # Reads until the buffer holds n bytes; a timeout leaves what was read in the buffer.
        while len(self.buffer) < n:
            chunk = self.sock.recv(max(65536, n - len(self.buffer)))
            if not chunk:
                raise ConnectionError("websocket closed")
            self.buffer += chunk

    def _frame(self):
        """The next whole frame as (fin, opcode, payload); consumed only once complete."""
        self._fill(2)
        b0, b1 = self.buffer[0], self.buffer[1]
        n = b1 & 0x7F
        at = 2
        if n == 126:
            self._fill(4)
            n = struct.unpack(">H", self.buffer[2:4])[0]
            at = 4
        elif n == 127:
            self._fill(10)
            n = struct.unpack(">Q", self.buffer[2:10])[0]
            at = 10
        mask = None
        if b1 & 0x80:
            self._fill(at + 4)
            mask = self.buffer[at : at + 4]
            at += 4
        self._fill(at + n)
        payload = self.buffer[at : at + n]
        self.buffer = self.buffer[at + n :]
        if mask:
            payload = _mask(payload, mask)
        return bool(b0 & 0x80), b0 & 0x0F, payload

    def _send_frame(self, opcode, payload):
        header = bytes([0x80 | opcode])
        n = len(payload)
        if n < 126:
            header += bytes([0x80 | n])
        elif n < 65536:
            header += bytes([0x80 | 126]) + struct.pack(">H", n)
        else:
            header += bytes([0x80 | 127]) + struct.pack(">Q", n)
        mask = os.urandom(4)
        self.sock.sendall(header + mask + _mask(payload, mask))

    def send(self, text):
        self._send_frame(0x1, text.encode())

    def recv(self, timeout=None):
        self.sock.settimeout(timeout)
        while True:
            fin, opcode, payload = self._frame()
            if opcode == 0x9:
                self._send_frame(0xA, payload)
                continue
            if opcode == 0xA:
                continue
            if opcode == 0x8:
                raise ConnectionError("websocket closed by peer")
            self.partial += payload
            if fin:
                message, self.partial = self.partial, b""
                return message.decode("utf-8", errors="replace")

    def close(self):
        try:
            self._send_frame(0x8, b"")
        except OSError:
            pass
        self.sock.close()


def _mask(payload, mask):
    # XOR with the repeated 4-byte key, through integers (fast for large payloads).
    n = len(payload)
    key = (mask * (n // 4 + 1))[:n]
    return (int.from_bytes(payload, "little") ^ int.from_bytes(key, "little")).to_bytes(n, "little")


class CDPError(Exception):
    pass


class Chrome:
    """One headless Chrome process with a temporary profile; `close()` ends it (by its PID)."""

    def __init__(self, width=1280, height=800, user_agent=None, chrome=CHROME, lang="en-US"):
        self.profile = tempfile.mkdtemp(prefix="layout-diff-chrome-")
        args = [
            chrome,
            "--headless=new",
            "--remote-debugging-port=0",
            f"--user-data-dir={self.profile}",
            f"--window-size={width},{height}",
            "--hide-scrollbars",
            "--force-device-scale-factor=1",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--disable-component-update",
            "--disable-background-networking",
            "--disable-sync",
            "--mute-audio",
            f"--lang={lang}",
            "about:blank",
        ]
        if user_agent:
            args.insert(-1, f"--user-agent={user_agent}")
        self.proc = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.next_id = 0
        self.events = []
        port_file = os.path.join(self.profile, "DevToolsActivePort")
        deadline = time.time() + 30
        while True:
            try:
                with open(port_file) as f:
                    lines = f.read().split()
                if len(lines) >= 2:
                    break
            except OSError:
                pass
            if time.time() > deadline or self.proc.poll() is not None:
                self.close()
                raise RuntimeError("Chrome did not start (no DevToolsActivePort)")
            time.sleep(0.05)
        self.ws = WebSocket(f"ws://127.0.0.1:{lines[0]}{lines[1]}")

    def call(self, method, params=None, session=None, timeout=60.0):
        self.next_id += 1
        mid = self.next_id
        msg = {"id": mid, "method": method, "params": params or {}}
        if session:
            msg["sessionId"] = session
        self.ws.send(json.dumps(msg))
        deadline = time.time() + timeout
        while True:
            left = deadline - time.time()
            if left <= 0:
                raise CDPError(f"{method}: timeout")
            reply = json.loads(self.ws.recv(left))
            if reply.get("id") == mid:
                if "error" in reply:
                    raise CDPError(f"{method}: {reply['error'].get('message')}")
                return reply.get("result", {})
            if "method" in reply:
                self.events.append(reply)

    def wait_event(self, name, session=None, timeout=30.0):
        """The first event `name` (queued or arriving within `timeout`), or None."""
        deadline = time.time() + timeout
        while True:
            for i, e in enumerate(self.events):
                if e["method"] == name and (session is None or e.get("sessionId") == session):
                    return self.events.pop(i)
            left = deadline - time.time()
            if left <= 0:
                return None
            try:
                reply = json.loads(self.ws.recv(left))
            except (socket.timeout, TimeoutError):
                return None
            if "method" in reply:
                self.events.append(reply)

    def drain(self, seconds):
        """Keeps reading events for `seconds` (the page keeps running meanwhile)."""
        self.wait_event("__none__", timeout=seconds)
        self.events.clear()

    def new_page(self, width, height, user_agent=None, color_scheme="light"):
        target = self.call("Target.createTarget", {"url": "about:blank"})["targetId"]
        session = self.call("Target.attachToTarget", {"targetId": target, "flatten": True})["sessionId"]
        self.call("Page.enable", session=session)
        self.call(
            "Emulation.setDeviceMetricsOverride",
            {"width": width, "height": height, "deviceScaleFactor": 1, "mobile": False},
            session=session,
        )
        # The OS theme would otherwise decide (Snowlark's --render reports light).
        self.call(
            "Emulation.setEmulatedMedia",
            {"features": [{"name": "prefers-color-scheme", "value": color_scheme}]},
            session=session,
        )
        if user_agent:
            self.call(
                "Emulation.setUserAgentOverride",
                {"userAgent": user_agent, "acceptLanguage": "en-US,en;q=0.9"},
                session=session,
            )
        return session

    def navigate(self, session, url, timeout=45.0):
        """Navigates and waits for the load event; returns whether it fired within `timeout`."""
        self.events.clear()
        result = self.call("Page.navigate", {"url": url}, session=session, timeout=timeout)
        if result.get("errorText"):
            raise CDPError(f"navigate {url}: {result['errorText']}")
        return self.wait_event("Page.loadEventFired", session=session, timeout=timeout) is not None

    def evaluate(self, session, expression, timeout=60.0):
        result = self.call(
            "Runtime.evaluate",
            {"expression": expression, "returnByValue": True},
            session=session,
            timeout=timeout,
        )
        if "exceptionDetails" in result:
            d = result["exceptionDetails"]
            raise CDPError("evaluate threw: " + str(d.get("exception", {}).get("description", d.get("text"))))
        return result["result"].get("value")

    def screenshot(self, session, width, height, path):
        data = self.call(
            "Page.captureScreenshot",
            {
                "format": "png",
                "captureBeyondViewport": True,
                "clip": {"x": 0, "y": 0, "width": width, "height": height, "scale": 1},
            },
            session=session,
        )["data"]
        with open(path, "wb") as f:
            f.write(base64.b64decode(data))

    def close(self):
        ws = getattr(self, "ws", None)
        if ws:
            try:
                self.call("Browser.close", timeout=5)
            except Exception:
                pass
            ws.close()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            # This Chrome and its children, by its PID (killing the browser process alone can orphan
            # its GPU, utility and crashpad processes).
            subprocess.run(["taskkill", "/PID", str(self.proc.pid), "/T", "/F"], capture_output=True)
            try:
                self.proc.wait(timeout=20)
            except subprocess.TimeoutExpired:
                pass
        for _ in range(20):
            shutil.rmtree(self.profile, ignore_errors=True)
            if not os.path.exists(self.profile):
                break
            time.sleep(0.25)
