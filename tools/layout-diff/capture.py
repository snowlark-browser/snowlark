# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Snapshots of one URL in Chrome and in Snowlark: the walker's JSON plus a screenshot per engine."""

import json
import os
import re
import subprocess
import time

from chrome import Chrome, CDPError
import os as _os, sys as _sys  # noqa: E401
_sys.path.insert(0, _os.path.join(_os.path.dirname(_os.path.abspath(__file__)), ".."))
import devenv  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
VIEWPORT_HEIGHT = 800  # Snowlark's --render viewport height
MAX_IMAGE_HEIGHT = 4000  # Snowlark's --render image height cap
BEGIN, END = "@@LAYOUT-DIFF-BEGIN@@", "@@LAYOUT-DIFF-END@@"

# Firefox's format, as snowlark-net sends it; read from the source when this runs in a checkout.
FALLBACK_UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:150.0) Gecko/20100101 Firefox/150.0"


def snowlark_user_agent():
    try:
        with open(os.path.join(REPO, "crates", "snowlark-net", "src", "metadata.rs"), encoding="utf-8") as f:
            m = re.search(r'pub const USER_AGENT: &str =\s*"([^"]+)"', f.read())
            if m:
                return m.group(1)
    except OSError:
        pass
    return FALLBACK_UA


def default_snowlark():
    for p in (
        os.environ.get("SNOWLARK_CLI"),
        os.path.join(REPO, "target", "release", "snowlark-cli.exe"),
        os.path.join(str(devenv.dev_dir()), ".target", os.path.basename(REPO), "release", "snowlark-cli.exe"),
    ):
        if p and os.path.exists(p):
            return p
    return os.path.join(REPO, "target", "release", "snowlark-cli.exe")


def walker_source(max_nodes):
    with open(os.path.join(HERE, "walker.js"), encoding="utf-8") as f:
        src = f.read()
    # Comments off (SNOWLARK_EVAL travels in an environment variable, at most 32767 characters on Windows).
    src = "\n".join(line for line in src.splitlines() if not line.lstrip().startswith("//"))
    return src.replace("__MAX_NODES__", str(int(max_nodes)))


def capture_snowlark(url, outdir, width, settle_ms, timeout_ms, max_nodes, snowlark, walk_s=300, color_scheme="light"):
    """Runs `snowlark-cli --render` with the walker as SNOWLARK_EVAL. Returns (snapshot or None, info)."""
    png = os.path.join(outdir, "snowlark.png")
    src = walker_source(max_nodes)
    env = dict(os.environ)
    env.pop("SNOWLARK_BLOCK", None)  # content blocking off (the default)
    env.pop("SNOWLARK_PROFILE", None)
    env["SNOWLARK_EVAL"] = f"'{BEGIN}' + {src.strip().rstrip(';')} + '{END}'"
    env["SNOWLARK_SETTLE_MS"] = str(settle_ms)
    env["SNOWLARK_COLOR_SCHEME"] = color_scheme
    env["SNOWLARK_RENDER_TIMEOUT_MS"] = str(timeout_ms)
    t = time.time()
    info = {"engine": "snowlark", "binary": snowlark}
    try:
        proc = subprocess.run(
            [snowlark, "--render", url, png, str(width)],
            env=env,
            capture_output=True,
            # The snapshot itself can take minutes in Snowlark on large pages (each CSSOM read costs time
            # linear in the document size, 2026-11-01).
            timeout=timeout_ms / 1000 + settle_ms / 1000 + walk_s,
        )
        out = proc.stdout.decode("utf-8", errors="replace")
        err = proc.stderr.decode("utf-8", errors="replace")
        info["exit"] = proc.returncode
    except subprocess.TimeoutExpired as e:  # subprocess.run kills the child it started
        out = (e.stdout or b"").decode("utf-8", errors="replace")
        err = (e.stderr or b"").decode("utf-8", errors="replace")
        info["exit"] = "timeout"
    info["seconds"] = round(time.time() - t, 2)
    with open(os.path.join(outdir, "snowlark.log"), "w", encoding="utf-8") as f:
        f.write(err)
    m = re.search(r"\[render\] (loaded|not loaded) after", err)
    info["loaded"] = bool(m and m.group(1) == "loaded")
    snap = None
    i, j = out.find(BEGIN), out.rfind(END)
    if i >= 0 and j > i:
        snap = json.loads(out[i + len(BEGIN) : j])
    else:
        e = re.search(r"\[eval\] threw (.*)", err)
        info["error"] = e.group(1) if e else "no snapshot in the output (exit %s)" % info["exit"]
    info["png"] = png if os.path.exists(png) else None
    return snap, info


def capture_chrome(chrome, url, outdir, width, settle_ms, timeout_ms, max_nodes, user_agent, color_scheme="light"):
    """Loads `url` in a new page of `chrome`, waits for load plus `settle_ms`, snapshots and screenshots."""
    t = time.time()
    info = {"engine": "chrome"}
    session = chrome.new_page(width, VIEWPORT_HEIGHT, user_agent, color_scheme)
    snap = None
    try:
        info["loaded"] = chrome.navigate(session, url, timeout=timeout_ms / 1000)
        chrome.drain(settle_ms / 1000)
        snap = json.loads(chrome.evaluate(session, walker_source(max_nodes)))
        doc_h = max(VIEWPORT_HEIGHT, min(MAX_IMAGE_HEIGHT, snap["scroll"][1]))
        png = os.path.join(outdir, "chrome.png")
        try:
            chrome.screenshot(session, width, doc_h, png)
            info["png"] = png
        except CDPError as e:
            info["png"] = None
            info["screenshot_error"] = str(e)
    except (CDPError, ValueError, ConnectionError) as e:
        info["error"] = str(e)
    info["seconds"] = round(time.time() - t, 2)
    info["session"] = session
    return snap, info
