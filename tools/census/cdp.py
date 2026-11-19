# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Headless Chrome over the DevTools protocol, with nothing but the standard library.

`Chrome` starts chrome.exe with a temporary profile and `--remote-debugging-port=0`, reads the port
Chrome picked from the profile's DevToolsActivePort file, and talks to the browser endpoint through a
minimal RFC 6455 client (`WebSocket`). Pages are driven as flat sessions (`Target.attachToTarget`
with `flatten`), so one socket carries the browser, its pages and their workers.
"""

from __future__ import annotations

import base64
import collections
import json
import os
import shutil
import socket
import struct
import subprocess
import tempfile
import time
from urllib.parse import urlparse

DEFAULT_CHROME = r"C:\Program Files\Google\Chrome\Application\chrome.exe"

# The user agent of desktop Chrome, without "HeadlessChrome" (some sites serve headless clients a
# different page or none). The major version is filled in from the running browser.
DESKTOP_UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) "
    "Chrome/{major}.0.0.0 Safari/537.36"
)


class WebSocketClosed(Exception):
    pass


class WebSocket:
    """A client WebSocket: text frames out (masked), text/binary frames in, pings answered."""

    def __init__(self, url: str, timeout: float = 60.0):
        u = urlparse(url)
        if u.scheme != "ws":
            raise ValueError(f"only ws:// is supported: {url}")
        self.sock = socket.create_connection((u.hostname, u.port or 80), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        path = u.path + (f"?{u.query}" if u.query else "")
        request = (
            f"GET {path} HTTP/1.1\r\nHost: {u.hostname}:{u.port}\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(request.encode())
        self.buf = b""
        while b"\r\n\r\n" not in self.buf:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise WebSocketClosed("handshake: connection closed")
            self.buf += chunk
        head, self.buf = self.buf.split(b"\r\n\r\n", 1)
        status = head.split(b"\r\n", 1)[0]
        if b" 101 " not in status + b" ":
            raise WebSocketClosed(f"handshake refused: {status!r}")

    def _read(self, n: int) -> bytes:
        while len(self.buf) < n:
            chunk = self.sock.recv(max(65536, n - len(self.buf)))
            if not chunk:
                raise WebSocketClosed("connection closed")
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def _send_frame(self, opcode: int, payload: bytes) -> None:
        self.sock.sendall(encode_frame(opcode, payload, os.urandom(4)))

    def send(self, text: str) -> None:
        self._send_frame(0x1, text.encode("utf-8"))

    def recv(self) -> str:
        """The next text or binary message (fragments joined), answering pings on the way."""
        message = b""
        while True:
            b0, b1 = self._read(2)
            fin, opcode = b0 & 0x80, b0 & 0x0F
            length = b1 & 0x7F
            if length == 126:
                (length,) = struct.unpack(">H", self._read(2))
            elif length == 127:
                (length,) = struct.unpack(">Q", self._read(8))
            mask = self._read(4) if b1 & 0x80 else None
            payload = self._read(length)
            if mask:
                payload = bytes(c ^ mask[i % 4] for i, c in enumerate(payload))
            if opcode == 0x8:
                raise WebSocketClosed("closed by peer")
            if opcode == 0x9:
                self._send_frame(0xA, payload)
                continue
            if opcode == 0xA:
                continue
            message += payload
            if fin:
                return message.decode("utf-8", "replace")

    def settimeout(self, t: float | None) -> None:
        self.sock.settimeout(t)

    def close(self) -> None:
        try:
            self._send_frame(0x8, b"")
        except OSError:
            pass
        self.sock.close()


def encode_frame(opcode: int, payload: bytes, mask: bytes) -> bytes:
    """One final client frame (masked, as RFC 6455 §5.3 requires of clients)."""
    head = bytes([0x80 | opcode])
    n = len(payload)
    if n < 126:
        head += bytes([0x80 | n])
    elif n < 65536:
        head += bytes([0x80 | 126]) + struct.pack(">H", n)
    else:
        head += bytes([0x80 | 127]) + struct.pack(">Q", n)
    masked = bytes(c ^ mask[i % 4] for i, c in enumerate(payload))
    return head + mask + masked


class CdpError(Exception):
    pass


class Chrome:
    """A headless Chrome process and its DevTools connection. Use as a context manager: the process
    (only the one this object started, by PID) is stopped and its profile removed on exit."""

    def __init__(self, exe: str = DEFAULT_CHROME, extra_args: list[str] | None = None):
        self.exe = exe
        self.profile = tempfile.mkdtemp(prefix="census-chrome-")
        self.extra_args = extra_args or []
        self.proc: subprocess.Popen | None = None
        self.ws: WebSocket | None = None
        self.next_id = 0
        self.events: collections.deque = collections.deque()
        self.major = "0"

    def __enter__(self) -> "Chrome":
        self.start()
        return self

    def __exit__(self, *exc) -> None:
        self.stop()

    def start(self) -> None:
        args = [
            self.exe,
            "--headless=new",
            "--remote-debugging-port=0",
            f"--user-data-dir={self.profile}",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--disable-background-networking",
            "--disable-sync",
            "--mute-audio",
            "--window-size=1280,800",
            *self.extra_args,
            "about:blank",
        ]
        self.proc = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        port_file = os.path.join(self.profile, "DevToolsActivePort")
        deadline = time.time() + 30
        while time.time() < deadline:
            if os.path.exists(port_file):
                lines = open(port_file, encoding="ascii").read().split()
                if len(lines) >= 2:
                    self.ws = WebSocket(f"ws://127.0.0.1:{lines[0]}{lines[1]}")
                    break
            time.sleep(0.1)
        if self.ws is None:
            self.stop()
            raise CdpError("Chrome did not open its DevTools port")
        product = self.call("Browser.getVersion").get("product", "")
        self.major = product.split("/")[-1].split(".")[0] or "0"

    def stop(self) -> None:
        if self.ws:
            try:
                self.call("Browser.close", timeout=5)
            except Exception:
                pass
            self.ws.close()
            self.ws = None
        if self.proc:
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                # Only the process this object started, by PID (and its children with /T).
                subprocess.run(
                    ["taskkill", "/PID", str(self.proc.pid), "/T", "/F"],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
            self.proc = None
        shutil.rmtree(self.profile, ignore_errors=True)

    @property
    def user_agent(self) -> str:
        return DESKTOP_UA.format(major=self.major)

    def send(self, method: str, params: dict | None = None, session: str | None = None) -> int:
        self.next_id += 1
        msg = {"id": self.next_id, "method": method, "params": params or {}}
        if session:
            msg["sessionId"] = session
        self.ws.send(json.dumps(msg))
        return self.next_id

    def call(
        self, method: str, params: dict | None = None, session: str | None = None, timeout: float = 60
    ) -> dict:
        """Sends a command and waits for its result; events that arrive meanwhile are queued."""
        mid = self.send(method, params, session)
        deadline = time.time() + timeout
        while True:
            left = deadline - time.time()
            if left <= 0:
                raise CdpError(f"{method}: no reply in {timeout} s")
            self.ws.settimeout(left)
            try:
                msg = json.loads(self.ws.recv())
            except socket.timeout:
                raise CdpError(f"{method}: no reply in {timeout} s") from None
            if msg.get("id") == mid:
                if "error" in msg:
                    raise CdpError(f"{method}: {msg['error'].get('message')}")
                return msg.get("result", {})
            if "method" in msg:
                self.events.append(msg)

    def next_event(self, timeout: float) -> dict | None:
        """The next queued or arriving event, or None after `timeout` seconds."""
        if self.events:
            return self.events.popleft()
        self.ws.settimeout(max(timeout, 0.01))
        try:
            while True:
                msg = json.loads(self.ws.recv())
                if "method" in msg:
                    return msg
        except socket.timeout:
            return None

    def new_page(self) -> tuple[str, str]:
        """A new tab: (targetId, sessionId)."""
        target = self.call("Target.createTarget", {"url": "about:blank"})["targetId"]
        session = self.call("Target.attachToTarget", {"targetId": target, "flatten": True})[
            "sessionId"
        ]
        return target, session

    def close_page(self, target: str) -> None:
        try:
            self.call("Target.closeTarget", {"targetId": target}, timeout=10)
        except CdpError:
            pass
        # Events of the closed page are of no further use.
        self.events.clear()

    def evaluate(self, session: str, expression: str, timeout: float = 60) -> object:
        """`expression`'s value (promises awaited), returned by value."""
        r = self.call(
            "Runtime.evaluate",
            {"expression": expression, "awaitPromise": True, "returnByValue": True},
            session,
            timeout=timeout,
        )
        if "exceptionDetails" in r:
            raise CdpError(f"evaluate: {r['exceptionDetails'].get('text')}")
        return r.get("result", {}).get("value")
