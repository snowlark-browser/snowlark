# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Records what every frame and worker of a page logs, throws and fetches in the reference browser
(headless, over the DevTools protocol), for `diff.py`.

    python tools/frame-trace/reference.py <url> <seconds> <out.jsonl> [--png <file>]

Every target is attached (frames in other processes, dedicated, shared and service workers, and the
ones they start), each paused at its start until its domains are on, so nothing it does early is
missed. Messages of frames that share a process with their parent come on the parent's session; the
execution context of each message names its frame. One JSON object per line, as `snowlark.py` writes:
`t` (ms since the navigation), `realm` (the URL of the frame's document or the worker's script),
`kind` (page, iframe, worker, ...), `type` (console, exception, log, issue, request, http, failed,
frame), `level`, `text`, `source`.
"""

import argparse
import json
import os
import socket
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "layout-diff"))
from chrome import CDPError, Chrome  # noqa: E402

WORKERS = ("worker", "shared_worker", "service_worker")


class Recorder:
    def __init__(self, browser, out):
        self.b = browser
        self.out = out
        self.start = time.time()
        self.sessions = {}  # session id -> {"type", "url"}
        self.frames = {}  # frame id -> url
        self.frame_parent = {}
        self.contexts = {}  # (session, context id) -> frame id
        self.requests = {}  # (session, request id) -> (url, realm)

    def write(self, realm, kind, type_, text, level="", source=""):
        rec = {
            "t": round((time.time() - self.start) * 1000),
            "realm": realm,
            "kind": kind,
            "type": type_,
            "level": level,
            "text": text,
            "source": source,
        }
        self.out.write(json.dumps(rec) + "\n")

    def call(self, method, params=None, session=None):
        try:
            return self.b.call(method, params, session=session)
        except CDPError as e:
            self.write("", "harness", "log", f"{method}: {e}", "warning")
            return {}

    def attach(self, session, info):
        self.sessions[session] = {"type": info["type"], "url": info.get("url", ""), "target": info.get("targetId")}
        for m in ("Runtime.enable", "Log.enable", "Network.enable", "Audits.enable"):
            self.call(m, session=session)
        if info["type"] in ("page", "iframe"):
            self.call("Page.enable", session=session)
            tree = self.call("Page.getFrameTree", session=session).get("frameTree")
            self.walk_tree(tree, None)
        self.call(
            "Target.setAutoAttach",
            {"autoAttach": True, "waitForDebuggerOnStart": True, "flatten": True},
            session=session,
        )
        self.call("Runtime.runIfWaitingForDebugger", session=session)

    def walk_tree(self, tree, parent):
        if not tree:
            return
        f = tree["frame"]
        self.frames[f["id"]] = f.get("url", "") + f.get("urlFragment", "")
        self.frame_parent[f["id"]] = parent
        for c in tree.get("childFrames", []):
            self.walk_tree(c, f["id"])

    def realm(self, session, context_id=None, frame_id=None):
        s = self.sessions.get(session, {"type": "?", "url": ""})
        if s["type"] in WORKERS:
            return s["url"], s["type"]
        if frame_id is None and context_id is not None:
            frame_id = self.contexts.get((session, context_id))
        if frame_id is not None:
            url = self.frames.get(frame_id) or "about:blank"
            kind = "page" if self.frame_parent.get(frame_id) is None and s["type"] == "page" else "iframe"
            return url, kind
        return s["url"], s["type"]

    def event(self, msg):
        m, p, s = msg["method"], msg.get("params", {}), msg.get("sessionId")
        if m == "Target.attachedToTarget":
            info = p["targetInfo"]
            if info.get("url", "").startswith("chrome-extension:"):
                # The browser's own component extensions: not the page's, and enabling their
                # domains can stall for seconds. Let them run, unrecorded.
                self.call("Runtime.runIfWaitingForDebugger", session=p["sessionId"])
                return
            self.write(info.get("url", ""), info["type"], "frame", "attached")
            self.attach(p["sessionId"], info)
        elif m == "Target.targetInfoChanged":
            info = p["targetInfo"]
            for sid, v in self.sessions.items():
                if v.get("target") == info["targetId"]:
                    v["url"] = info["url"]
        elif m == "Runtime.executionContextCreated":
            c = p["context"]
            aux = c.get("auxData", {})
            if aux.get("frameId") and aux.get("isDefault", True):
                self.contexts[(s, c["id"])] = aux["frameId"]
        elif m == "Page.frameAttached":
            self.frame_parent[p["frameId"]] = p.get("parentFrameId")
            self.frames.setdefault(p["frameId"], "about:blank")
        elif m == "Page.frameNavigated":
            f = p["frame"]
            self.frames[f["id"]] = f.get("url", "") + f.get("urlFragment", "")
            self.frame_parent[f["id"]] = f.get("parentId")
            if s in self.sessions and f.get("parentId") is None:
                self.sessions[s]["url"] = f["url"]
            realm, kind = self.realm(s, frame_id=f["id"])
            self.write(realm, kind, "frame", "navigated")
        elif m == "Runtime.consoleAPICalled":
            realm, kind = self.realm(s, context_id=p.get("executionContextId"))
            args = " ".join(
                str(a.get("value", a.get("description", a.get("unserializableValue", a.get("type")))))
                for a in p.get("args", [])
            )
            frames = (p.get("stackTrace") or {}).get("callFrames") or []
            src = f"{frames[0]['url']}:{frames[0]['lineNumber'] + 1}:{frames[0]['columnNumber'] + 1}" if frames else ""
            self.write(realm, kind, "console", args, p["type"], src)
        elif m == "Runtime.exceptionThrown":
            d = p["exceptionDetails"]
            realm, kind = self.realm(s, context_id=d.get("executionContextId"))
            text = d.get("exception", {}).get("description") or d.get("text", "")
            src = f"{d.get('url', '')}:{d.get('lineNumber', 0) + 1}:{d.get('columnNumber', 0) + 1}"
            self.write(realm, kind, "exception", text.split("\n")[0], "error", src)
        elif m == "Log.entryAdded":
            e = p["entry"]
            realm, kind = self.realm(s)
            self.write(realm, kind, "log", e["text"], e["level"], e.get("url", ""))
        elif m == "Audits.issueAdded":
            issue = p["issue"]
            details = issue.get("details", {})
            frame_id = None
            for d in details.values():
                if isinstance(d, dict):
                    frame_id = (d.get("frameId") or (d.get("frame") or {}).get("frameId")
                                or (d.get("sourceCodeLocation") or {}).get("frameId") or frame_id)
            realm, kind = self.realm(s, frame_id=frame_id)
            self.write(realm, kind, "issue", issue["code"] + " " + json.dumps(details)[:400], "issue")
        elif m == "Network.requestWillBeSent":
            r = p["request"]
            realm, kind = self.realm(s, frame_id=p.get("frameId"))
            self.requests[(s, p["requestId"])] = (r["url"], realm, kind)
            self.write(realm, kind, "request", f"{r['method']} {p.get('type', '')} {r['url']}")
        elif m == "Network.responseReceived":
            r = p["response"]
            if r["status"] >= 400:
                realm, kind = self.realm(s, frame_id=p.get("frameId"))
                self.write(realm, kind, "http", f"{r['status']} {r['url']}", "error")
        elif m == "Network.loadingFailed":
            url, realm, kind = self.requests.get((s, p["requestId"]), ("?", *self.realm(s)))
            why = p.get("errorText", "")
            for k in ("blockedReason", "corsErrorStatus"):
                if p.get(k):
                    why += f" {k}={json.dumps(p[k])}"
            self.write(realm, kind, "failed", f"{url} {why}", "error")

    def next_event(self, until):
        if self.b.events:
            return self.b.events.pop(0)
        left = until - time.time()
        if left <= 0:
            return None
        try:
            msg = json.loads(self.b.ws.recv(max(0.05, left)))
        except (socket.timeout, TimeoutError):
            return None
        return msg if "method" in msg else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("url")
    ap.add_argument("seconds", type=float)
    ap.add_argument("out")
    ap.add_argument("--png")
    ap.add_argument(
        "--eval-at",
        nargs=2,
        metavar=("SECONDS", "JS"),
        help="evaluate JS in the top page SECONDS after the navigation starts (as snowlark.py's"
        " SNOWLARK_EVAL_LOADED: press a button, then record what follows)",
    )
    a = ap.parse_args()
    if a.eval_at:
        a.eval_at = (float(a.eval_at[0]), a.eval_at[1])
    b = Chrome(width=1280, height=900)
    try:
        with open(a.out, "w", encoding="utf-8") as out:
            rec = Recorder(b, out)
            # Shared and service workers belong to no page session: the browser's own auto-attach
            # brings them (and only them).
            b.call(
                "Target.setAutoAttach",
                {
                    "autoAttach": True,
                    "waitForDebuggerOnStart": True,
                    "flatten": True,
                    "filter": [{"type": "shared_worker"}, {"type": "service_worker"}],
                },
            )
            page = b.new_page(1280, 900)
            rec.attach(page, {"type": "page", "url": "about:blank"})
            rec.start = time.time()
            b.call("Page.navigate", {"url": a.url}, session=page)
            deadline = time.time() + a.seconds
            eval_at = rec.start + a.eval_at[0] if a.eval_at else None
            while time.time() < deadline:
                msg = rec.next_event(min(deadline, eval_at) if eval_at else deadline)
                if msg:
                    rec.event(msg)
                if eval_at and time.time() >= eval_at:
                    eval_at = None
                    r = b.call("Runtime.evaluate", {"expression": a.eval_at[1]}, session=page)
                    rec.write("", "harness", "eval", json.dumps(r)[:300])
            for fid, url in rec.frames.items():
                rec.write(url, "frame-tree", "frame", f"id={fid} parent={rec.frame_parent.get(fid)}")
            if a.png:
                b.screenshot(page, 1280, 900, a.png)
    finally:
        b.close()


if __name__ == "__main__":
    main()
