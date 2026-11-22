# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Chrome-versus-Snowlark layout diff.

    python tools/layout-diff/run.py <url> [--out DIR] [--width 1280] [--settle 2000] [--tol 1] [--first 10]
    python tools/layout-diff/run.py --corpus tools/layout-diff/corpus.txt --out DIR

Loads the URL in headless Chrome (DevTools protocol) and in Snowlark (`snowlark-cli --render` with the walker
as SNOWLARK_EVAL), at the same CSS width, device scale 1, Snowlark's user agent, after the load event plus
the settle delay; snapshots every element's box and computed styles in both, aligns the trees, and writes
report.html (root causes in document order, the first one being "the first difference"), result.json, both
snapshots and screenshots, and side-by-side.png. See README.md.
"""

import argparse
import json
import os
import re
import sys
import threading
import time
import urllib.parse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import capture  # noqa: E402
import compare  # noqa: E402
import report  # noqa: E402
from chrome import CHROME, Chrome  # noqa: E402

BLOCKED = [
    (r"verify you are (a )?human|are you a robot|captcha|unusual traffic|just a moment|access denied|"
     r"press (and|&) hold|bot detection|request blocked|enable javascript and cookies|"
     r"awswafintegration|captcha-delivery|var dd=\{", "bot check"),
    (r"before you continue|cookie(s)? (settings|preferences|policy)|we use cookies|accept all|"
     r"manage (your )?(privacy|cookies|consent)|your privacy choices", "consent/cookie prompt"),
]


def slug(url):
    u = urllib.parse.urlparse(url)
    s = (u.netloc + u.path).strip("/") or "page"
    return re.sub(r"[^A-Za-z0-9._-]+", "_", s)[:80]


def notes_for(chrome_snap, snowlark_snap):
    notes = []
    for name, snap in (("Chrome", chrome_snap), ("Snowlark", snowlark_snap)):
        if not snap:
            continue
        text = (snap.get("title", "") + " " + snap.get("text", "")).lower()
        for pattern, what in BLOCKED:
            if re.search(pattern, text):
                notes.append(f"{name}: possible {what}")
                break
    if chrome_snap and snowlark_snap:
        # (query strings often carry per-load tokens)
        if chrome_snap["url"].split("?")[0] != snowlark_snap["url"].split("?")[0]:
            notes.append(f"final URLs differ: {chrome_snap['url']} vs {snowlark_snap['url']}")
        nc, no = len(chrome_snap["nodes"]), len(snowlark_snap["nodes"])
        if abs(nc - no) > 0.2 * max(nc, no, 1):
            notes.append(f"element counts differ a lot ({nc} in Chrome, {no} in Snowlark): the pages' content "
                         "differs (scripts, shadow DOM), compare the screenshots")
        if chrome_snap.get("media") != snowlark_snap.get("media"):
            notes.append(f"media features differ: {chrome_snap.get('media')} vs {snowlark_snap.get('media')}")
        if chrome_snap.get("title") != snowlark_snap.get("title"):
            notes.append(f"titles differ: {chrome_snap.get('title')!r} vs {snowlark_snap.get('title')!r}")
        if snowlark_snap.get("truncated") or chrome_snap.get("truncated"):
            notes.append("snapshot truncated at --max-nodes")
    return notes


def run_one(url, outdir, args, chrome=None):
    """Captures, compares and reports one URL into `outdir`. Returns the result dict."""
    os.makedirs(outdir, exist_ok=True)
    started = time.time()
    ua = capture.snowlark_user_agent(args.snowlark)
    own = chrome is None
    if own:
        chrome = Chrome(args.width, capture.VIEWPORT_HEIGHT, ua, chrome=args.chrome)
    try:
        if args.recompare:
            c_snap, c_info, o_snap, o_info, old = load_saved(outdir)
        else:
            c_snap, c_info, o_snap, o_info = capture_both(url, outdir, args, chrome, ua)
            old = {}
        result = compare_and_report(url, outdir, args, chrome, ua, c_snap, c_info, o_snap, o_info)
    finally:
        if own:
            try:
                chrome.close()
            except Exception as e:  # the result stands without a clean shutdown
                print(f"  note: closing Chrome: {e}", file=sys.stderr)
    result["meta"]["total_s"] = old.get("total_s") or round(time.time() - started, 2)
    with open(os.path.join(outdir, "result.json"), "w", encoding="utf-8") as f:
        json.dump(result, f, indent=1)
    report.site_report(os.path.join(outdir, "report.html"), result)
    return result


def load_saved(outdir):
    """The snapshots and timings of an earlier run in `outdir` (--recompare)."""
    def snap(name):
        path = os.path.join(outdir, name + ".json")
        if not os.path.exists(path):
            return None
        with open(path, encoding="utf-8") as f:
            return json.load(f)

    old = {}
    try:
        with open(os.path.join(outdir, "result.json"), encoding="utf-8") as f:
            old_result = json.load(f)
        old = old_result.get("meta", {})
    except (OSError, ValueError):
        old_result = {}
    infos = []
    for name, label in (("chrome", "Chrome"), ("snowlark", "Snowlark")):
        png = os.path.join(outdir, name + ".png")
        info = {"seconds": old.get(name + "_s"), "loaded": old.get(name + "_loaded"),
                "png": png if os.path.exists(png) else None}
        errors = [w[len(label) + 2:] for w in old_result.get("warnings", []) if w.startswith(label + ": ")
                  and "no load event" not in w and "possible" not in w]
        if errors:
            info["error"] = errors[0]
        infos.append(info)
    return snap("chrome"), infos[0], snap("snowlark"), infos[1], old


def capture_both(url, outdir, args, chrome, ua):
    """Snowlark on a thread (unless --serial) while Chrome loads the same URL."""
    o_result = {}

    def snowlark_job():
        o_result["v"] = capture.capture_snowlark(
            url, outdir, args.width, args.settle, args.timeout, args.max_nodes, args.snowlark,
            getattr(args, "walk_timeout", 300), getattr(args, "color_scheme", "light"),
        )

    thread = threading.Thread(target=snowlark_job)
    thread.start()
    if args.serial:
        thread.join()
    c_snap, c_info = capture.capture_chrome(
        chrome, url, outdir, args.width, args.settle, args.timeout, args.max_nodes, ua,
        getattr(args, "color_scheme", "light"),
    )
    thread.join()
    o_snap, o_info = o_result["v"]
    for name, snap in (("chrome", c_snap), ("snowlark", o_snap)):
        if snap:
            with open(os.path.join(outdir, name + ".json"), "w", encoding="utf-8") as f:
                json.dump(snap, f)
    return c_snap, c_info, o_snap, o_info


def compare_and_report(url, outdir, args, chrome, ua, c_snap, c_info, o_snap, o_info):
    """Compares two snapshots and draws the side-by-side picture; returns the result dict."""
    result = {
        "meta": {
            "url": url,
            "width": args.width,
            "settle_ms": args.settle,
            "tol": args.tol,
            "ua": ua,
            "chrome_url": c_snap and c_snap["url"],
            "snowlark_url": o_snap and o_snap["url"],
            "chrome_s": c_info.get("seconds"),
            "snowlark_s": o_info.get("seconds"),
            "chrome_walk_ms": c_snap and c_snap.get("walk_ms"),
            "snowlark_walk_ms": o_snap and o_snap.get("walk_ms"),
            "chrome_loaded": c_info.get("loaded"),
            "snowlark_loaded": o_info.get("loaded"),
        },
        "warnings": notes_for(c_snap, o_snap),
    }
    for name, info in (("Chrome", c_info), ("Snowlark", o_info)):
        if info.get("error"):
            result["warnings"].append(f"{name}: {info['error']}")
        if info.get("loaded") is False:
            result["warnings"].append(f"{name}: no load event within the timeout")
    if c_snap and o_snap:
        diff = compare.Diff(c_snap, o_snap, tol=args.tol)
        result["summary"] = diff.summary()
        result["roots"] = diff.roots(args.first)
        result["raw"] = diff.raw(args.first)
        result["status"] = "ok"
        first = headline(result["roots"])
        result["first"] = result["roots"].index(first) if first else None
        shown = [first] + [r for r in result["roots"] if r is not first] if first else result["raw"]
        try:
            result["side_by_side"] = report.compose(
                chrome, shown, c_info.get("png"), o_info.get("png"),
                args.width, os.path.join(outdir, "side-by-side.png"), outdir,
            )
        except Exception as e:  # the picture is a convenience; the report stands without it
            result["warnings"].append(f"side-by-side picture failed: {e}")
    else:
        result["status"] = "error"
    return result


def headline(roots):
    """The first root cause that is visible (not only an inline box's rect)."""
    for it in roots:
        if it["kinds"] != ["inline-box"]:
            return it
    return roots[0] if roots else None


def first_line(result):
    roots = result.get("roots") or []
    if not roots:
        return "no difference" if result.get("status") == "ok" else "error"
    it = headline(roots)
    return f"{', '.join(it['kinds'])}: {it['label']} delta {it.get('delta')} | {report.props_text(it)}"


def summary_row(url, outdir, result, outroot):
    roots = result.get("roots") or []
    it = headline(roots)
    row = {
        "url": url,
        "status": result.get("status"),
        "report": os.path.relpath(os.path.join(outdir, "report.html"), outroot).replace("\\", "/"),
        "kind": ", ".join(it["kinds"]) if it else ("none" if result.get("status") == "ok" else "-"),
        "element": it["label"] if it else "",
        "path": it["path"] if it else "",
        "delta": it.get("delta") if it else "",
        "properties": report.props_text(it) if it else "",
        "chrome_s": result["meta"].get("chrome_s"),
        "snowlark_s": result["meta"].get("snowlark_s"),
        "total_s": result["meta"].get("total_s"),
        "notes": "; ".join(result.get("warnings", [])),
        "roots_top": [],
    }
    for it in roots:
        styles = {p: (va if va is not None else vb) for p, va, vb, _ in it["styles"]}
        for kind in it["kinds"]:
            row["roots_top"].append({
                "kind": kind,
                "tag": it["label"].split("#")[0].split(".")[0],
                "display": styles.get("display", ""),
                "differing": [p for p, _, _ in it["props"]["differing"]],
            })
    return row


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("url", nargs="?")
    ap.add_argument("--corpus", help="file with one URL per line (# comments)")
    ap.add_argument("--out", default="layout-diff-out")
    ap.add_argument("--width", type=int, default=1280, help="CSS width (default 1280)")
    ap.add_argument("--settle", type=int, default=2000, help="ms after the load event (default 2000)")
    ap.add_argument("--timeout", type=int, default=30000, help="ms to wait for the load event (default 30000)")
    ap.add_argument("--tol", type=float, default=1.0, help="px tolerance (default 1)")
    ap.add_argument("--first", type=int, default=10, help="differences listed per page (default 10)")
    ap.add_argument("--max-nodes", type=int, default=20000)
    ap.add_argument("--walk-timeout", type=int, default=300,
                    help="s allowed for Snowlark's snapshot script after the settle (default 300)")
    ap.add_argument("--snowlark", default=capture.default_snowlark(), help="snowlark-cli.exe")
    ap.add_argument("--chrome", default=CHROME)
    ap.add_argument("--color-scheme", choices=("light", "dark"), default="light",
                    help="the user's preferred color scheme (prefers-color-scheme) in both engines")
    ap.add_argument("--serial", action="store_true", help="run the engines one after the other")
    ap.add_argument("--recompare", action="store_true",
                    help="no capture: compare the snapshots an earlier run left in --out again")
    args = ap.parse_args(argv)
    if not args.url and not args.corpus:
        ap.error("a URL or --corpus is needed")
    if not os.path.exists(args.snowlark):
        ap.error(f"snowlark-cli not found: {args.snowlark} (build with .\\dev.ps1 build --release, or --snowlark)")
    os.makedirs(args.out, exist_ok=True)
    if args.url:
        result = run_one(args.url, args.out, args)
        print(f"{args.url}: {first_line(result)}")
        for w in result.get("warnings", []):
            print("  note:", w)
        print("report:", os.path.abspath(os.path.join(args.out, "report.html")))
        return 0 if result.get("status") == "ok" else 1
    with open(args.corpus, encoding="utf-8") as f:
        urls = [line.split("#", 1)[0].strip() for line in f]
    urls = [u for u in urls if u]
    rows = []
    for url in urls:
        outdir = os.path.join(args.out, slug(url))
        try:
            result = run_one(url, outdir, args)
        except Exception as e:  # one broken site must not end the corpus run
            result = {"status": "error", "meta": {"url": url}, "warnings": [f"{type(e).__name__}: {e}"]}
            os.makedirs(outdir, exist_ok=True)
            report.site_report(os.path.join(outdir, "report.html"), {**result, "meta": {
                "url": url, "width": args.width, "settle_ms": args.settle, "tol": args.tol}})
        print(f"{url} [{result['meta'].get('total_s', '-')} s]: {first_line(result)}", flush=True)
        rows.append(summary_row(url, outdir, result, args.out))
        report.corpus_report(args.out, rows)
    print("summary:", os.path.abspath(os.path.join(args.out, "summary.html")))
    return 0


if __name__ == "__main__":
    sys.exit(main())
