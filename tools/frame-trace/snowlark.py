# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Records what every frame and worker of a page logs, throws and fetches in Snowlark (`snowlark-cli --render`,
headless), in the format `reference.py` writes, for `diff.py`.

    python tools/frame-trace/snowlark.py <snowlark-cli.exe> <url> <seconds> <out.jsonl> [--png <file>]

The console comes from `SNOWLARK_CONSOLE=tagged` (each line names its realm: the frame's document or the
worker's script), requests from `SNOWLARK_TRACE=1` (`[net]` lines, which name no frame). The full stderr is
kept next to the output (`<out>.err`). Extra environment variables pass through (for example
`SNOWLARK_HOST_RESOLVER_RULES`).
"""

import argparse
import json
import os
import re
import subprocess
import threading
import time

CONSOLE = re.compile(r"^\[console\.(\w+)\] (.*?)(?: \((\S+:\d+:\d+)\))?(?: \[realm (\S*)\])?$")
NET_STATUS = re.compile(r"^\[net\] (\d{3})\s.*?\b(GET|POST|PUT|HEAD|OPTIONS|DELETE|PATCH)\b.*?(\S+)$")


def kind_of(realm):
    if realm.startswith("blob:") or realm.endswith(".js"):
        return "worker"
    return "page-or-frame"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("exe")
    ap.add_argument("url")
    ap.add_argument("seconds", type=float)
    ap.add_argument("out")
    ap.add_argument("--png")
    a = ap.parse_args()
    env = dict(os.environ)
    env.update(
        {
            "SNOWLARK_NO_ACTIVATE": "1",
            "SNOWLARK_CONSOLE": "tagged",
            "SNOWLARK_TRACE": "1",
            "SNOWLARK_BLOCK": "off",
            "SNOWLARK_SETTLE_MS": str(int(a.seconds * 1000)),
            "SNOWLARK_RENDER_TIMEOUT_MS": str(int((a.seconds + 30) * 1000)),
        }
    )
    png = a.png or a.out + ".png"
    start = time.time()
    proc = subprocess.Popen(
        [os.path.abspath(a.exe), "--render", a.url, png, "1280"],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    lines = []

    def pump():
        for raw in proc.stderr:
            lines.append((round((time.time() - start) * 1000), raw.decode("utf-8", "replace").rstrip("\r\n")))

    t = threading.Thread(target=pump, daemon=True)
    t.start()
    try:
        proc.wait(timeout=a.seconds + 90)
    except subprocess.TimeoutExpired:
        proc.kill()  # this run's own process, by its handle
    t.join(5)
    top = a.url
    with open(a.out + ".err", "w", encoding="utf-8") as err, open(a.out, "w", encoding="utf-8") as out:
        for ms, line in lines:
            err.write(line + "\n")
            rec = None
            m = CONSOLE.match(line)
            if m:
                level, text, source, realm = m.groups()
                realm = realm or ""
                type_ = "exception" if text.startswith("Uncaught") else "console"
                kind = "page" if realm == top else kind_of(realm)
                rec = (realm, kind, type_, text, level, source or "")
            elif line.startswith("[net] ERR") or line.startswith("[net] error"):
                rec = ("", "?", "failed", line[6:], "error", "")
            else:
                m = NET_STATUS.match(line)
                if m:
                    status, method, url = m.groups()
                    rec = ("", "?", "request", f"{method} {url}", "", "")
                    if int(status) >= 400:
                        out.write(json.dumps({"t": ms, "realm": "", "kind": "?", "type": "http",
                                              "level": "error", "text": f"{status} {url}", "source": ""}) + "\n")
            if rec:
                realm, kind, type_, text, level, source = rec
                out.write(json.dumps({"t": ms, "realm": realm, "kind": kind, "type": type_,
                                      "level": level, "text": text, "source": source}) + "\n")


if __name__ == "__main__":
    main()
