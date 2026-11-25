# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Snapshots of one URL in Chrome and in Snowlark after the page settles.

Both engines get the same steps: load, wait `SCROLL_AFTER_MS` after the load event, scroll down two
screens and back (so content revealed on scroll or loaded lazily appears), then wait until nothing is
animating, at most `settle_ms` after the load event. Then `walker.js` runs and a screenshot of the first
`SCREENS` screens is taken.
"""

import base64
import json
import os
import re
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, os.path.join(REPO, "tools", "layout-diff"))
sys.path.insert(0, os.path.join(REPO, "tools"))
from chrome import Chrome, CDPError  # noqa: E402
import devenv  # noqa: E402
import limits  # noqa: E402
from limits import Job  # noqa: E402

VIEWPORT_HEIGHT = 800  # Snowlark's --render viewport height
SCREENS = 3  # the viewport and the first two screens after scrolling
SCROLL_AFTER_MS = 3000  # as Snowlark's SNOWLARK_EVAL_LOADED, 3 s after the load event
# Each scroll is instant (a page's `scroll-behavior: smooth` would make where the page is at each step
# depend on how busy the engine is), and held long enough for scroll-triggered reveals to start.
SCROLL_STEP_MS = 1000
BEGIN, END = "@@VISDIFF-BEGIN@@", "@@VISDIFF-END@@"
# A page's render uses well under 1 GB; one far past it is a page whose memory runs away.
DEFAULT_MEMORY_GB = 6

# Declines a consent prompt in the page itself (the first button or link whose whole text is a refusal;
# prompts in frames stay), then scrolls one screen down, then another, then back to the top
# (SCROLL_STEP_MS apart). The same script runs in both engines.
SCROLL_SCRIPT = (
    "(function(){var re=/^(reject all|reject|decline all|decline|deny|refuse all|refuse|i do not agree|"
    "do not agree|disagree|only necessary|necessary only|only essential|essential only|use necessary cookies only|"
    "reject (all|optional|non-essential) cookies|continue without accepting)$/i,d=0;"
    "try{var b=document.querySelectorAll('button,a,[role=button],input[type=button]');"
    "for(var i=0;i<b.length&&!d;i++){var t=(b[i].innerText||b[i].value||'').replace(/\\s+/g,' ').trim();"
    "if(t.length<40&&re.test(t)){b[i].click();d=1}}}catch(e){}"
    "var h=innerHeight,s=function(y){scrollTo({left:0,top:y,behavior:'instant'})};s(h);"
    "setTimeout(function(){s(2*h)},%d);"
    "setTimeout(function(){s(0)},%d);return d})()" % (SCROLL_STEP_MS, 2 * SCROLL_STEP_MS)
)
# Animations and transitions that are running and will end (an infinite one never settles).
RUNNING_FINITE = (
    "(function(){try{return document.getAnimations().filter(function(a){return a.playState==='running'"
    "&&isFinite(a.effect.getComputedTiming().endTime)}).length}catch(e){return -1}})()"
)

FALLBACK_UA = ("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) "
               "Chrome/158.0.0.0 Safari/537.36 Snowlark/0.1")


def default_snowlark():
    """`SNOWLARK_CLI`, else this checkout's iter build, else its release build."""
    if os.environ.get("SNOWLARK_CLI"):
        return os.environ["SNOWLARK_CLI"]
    targets = [os.path.join(REPO, "target"),
               os.path.join(str(devenv.dev_dir()), ".target", os.path.basename(REPO))]
    for profile in ("iter", "release"):
        for t in targets:
            p = os.path.join(t, profile, "snowlark-cli.exe")
            if os.path.exists(p):
                return p
    return os.path.join(REPO, "target", "iter", "snowlark-cli.exe")


def snowlark_user_agent(exe):
    """The default user agent of the Snowlark build `exe` (its `navigator.userAgent`)."""
    try:
        proc = subprocess.run([exe, "--eval", "navigator.userAgent"], capture_output=True, text=True,
                              timeout=60, env={**os.environ, "SNOWLARK_NO_ACTIVATE": "1"})
        lines = [line.strip() for line in proc.stdout.splitlines() if line.strip().startswith("Mozilla/")]
        if proc.returncode == 0 and lines:
            return lines[-1]
    except (OSError, subprocess.TimeoutExpired):
        pass
    return FALLBACK_UA


def script(name, **subst):
    with open(os.path.join(HERE, name), encoding="utf-8") as f:
        src = f.read()
    # Comments off (SNOWLARK_EVAL travels in an environment variable, at most 32767 characters on Windows).
    src = "\n".join(line for line in src.splitlines() if not line.lstrip().startswith("//"))
    for key, value in subst.items():
        src = src.replace("__%s__" % key, str(value))
    return src


def walker_source(max_nodes):
    return script("walker.js", MAX_NODES=int(max_nodes))


def capture_snowlark(url, outdir, width, settle_ms, timeout_ms, max_nodes, exe, walk_s=180,
                     memory_gb=DEFAULT_MEMORY_GB):
    """Runs `snowlark-cli --render` with the scroll as SNOWLARK_EVAL_LOADED and the walker as SNOWLARK_EVAL,
    in a job that caps its memory at `memory_gb` and ends it with the run. Returns (snapshot or None, info);
    info["memory_gb"] is its peak, and info["over_memory"] says it reached the cap."""
    png = os.path.join(outdir, "snowlark.png")
    env = dict(os.environ)
    for k in ("SNOWLARK_BLOCK", "SNOWLARK_PROFILE", "SNOWLARK_PROFILE_DIR"):
        env.pop(k, None)
    env["SNOWLARK_NO_ACTIVATE"] = "1"
    env["SNOWLARK_EVAL_LOADED"] = SCROLL_SCRIPT
    env["SNOWLARK_EVAL"] = f"'{BEGIN}' + {walker_source(max_nodes).strip().rstrip(';')} + '{END}'"
    env["SNOWLARK_SETTLE_MS"] = str(settle_ms)
    env["SNOWLARK_COLOR_SCHEME"] = "light"
    env["SNOWLARK_RENDER_TIMEOUT_MS"] = str(timeout_ms + settle_ms)
    t = time.time()
    info = {"engine": "snowlark"}
    r = limits.run([exe, "--render", url, png, str(width), "--height", str(SCREENS * VIEWPORT_HEIGHT)],
                   (timeout_ms + settle_ms) / 1000 + walk_s, memory_gb * 2**30 if memory_gb else None, env)
    info["exit"] = r["exit"]
    out = r["out"].decode("utf-8", errors="replace")
    err = r["err"].decode("utf-8", errors="replace")
    info["memory_gb"] = round(r["peak"] / 2**30, 2) if r["peak"] else None
    # One large allocation that fails leaves the peak below the cap; Rust and V8 then say so on stderr.
    info["over_memory"] = r["over_cap"] or bool(
        memory_gb and re.search(r"memory allocation of \d+ bytes failed|out of memory", err, re.I))
    info["seconds"] = round(time.time() - t, 2)
    with open(os.path.join(outdir, "snowlark.log"), "w", encoding="utf-8") as f:
        f.write(err)
    m = re.search(r"\[render\] (loaded|not loaded) after (\d+) ms", err)
    info["loaded"] = bool(m and m.group(1) == "loaded")
    if m:
        info["settled_ms"] = int(m.group(2))
    snap = None
    i, j = out.find(BEGIN), out.rfind(END)
    if i >= 0 and j > i:
        try:
            snap = json.loads(out[i + len(BEGIN): j])
        except ValueError as e:
            info["error"] = f"snapshot is not JSON: {e}"
    else:
        e = re.search(r"\[eval\] threw (.*)", err)
        info["error"] = e.group(1) if e else "no snapshot in the output (exit %s)" % info["exit"]
    if info["over_memory"]:
        info["error"] = f"memory ran away: reached the {memory_gb:g} GB cap" + (
            " (exit %s)" % info["exit"] if snap is None else "")
    info["png"] = png if os.path.exists(png) else None
    return snap, info


def capture_chrome(chrome, url, outdir, width, settle_ms, timeout_ms, max_nodes, user_agent):
    """Loads `url` in a new page of `chrome` and snapshots it once it settles."""
    t = time.time()
    info = {"engine": "chrome"}
    session = chrome.new_page(width, VIEWPORT_HEIGHT, user_agent, "light")
    snap = None
    try:
        # Classic scrollbars would take 15 px of the width (--hide-scrollbars does not reach an emulated
        # viewport); Snowlark's --render has none.
        chrome.call("Emulation.setScrollbarsHidden", {"hidden": True}, session=session)
        info["loaded"] = chrome.navigate(session, url, timeout=timeout_ms / 1000)
        loaded = time.time()
        chrome.drain(SCROLL_AFTER_MS / 1000)
        info["declined"] = chrome.evaluate(session, SCROLL_SCRIPT)
        chrome.drain((2 * SCROLL_STEP_MS + 700) / 1000)
        # Until nothing that will end is still animating, at most `settle_ms` after the load event.
        while time.time() - loaded < settle_ms / 1000:
            n = chrome.evaluate(session, RUNNING_FINITE)
            if not n or n < 0:
                break
            chrome.drain(0.25)
        info["settled_ms"] = round((time.time() - loaded) * 1000)
        snap = json.loads(chrome.evaluate(session, walker_source(max_nodes), timeout=120))
        png = os.path.join(outdir, "chrome.png")
        try:
            chrome.screenshot(session, width, SCREENS * VIEWPORT_HEIGHT, png)
            info["png"] = png
        except CDPError as e:
            info["png"] = None
            info["screenshot_error"] = str(e)
    except (CDPError, ValueError, ConnectionError, OSError) as e:
        info["error"] = str(e)
    info["seconds"] = round(time.time() - t, 2)
    try:
        chrome.call("Target.detachFromTarget", {"sessionId": session}, timeout=5)
    except Exception:
        pass
    return snap, info


def pixel_check(chrome, chrome_png, snowlark_png, width, block=8, threshold=48):
    """The coarse pixel check (`pixels.js`) in a blank page of `chrome`; a dict, or None without both images."""
    if not chrome_png or not snowlark_png:
        return None

    def data_url(path):
        with open(path, "rb") as f:
            return "data:image/png;base64," + base64.b64encode(f.read()).decode()

    src = script("pixels.js", WIDTH=width, HEIGHT=SCREENS * VIEWPORT_HEIGHT, BLOCK=block,
                 THRESHOLD=threshold)
    src = src.replace("'__CHROME__'", json.dumps(data_url(chrome_png)))
    src = src.replace("'__SNOWLARK__'", json.dumps(data_url(snowlark_png)))
    session = chrome.new_page(width, VIEWPORT_HEIGHT)
    try:
        r = chrome.call("Runtime.evaluate", {"expression": src, "awaitPromise": True, "returnByValue": True},
                        session=session, timeout=120)
        if "exceptionDetails" in r:
            return {"error": str(r["exceptionDetails"].get("exception", {}).get("description"))}
        return json.loads(r["result"]["value"])
    finally:
        try:
            chrome.call("Target.detachFromTarget", {"sessionId": session}, timeout=5)
        except Exception:
            pass


def capture(url, outdir, width, args, user_agent):
    """Both engines on one URL at one width (Snowlark on a thread while Chrome runs). Returns a dict with
    the snapshots, the infos and the pixel check; the snapshots are also written to `outdir`."""
    import threading

    os.makedirs(outdir, exist_ok=True)
    result = {}

    def snowlark_job():
        result["snowlark"] = capture_snowlark(url, outdir, width, args.settle, args.timeout, args.max_nodes,
                                              args.snowlark, memory_gb=args.max_memory_gb)

    thread = threading.Thread(target=snowlark_job)
    thread.start()
    # Chrome's browser process ends with the run (its own processes follow it); its memory is not capped.
    chrome_job = Job()
    chrome = Chrome(width, VIEWPORT_HEIGHT, user_agent, chrome=args.chrome)
    chrome_job.add(chrome.proc.pid)
    try:
        c_snap, c_info = capture_chrome(chrome, url, outdir, width, args.settle, args.timeout, args.max_nodes,
                                        user_agent)
        thread.join()
        o_snap, o_info = result["snowlark"]
        try:
            pixels = pixel_check(chrome, c_info.get("png"), o_info.get("png"), width)
        except (CDPError, ConnectionError, OSError, ValueError) as e:
            pixels = {"error": str(e)}
    finally:
        thread.join()
        try:
            chrome.close()
        except Exception:
            pass
        chrome_job.close()
    for name, snap in (("chrome", c_snap), ("snowlark", o_snap)):
        if snap:
            with open(os.path.join(outdir, name + ".json"), "w", encoding="utf-8") as f:
                json.dump(snap, f)
    return {"chrome": (c_snap, c_info), "snowlark": (o_snap, o_info), "pixels": pixels}
