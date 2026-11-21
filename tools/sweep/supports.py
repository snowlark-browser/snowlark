# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Part 1: what CSS Chrome accepts and Snowlark does not, weighted by how many corpus sites use it.

1. `collect_styles`: each corpus site in headless Chrome; every author stylesheet it ends up with (linked,
   inline, imported, constructed) is saved under `<out>/styles/<site>/`.
2. `usage`: the declarations, selectors, media queries and at-rules those sheets use, each with the sites.
3. `build_items` and `probe`: one probe page asks Chrome, then Snowlark (`--render`), about every distinct
   item: `CSS.supports(property, value)`, `CSS.supports("selector(...)")`, `matchMedia(query).media`, and
   whether a rule parses in a `<style>` sheet (at-rules, nesting). Chrome also lists every property it
   knows; Snowlark is asked about each with `CSS.supports(name, "inherit")`.
4. `diff` and `blame`: the items Chrome takes and Snowlark refuses, each blamed on the feature (property,
   function, unit, keyword, pseudo-class, media feature, at-rule) that fails everywhere it is used, and
   ranked by sites.
"""

from __future__ import annotations

import http.server
import json
import os
import re
import subprocess
import threading
import time
from collections import defaultdict
from pathlib import Path

import cssparse

HERE = Path(__file__).resolve().parent

MAX_VALUE = 240  # longer declarations and selectors are left out of the probe (data URLs, generated CSS)

# Rules probed whole, by at-rule name; a site uses the feature when its sheets have that at-rule.
RULE_PROBES = {
    "container": "@container (min-width: 1px) { a { color: red } }",
    "container-name": "@container card (min-width: 1px) { a { color: red } }",
    "container-style": "@container style(--x: 1) { a { color: red } }",
    "container-scroll-state": "@container scroll-state(stuck: top) { a { color: red } }",
    "layer": "@layer base { a { color: red } }",
    "layer-statement": "@layer a, b;",
    "property": "@property --sweep-x { syntax: '<length>'; inherits: false; initial-value: 0px; }",
    "scope": "@scope (.a) to (.b) { a { color: red } }",
    "starting-style": "@starting-style { a { opacity: 0 } }",
    "supports": "@supports (display: grid) { a { color: red } }",
    "supports-selector": "@supports selector(:has(a)) { a { color: red } }",
    "media": "@media (min-width: 1px) { a { color: red } }",
    "font-face": "@font-face { font-family: sweep; src: local(Arial); }",
    "keyframes": "@keyframes sweep { from { opacity: 0 } to { opacity: 1 } }",
    "counter-style": "@counter-style sweep { system: cyclic; symbols: '*'; }",
    "font-palette-values": "@font-palette-values --sweep { font-family: Arial; base-palette: 0; }",
    "font-feature-values": "@font-feature-values Arial { @swash { fancy: 1; } }",
    "page": "@page { margin: 1cm }",
    "position-try": "@position-try --sweep { top: anchor(bottom); }",
    "view-transition": "@view-transition { navigation: auto; }",
    "namespace": "@namespace svg url(http://www.w3.org/2000/svg);",
    "nesting": "a { color: red; & b { color: blue } }",
    "nesting-implicit": "a { color: red; b & { color: blue } }",
    "nesting-media": "a { color: red; @media (min-width: 1px) { color: blue } }",
}
# The corpus at-rule names that count as a use of each probe (nesting is counted from nested selectors).
PROBE_USES = {
    "container": ("container",), "layer": ("layer",), "property": ("property",), "scope": ("scope",),
    "starting-style": ("starting-style",), "supports": ("supports",), "media": ("media",),
    "font-face": ("font-face",), "keyframes": ("keyframes", "-webkit-keyframes"),
    "counter-style": ("counter-style",), "font-palette-values": ("font-palette-values",),
    "font-feature-values": ("font-feature-values",), "page": ("page",), "position-try": ("position-try",),
    "view-transition": ("view-transition",), "namespace": ("namespace",),
}


# ----- 1. stylesheets ----------------------------------------------------------------------------

def collect_styles(chrome, url: str, outdir: Path, timeout: float = 45.0, settle: float = 2.0) -> dict:
    """Loads `url` in a new tab (desktop user agent, 1280x800) and saves its author stylesheets."""
    from cdp import CdpError

    outdir.mkdir(parents=True, exist_ok=True)
    index = {"url": url, "sheets": [], "errors": []}
    started = time.time()
    target, session = chrome.new_page()
    try:
        chrome.call("Page.enable", {}, session)
        chrome.call("Network.enable", {}, session)
        chrome.call("Network.setUserAgentOverride", {"userAgent": chrome.user_agent}, session)
        chrome.call("Emulation.setDeviceMetricsOverride",
                    {"width": 1280, "height": 800, "deviceScaleFactor": 1, "mobile": False}, session)
        chrome.call("Page.navigate", {"url": url}, session)
        loaded = False
        while time.time() - started < timeout and not loaded:
            ev = chrome.next_event(0.25)
            if ev and ev.get("sessionId") == session and ev["method"] == "Page.loadEventFired":
                loaded = True
        end = time.time() + settle
        while time.time() < end:
            chrome.next_event(0.1)
        index["loaded"] = loaded
        chrome.events.clear()
        chrome.call("DOM.enable", {}, session)
        chrome.call("CSS.enable", {}, session, timeout=60)
        headers = {}
        end = time.time() + 1.5
        while time.time() < end:
            ev = chrome.next_event(0.2)
            if ev and ev.get("sessionId") == session and ev["method"] == "CSS.styleSheetAdded":
                h = ev["params"]["header"]
                if h.get("origin") == "regular":
                    headers[h["styleSheetId"]] = h
        for n, (sid, h) in enumerate(headers.items()):
            try:
                text = chrome.call("CSS.getStyleSheetText", {"styleSheetId": sid}, session, timeout=30)["text"]
            except CdpError as e:
                index["errors"].append(f"{h.get('sourceURL')}: {e}")
                continue
            name = f"{n:03}.css"
            (outdir / name).write_text(text, encoding="utf-8")
            index["sheets"].append({"file": name, "url": h.get("sourceURL") or "", "inline": h.get("isInline"),
                                    "constructed": h.get("isConstructed"), "bytes": len(text)})
    except CdpError as e:
        index["errors"].append(str(e))
    finally:
        chrome.close_page(target)
    index["seconds"] = round(time.time() - started, 1)
    (outdir / "index.json").write_text(json.dumps(index, indent=1), encoding="utf-8")
    return index


# ----- 2. usage ----------------------------------------------------------------------------------

class Usage:
    """What the corpus's stylesheets use, each with the set of sites that use it."""

    def __init__(self):
        self.decl = defaultdict(set)      # (prop, value) -> sites
        self.prop = defaultdict(set)      # prop -> sites
        self.sel = defaultdict(set)       # selector -> sites
        self.media = defaultdict(set)     # media query (one of a list) -> sites
        self.at = defaultdict(set)        # at-rule name -> sites
        self.container = defaultdict(set)  # @container prelude -> sites
        self.nested = set()               # sites with nested style rules

    def add_text(self, site: str, text: str) -> None:
        for ev in cssparse.scan(text):
            if ev[0] == "decl":
                _, p, v = ev
                self.prop[p].add(site)
                self.decl[(p, v)].add(site)
            elif ev[0] == "selector":
                _, s, nested = ev
                self.sel[s].add(site)
                if nested:
                    self.nested.add(site)
            else:
                _, name, prelude = ev
                self.at[name].add(site)
                if name in ("media", "import") and prelude:
                    q = prelude if name == "media" else _import_media(prelude)
                    for part in cssparse.split_top(q, ","):
                        part = " ".join(part.split())
                        if part:
                            self.media[part].add(site)
                elif name == "container" and prelude:
                    self.container[prelude].add(site)

    def to_json(self) -> dict:
        return {
            "decl": [[p, v, sorted(s)] for (p, v), s in self.decl.items()],
            "prop": {p: sorted(s) for p, s in self.prop.items()},
            "sel": {k: sorted(s) for k, s in self.sel.items()},
            "media": {k: sorted(s) for k, s in self.media.items()},
            "at": {k: sorted(s) for k, s in self.at.items()},
            "container": {k: sorted(s) for k, s in self.container.items()},
            "nested": sorted(self.nested),
        }

    @classmethod
    def from_json(cls, d: dict) -> "Usage":
        u = cls()
        for p, v, s in d["decl"]:
            u.decl[(p, v)] = set(s)
        for name in ("prop", "sel", "media", "at", "container"):
            getattr(u, name).update({k: set(s) for k, s in d[name].items()})
        u.nested = set(d["nested"])
        return u


def _import_media(prelude: str) -> str:
    """The media query list of an @import prelude (after the URL, layer() and supports())."""
    rest = re.sub(r"^(url\([^)]*\)|\"[^\"]*\"|'[^']*')\s*", "", prelude.strip())
    rest = re.sub(r"^layer(\([^)]*\))?\s*", "", rest)
    rest = re.sub(r"^supports\((?:[^()]|\([^()]*\))*\)\s*", "", rest)
    return rest


def usage_of(styles_root: Path, sites: list[str]) -> Usage:
    u = Usage()
    for site in sites:
        d = styles_root / site
        if not d.is_dir():
            continue
        for f in sorted(d.glob("*.css")):
            u.add_text(site, f.read_text(encoding="utf-8", errors="replace"))
    return u


# ----- 3. the probe ------------------------------------------------------------------------------

def build_items(u: Usage) -> dict:
    """The distinct things to ask both engines about, in a fixed order."""
    decl = sorted((p, v) for (p, v) in u.decl
                  if len(v) <= MAX_VALUE and "var(" not in v.lower() and not re.search(r"\$|\{\{", v))
    sel = sorted(s for s in u.sel if len(s) <= MAX_VALUE)
    media = sorted(m for m in u.media if len(m) <= MAX_VALUE)
    rules = dict(RULE_PROBES)
    for prelude in sorted(u.container):
        if len(prelude) <= MAX_VALUE:
            rules["container:" + prelude] = f"@container {prelude} {{ a {{ color: red }} }}"
    return {"decl": [list(d) for d in decl], "sel": sel, "media": media,
            "rule_keys": list(rules), "rule": list(rules.values()), "prop": []}


class ProbeServer:
    """Serves probe.html, probe.js and the items; takes each engine's answers by POST."""

    def __init__(self, items: dict):
        self.items = items
        self.results: dict[str, dict] = {}
        self.cond = threading.Condition()

    def __enter__(self):
        server = self
        page = b"<!doctype html><meta charset=utf-8><title>probe</title><script src=probe.js></script>"
        js = (HERE / "probe.js").read_bytes()

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                path = self.path.split("?", 1)[0]
                if path == "/probe.html":
                    body, ctype = page, "text/html; charset=utf-8"
                elif path == "/probe.js":
                    body, ctype = js, "text/javascript"
                elif path == "/items.json":
                    body, ctype = json.dumps(server.items).encode(), "application/json"
                else:
                    self.send_error(404)
                    return
                self.send_response(200)
                self.send_header("content-type", ctype)
                self.send_header("content-length", str(len(body)))
                self.send_header("cache-control", "no-store")
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                engine = self.path.split("engine=", 1)[-1].split("&")[0]
                body = self.rfile.read(int(self.headers.get("content-length", "0")))
                with server.cond:
                    server.results[engine] = json.loads(body)
                    server.cond.notify_all()
                self.send_response(204)
                self.end_headers()

        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
        return self

    def __exit__(self, *exc):
        self.httpd.shutdown()
        self.httpd.server_close()

    def url(self, engine: str) -> str:
        return f"http://127.0.0.1:{self.httpd.server_address[1]}/probe.html?engine={engine}"

    def wait(self, engine: str, timeout: float) -> dict | None:
        end = time.time() + timeout
        with self.cond:
            while engine not in self.results:
                left = end - time.time()
                if left <= 0:
                    return None
                self.cond.wait(left)
            return self.results[engine]


def probe(items: dict, chrome_exe: str, snowlark: str, workdir: Path) -> tuple[dict, dict]:
    """Runs the probe page in Chrome, then in Snowlark; returns both engines' answers."""
    from cdp import Chrome

    with ProbeServer(items) as server:
        with Chrome(chrome_exe) as chrome:
            target, session = chrome.new_page()
            chrome.call("Page.navigate", {"url": server.url("chrome")}, session)
            c = server.wait("chrome", 300)
            chrome.close_page(target)
        if c is None or "error" in c:
            raise RuntimeError(f"Chrome's probe failed: {c and c.get('error')}")
        items["prop"] = c["props"]
        env = {k: v for k, v in os.environ.items() if k != "SNOWLARK_BLOCK"}
        env.update(SNOWLARK_RENDER_TIMEOUT_MS="300000", SNOWLARK_NO_ACTIVATE="1")
        log = workdir / "probe-snowlark.log"
        with open(log, "wb") as out:
            proc = subprocess.Popen([snowlark, "--render", server.url("snowlark"), str(workdir / "probe.png")],
                                    stdout=out, stderr=subprocess.STDOUT, env=env)
            s = server.wait("snowlark", 400)
            try:
                proc.wait(timeout=60)
            except subprocess.TimeoutExpired:
                subprocess.run(["taskkill", "/PID", str(proc.pid), "/T", "/F"], capture_output=True)
        if s is None or "error" in s:
            raise RuntimeError(f"Snowlark's probe failed: {s and s.get('error')} (see {log})")
    return c, s


# ----- 4. the diff -------------------------------------------------------------------------------

def media_tokens(query: str) -> list[str]:
    """The features of a media query: `mf:<name>` per media feature, `media:range-syntax`, `media:<type>`."""
    q = query.lower()
    toks = [f"mf:{m.group(1)}" for m in re.finditer(r"\(\s*([a-z-]+)\s*[:)<>=]", q)]
    toks += [f"mf:{m.group(1)}" for m in re.finditer(r"[<>=]\s*([a-z-]+)\s*\)", q)]
    if re.search(r"\([^():]*[<>=][^():]*\)", q):
        toks.append("media:range-syntax")
    m = re.match(r"\s*(?:only\s+|not\s+)?([a-z-]+)", q)
    if m and m.group(1) not in ("and", "not", "only", "or"):
        toks.append(f"media:{m.group(1)}")
    return list(dict.fromkeys(toks))


def failures(items: dict, c: dict, s: dict, u: Usage) -> list[dict]:
    """Every item Chrome takes and Snowlark refuses, with its tokens and sites."""
    out = []
    unsupported_props = {p for p, ok in zip(items["prop"], s["prop"]) if not ok}
    for (p, v), oc, os_ in zip(items["decl"], c["decl"], s["decl"]):
        if oc and not os_:
            toks = [f"prop:{p}"] if p in unsupported_props else cssparse.value_tokens(p, v)
            out.append({"kind": "decl", "text": f"{p}: {v}", "tokens": toks, "sites": sorted(u.decl[(p, v)])})
    for sel, oc, os_ in zip(items["sel"], c["sel"], s["sel"]):
        if oc and not os_:
            out.append({"kind": "sel", "text": sel, "tokens": cssparse.selector_tokens(sel),
                        "sites": sorted(u.sel[sel])})
    for q, oc, os_ in zip(items["media"], c["media"], s["media"]):
        if oc and not os_:
            out.append({"kind": "media", "text": q, "tokens": media_tokens(q), "sites": sorted(u.media[q])})
    for key, text, rc, rs in zip(items["rule_keys"], items["rule"], c["rule"], s["rule"]):
        if rc[0] and (rs[0] == 0 or rc[1] != rs[1] or rc[2] != rs[2]):
            if key.startswith("container:"):
                sites = sorted(u.container[key[len("container:"):]])
                toks = ["at:container"] + [t.replace("mf:", "container-mf:") for t in media_tokens(key)]
                toks += [f"container-fn:{m}()" for m in re.findall(r"([a-z-]+)\(", key[len("container:"):])]
            elif key.startswith("nesting"):
                sites, toks = sorted(u.nested), [f"rule:{key}"]
            else:
                names = PROBE_USES.get(key) or PROBE_USES.get(key.split("-")[0]) or (key,)
                sites = sorted(set().union(*(u.at.get(n, set()) for n in names)))
                # Parsed, but with another CSSOM interface or other child rules: the CSSOM is short.
                toks = [f"rule:{key}" if rs[0] == 0 else f"cssom:{key} ({rs[1]}, {rs[2]} child rules)"]
            out.append({"kind": "rule", "text": text, "tokens": toks, "sites": sites,
                        "chrome": rc, "snowlark": rs})
    return out


def blame(fails: list[dict], passes: dict[str, int]) -> dict[str, dict]:
    """Groups failing items by the feature to blame.

    `passes[token]` is how many items with that token Snowlark accepted. A failing item is blamed on its
    token that fails most consistently (fewest passes for its failures); a token that never passes is the
    feature. When every token also passes elsewhere, the item is its own group (the combination fails)."""
    fail_count: dict[str, int] = defaultdict(int)
    for f in fails:
        for t in f["tokens"]:
            fail_count[t] += 1
    groups: dict[str, dict] = {}
    for f in fails:
        best, best_score = None, -1.0
        for t in f["tokens"]:
            score = fail_count[t] / (fail_count[t] + passes.get(t, 0))
            if score > best_score:
                best, best_score = t, score
        if best is None or best_score < 1.0:
            key = f"{f['kind']}:{cssparse.value_shape(f['text'])}" if f["kind"] == "decl" else f"{f['kind']}:{f['text']}"
            if best is not None and best_score >= 0.5:
                key = f"{best} (mostly)"
        else:
            key = best
        g = groups.setdefault(key, {"key": key, "sites": set(), "items": 0, "examples": []})
        g["sites"].update(f["sites"])
        g["items"] += 1
        if len(g["examples"]) < 4:
            g["examples"].append(f["text"])
    return groups


def pass_counts(items: dict, c: dict, s: dict) -> dict[str, int]:
    """For each token, the number of items with it that both engines accept."""
    passes: dict[str, int] = defaultdict(int)
    unsupported_props = {p for p, ok in zip(items["prop"], s["prop"]) if not ok}
    for (p, v), oc, os_ in zip(items["decl"], c["decl"], s["decl"]):
        if oc and os_:
            for t in ([f"prop:{p}"] if p in unsupported_props else cssparse.value_tokens(p, v)):
                passes[t] += 1
    for sel, oc, os_ in zip(items["sel"], c["sel"], s["sel"]):
        if oc and os_:
            for t in cssparse.selector_tokens(sel):
                passes[t] += 1
    for q, oc, os_ in zip(items["media"], c["media"], s["media"]):
        if oc and os_:
            for t in media_tokens(q):
                passes[t] += 1
    return passes


def property_gaps(items: dict, s: dict, u: Usage) -> list[dict]:
    """Chrome's properties that Snowlark does not know, with the corpus sites that declare each."""
    out = []
    for p, ok in zip(items["prop"], s["prop"]):
        if not ok:
            out.append({"property": p, "sites": sorted(u.prop.get(p, ()))})
    out.sort(key=lambda r: (-len(r["sites"]), r["property"]))
    return out


def ranked(groups: dict[str, dict]) -> list[dict]:
    rows = [{**g, "sites": sorted(g["sites"])} for g in groups.values()]
    rows.sort(key=lambda r: (-len(r["sites"]), -r["items"], r["key"]))
    return rows
