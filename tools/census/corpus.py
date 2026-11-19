# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Stage 2: the corpus's scripts as Chrome gets them, and which API names they reference.

Each site loads in a fresh headless Chrome tab (desktop user agent) until its load event and two
seconds without network activity, or `timeout`. Every script response (the page's, its workers' and
its out-of-process frames', through auto-attached sessions) and every HTML document is saved under
`<out>/sites/<slug>/`, with `index.json` listing them and the exceptions Chrome itself threw.

Usage is a crude token match over a site's scripts (inline scripts of its HTML documents included):
an interface or a global counts when its name appears as a free identifier (not after a dot, not an
object key) or as `window.name`; a member of an interface that scripts reach through a well-known
global (Navigator, Document, Performance...) when `navigator.name` or `"name" in navigator`
appears; a static or namespace member when `Name.member` appears; any other prototype member when
its name follows a dot or is a quoted string (generic names over-count: see `match_kind`). Good
enough to rank, not to prove.
"""

from __future__ import annotations

import base64
import json
import re
import time
from pathlib import Path
from urllib.parse import urlparse

from cdp import CdpError, Chrome
from surface import head, member_name

SCRIPT_TYPES = {"Script", "Document"}


def slug(url: str) -> str:
    u = urlparse(url)
    s = (u.hostname or "site").removeprefix("www.")
    path = u.path.strip("/").replace("/", "_")
    return re.sub(r"[^A-Za-z0-9._-]", "_", s + (("_" + path) if path else ""))


def read_corpus(path: Path) -> list[str]:
    urls = []
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            urls.append(line)
    return urls


def fetch_site(chrome: Chrome, url: str, outdir: Path, timeout: float = 45.0, quiet: float = 2.0):
    """Loads `url` in a new tab and saves its scripts and documents to `outdir`; returns the index."""
    outdir.mkdir(parents=True, exist_ok=True)
    target, session = chrome.new_page()
    started = time.time()
    index = {"url": url, "resources": [], "chrome_exceptions": [], "errors": []}
    sessions = {session}
    pending: dict[tuple[str, str], dict] = {}
    loaded = False
    last_activity = time.time()
    try:
        _enable(chrome, session)
        chrome.call("Emulation.setDeviceMetricsOverride",
                    {"width": 1280, "height": 800, "deviceScaleFactor": 1, "mobile": False}, session)
        chrome.call("Target.setAutoAttach", {"autoAttach": True, "waitForDebuggerOnStart": False,
                                             "flatten": True}, session)
        chrome.call("Page.navigate", {"url": url}, session)
        while time.time() - started < timeout:
            # Settled: loaded, then `quiet` seconds without network events (requests that never
            # finish, like long polls, do not hold it).
            if loaded and time.time() - last_activity >= quiet:
                break
            ev = chrome.next_event(0.25)
            if ev is None:
                continue
            method, params, sid = ev["method"], ev.get("params", {}), ev.get("sessionId")
            if sid not in sessions and method != "Target.attachedToTarget":
                continue
            if method == "Target.attachedToTarget":
                child = params["sessionId"]
                if params.get("targetInfo", {}).get("type") in ("worker", "iframe", "shared_worker"):
                    sessions.add(child)
                    try:
                        _enable(chrome, child)
                        if params["targetInfo"]["type"] == "iframe":
                            chrome.call("Target.setAutoAttach", {"autoAttach": True,
                                        "waitForDebuggerOnStart": False, "flatten": True}, child)
                    except CdpError as e:
                        index["errors"].append(f"attach: {e}")
            elif method == "Page.loadEventFired" and sid == session:
                loaded = True
                last_activity = time.time()
            elif method == "Network.requestWillBeSent":
                last_activity = time.time()
            elif method == "Network.responseReceived":
                if params.get("type") in SCRIPT_TYPES:
                    pending[(sid, params["requestId"])] = {
                        "url": params["response"]["url"],
                        "type": params["type"],
                        "status": params["response"].get("status"),
                    }
            elif method in ("Network.loadingFinished", "Network.loadingFailed"):
                key = (sid, params["requestId"])
                last_activity = time.time()
                info = pending.pop(key, None)
                if info and method == "Network.loadingFinished":
                    _save_body(chrome, sid, params["requestId"], info, outdir, index)
            elif method == "Runtime.exceptionThrown":
                d = params.get("exceptionDetails", {})
                text = (d.get("exception") or {}).get("description") or d.get("text", "")
                index["chrome_exceptions"].append(text.split("\n", 1)[0][:300])
        index["loaded"] = loaded
        try:
            index["final_url"] = chrome.evaluate(session, "location.href", timeout=10)
        except CdpError:
            index["final_url"] = None
        # The viewport as Chrome draws it, for comparing Snowlark's capture with.
        try:
            shot = chrome.call("Page.captureScreenshot", {"format": "png"}, session, timeout=30)
            (outdir / "chrome.png").write_bytes(base64.b64decode(shot["data"]))
        except CdpError as e:
            index["errors"].append(f"screenshot: {e}")
    except CdpError as e:
        index["errors"].append(str(e))
    finally:
        chrome.close_page(target)
    index["seconds"] = round(time.time() - started, 1)
    (outdir / "index.json").write_text(json.dumps(index, indent=1), encoding="utf-8")
    return index


def _enable(chrome: Chrome, session: str) -> None:
    chrome.call("Network.enable", {"maxTotalBufferSize": 200_000_000,
                                   "maxResourceBufferSize": 50_000_000}, session)
    chrome.call("Network.setUserAgentOverride", {"userAgent": chrome.user_agent}, session)
    try:
        chrome.call("Page.enable", {}, session)
    except CdpError:
        pass  # workers have no Page domain
    chrome.call("Runtime.enable", {}, session)
    try:
        chrome.call("Runtime.runIfWaitingForDebugger", {}, session)
    except CdpError:
        pass


def _save_body(chrome: Chrome, sid: str, request_id: str, info: dict, outdir: Path, index: dict):
    try:
        r = chrome.call("Network.getResponseBody", {"requestId": request_id}, sid, timeout=30)
    except CdpError as e:
        index["errors"].append(f"{info['url'][:120]}: {e}")
        return
    body = r.get("body", "")
    data = base64.b64decode(body) if r.get("base64Encoded") else body.encode("utf-8")
    n = len(index["resources"])
    ext = "html" if info["type"] == "Document" else "js"
    name = f"{n:03d}.{ext}"
    (outdir / name).write_bytes(data)
    index["resources"].append({**info, "file": name, "bytes": len(data)})


# ----- usage ---------------------------------------------------------------------------------------

IDENT = r"[A-Za-z_$][\w$]*"
_RE_IDENT = re.compile(IDENT)
_RE_DOTTED = re.compile(r"\.\s*(" + IDENT + ")")
_RE_QUOTED = re.compile(r"""["'`](""" + IDENT + r""")["'`]""")
# Identifiers start only where none continues (a match attempt inside a long identifier would make
# the scan quadratic); the second name is a lookahead so `a.b.c` gives both `a.b` and `b.c`.
_RE_PAIR = re.compile(r"(?<![\w$])(" + IDENT + r")\s*\??\.\s*(?=(" + IDENT + "))")
# `"name" in receiver` (feature detection).
_RE_IN = re.compile(r"""["'](""" + IDENT + r""")["']\s*in\s+(""" + IDENT + ")")
# A free identifier: not a property access (no dot before) and not an object literal key (no colon
# after), so a global or an interface used bare.
_RE_FREE = re.compile(r"(?<![\w$.])(" + IDENT + r")(?![\w$])(?!\s*:)")
# String literals and comments, removed before looking for free identifiers and strong uses
# (English words in comments and messages are not globals). Crude: template literals go whole,
# and a regular expression literal holding a quote can throw the scan off until the next line.
_RE_STRINGS_COMMENTS = re.compile(
    r'"(?:[^"\\\n]|\\.)*"' + r"|'(?:[^'\\\n]|\\.)*'" + r"|`(?:[^`\\]|\\.)*`" + r"|//[^\n]*|/\*.*?\*/",
    re.S,
)
# Uses that name an interface or global as such: `new X`, `instanceof X`, `typeof X`.
_RE_STRONG = re.compile(r"(?<![\w$.])(?:new|instanceof|typeof)\s+(" + IDENT + ")")
_RE_INLINE_SCRIPT = re.compile(r"<script\b[^>]*>(.*?)</script\s*>", re.S | re.I)

# Interfaces whose instance scripts reach through a well-known global: their members count only
# as `<global>.member` or `"member" in <global>`, which is exact even for generic names.
RECEIVERS = {
    "Window": ("window", "self", "globalThis"),
    "WorkerGlobalScope": ("self", "globalThis"),
    "DedicatedWorkerGlobalScope": ("self", "globalThis"),
    "Navigator": ("navigator",),
    "WorkerNavigator": ("navigator",),
    "Performance": ("performance",),
    "Document": ("document",),
    "Screen": ("screen",),
    "History": ("history",),
    "Location": ("location",),
    "WorkerLocation": ("location",),
    "CustomElementRegistry": ("customElements",),
    "Crypto": ("crypto",),
    "VisualViewport": ("visualViewport",),
    "Storage": ("localStorage", "sessionStorage"),
}
INSTANCES = {"window": RECEIVERS["Window"], "self": RECEIVERS["WorkerGlobalScope"],
             "document": ("document",), "location": ("location",), "navigator": ("navigator",),
             "screen": ("screen",)}


def match_kind(key: str) -> str:
    """How `references` matches a key: "exact" (the name with its receiver or its own identifier)
    or "name" (the bare member name after any dot: generic names over-count)."""
    h = head(key)
    if "." not in key or h in INSTANCES or ".prototype." not in key or h in RECEIVERS:
        return "exact"
    return "name"


class SiteTokens:
    """The token sets of one site's scripts."""

    def __init__(self):
        self.free: set[str] = set()
        self.strong: set[str] = set()
        self.dotted: set[str] = set()
        self.quoted: set[str] = set()
        self.pairs: set[str] = set()

    def add_script(self, text: str) -> None:
        code = _RE_STRINGS_COMMENTS.sub(" ", text)
        self.free.update(_RE_FREE.findall(code))
        self.strong.update(_RE_STRONG.findall(code))
        self.dotted.update(_RE_DOTTED.findall(text))
        self.quoted.update(_RE_QUOTED.findall(text))
        self.pairs.update(f"{a}.{b}" for a, b in _RE_PAIR.findall(text))
        self.pairs.update(f"{b}.{a}" for a, b in _RE_IN.findall(text))

    def add_html(self, text: str) -> None:
        for script in _RE_INLINE_SCRIPT.findall(text):
            self.add_script(script)

    def _via(self, receivers, name: str) -> bool:
        return any(f"{r}.{name}" in self.pairs for r in receivers)

    def references(self, key: str, kind: str) -> bool:
        """Whether these scripts plausibly reference the surface member `key` (of kind `kind`)."""
        name = member_name(key)
        if name.startswith("@@"):
            return False
        if kind in ("interface", "namespace") and "." not in key:
            if self._via(INSTANCES["window"], key) or key in self.strong:
                return True
            # A one-word name (Animation, Lock) is often a bundle's own class: only `new X`,
            # `instanceof X`, `typeof X`, `window.X` or `X.prototype` count for it.
            if re.fullmatch(r"[A-Z][a-z]+", key):
                return f"{key}.prototype" in self.pairs
            return key in self.free
        h = head(key)
        if h in ("window", "self"):
            # A global: used bare, or through window/self/globalThis; a plain-word one (`when`,
            # `external`) only through those or `typeof`.
            if self._via(INSTANCES[h], name) or name in self.strong:
                return True
            return not re.fullmatch(r"[a-z]+", name) and name in self.free
        if h in INSTANCES:
            return self._via(INSTANCES[h], name)
        if ".prototype." in key:
            if h in RECEIVERS:
                return self._via(RECEIVERS[h], name)
            return name in self.dotted or name in self.quoted
        # A static member or a namespace's: `Name.member` (or `Name?.member`).
        return f"{h}.{name}" in self.pairs


def site_tokens(site_dir: Path) -> SiteTokens:
    tokens = SiteTokens()
    index = json.loads((site_dir / "index.json").read_text(encoding="utf-8"))
    for r in index["resources"]:
        text = (site_dir / r["file"]).read_bytes().decode("utf-8", "replace")
        if r["type"] == "Document":
            tokens.add_html(text)
        else:
            tokens.add_script(text)
    return tokens


def usage(keys: dict[str, str], sites: dict[str, SiteTokens]) -> dict[str, list[str]]:
    """For each key (with its kind), the sites whose scripts reference it."""
    out = {}
    for key, kind in keys.items():
        hits = [s for s, t in sites.items() if t.references(key, kind)]
        if hits:
            out[key] = hits
    return out
