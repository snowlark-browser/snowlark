# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""visdiff: content that Chrome shows and Snowlark does not, once the page has settled.

    python tools/visdiff/run.py https://example.com --out <dir>
    python tools/visdiff/run.py --corpus tools/visdiff/corpus.txt --out <dir> [--jobs 6]
    python tools/visdiff/run.py --recompare --out <dir>     # compare the saved snapshots again

See README.md for what is compared and how to read the report.
"""

import argparse
import concurrent.futures
import html
import json
import os
import re
import sys
import time
import urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import findings  # noqa: E402
import settled  # noqa: E402
from chrome import CHROME  # noqa: E402  (tools/layout-diff, on the path through settled)


def slug(url):
    u = urllib.parse.urlparse(url)
    s = (u.netloc + u.path).strip("/") or "page"
    return re.sub(r"[^A-Za-z0-9._-]+", "_", s)[:80]


def read_corpus(path, _seen=None):
    """The URLs of a corpus file, in order and without repeats; "include <file>" reads another list
    (relative to this one) in its place."""
    seen = set() if _seen is None else _seen
    out = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            u = line.split("#", 1)[0].strip()
            if u.startswith("include "):
                out += read_corpus(os.path.join(os.path.dirname(path), u[len("include "):].strip()), seen)
            elif u and u.rstrip("/") not in seen:
                seen.add(u.rstrip("/"))
                out.append(u)
    return out


def load_json(path):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return None


def analyse(url, width, outdir, c_snap, c_info, o_snap, o_info, pixels, screens):
    result = {"url": url, "width": width, "chrome": c_info, "snowlark": o_info, "pixels": pixels,
              "notes": findings.page_notes(c_snap, o_snap)}
    for name, info in (("Chrome", c_info), ("Snowlark", o_info)):
        if info.get("error"):
            result["notes"].append(f"{name}: {info['error']}")
        if info.get("loaded") is False:
            result["notes"].append(f"{name}: no load event within the timeout")
    if c_snap and o_snap:
        result["status"] = "ok"
        result["findings"] = findings.compare(c_snap, o_snap, screens)
    else:
        result["status"] = "error"
        result["findings"] = []
    # Snowlark's own failures are findings too: memory that ran away to the cap, a render that never ended.
    result["memory_gb"] = o_info.get("memory_gb")
    result["over_memory"] = bool(o_info.get("over_memory"))
    result["timed_out"] = o_info.get("exit") == "timeout"
    result["pixel_share"] = findings.pixel_share(pixels, findings.frame_rects(c_snap, o_snap))
    result["score"] = findings.site_score(result["findings"])
    # Pages whose findings are doubtful: a bot check, a consent wall or other content in one engine only.
    result["doubtful"] = result["status"] != "ok" or any(
        "possible" in n or "final URLs differ" in n for n in result["notes"])
    with open(os.path.join(outdir, "result.json"), "w", encoding="utf-8") as f:
        json.dump(result, f, indent=1)
    page_report(os.path.join(outdir, "report.html"), result)
    return result


def run_one(url, width, outdir, args, ua):
    os.makedirs(outdir, exist_ok=True)
    started = time.time()
    old = (load_json(os.path.join(outdir, "result.json")) or {}) if args.recompare else {}
    if args.recompare and not (args.retry_errors and old.get("status") != "ok"):
        c_snap = load_json(os.path.join(outdir, "chrome.json"))
        o_snap = load_json(os.path.join(outdir, "snowlark.json"))
        c_info, o_info, pixels = old.get("chrome", {}), old.get("snowlark", {}), old.get("pixels")
    else:
        cap = settled.capture(url, outdir, width, args, ua)
        c_snap, c_info = cap["chrome"]
        o_snap, o_info = cap["snowlark"]
        pixels = cap["pixels"]
    result = analyse(url, width, outdir, c_snap, c_info, o_snap, o_info, pixels, settled.SCREENS)
    result["seconds"] = round(time.time() - started, 1)
    return result


def first_line(r):
    if r["status"] != "ok":
        return "error: " + "; ".join(r["notes"][:2])
    fs = r["findings"]
    top = fs[0] if fs else None
    px = r.get("pixel_share")
    s = f"score {r['score']:.3f}, pixels {px if px is not None else '-'}"
    if top:
        s += f", top: {top['group']} {top['element']} ({top['share']:.3f}) {top['detail']}"
    else:
        s += ", no hidden content"
    if r["doubtful"]:
        s += " [doubtful]"
    if r.get("over_memory"):
        s += f" [Snowlark's memory reached the cap: {r.get('memory_gb')} GB]"
    return s


# ---------------------------------------------------------------- reports

COLORS = ["#e00", "#e80", "#a0a", "#06c", "#090"]


def page_report(path, r):
    e = html.escape
    rows = []
    for i, f in enumerate(r["findings"][:40]):
        rows.append(
            f"<tr><td>{i + 1}</td><td>{e(f['group'])}</td><td><code>{e(f['element'])}</code></td>"
            f"<td>{f['share']:.3f}</td><td>{f['elements']}</td><td>{e(f['detail'])}</td>"
            f"<td><code>{e(f['path'])}</code></td></tr>")
    boxes = []
    for i, f in enumerate(r["findings"][:5]):
        x0, y0, x1, y1 = f["rect"]
        boxes.append(f"<div class=box style='left:{x0}px;top:{y0}px;width:{x1 - x0}px;height:{y1 - y0}px;"
                     f"border-color:{COLORS[i]}'><span style='background:{COLORS[i]}'>{i + 1}</span></div>")
    w = r["width"]
    h = settled.SCREENS * settled.VIEWPORT_HEIGHT
    pic = ""
    for name in ("chrome", "snowlark"):
        pic += (f"<div class=shot style='width:{w}px;height:{h}px'><div class=cap>{name}</div>"
                f"<img src='{name}.png' width={w}>{''.join(boxes)}</div>")
    px = r.get("pixels") or {}
    pmap = "\n".join(px.get("map", [])) if px.get("map") else ""
    doc = f"""<!doctype html><meta charset=utf-8><title>visdiff {e(r['url'])} {w}</title>
<style>body{{font:13px system-ui;margin:16px}} table{{border-collapse:collapse}} td,th{{border:1px solid #ccc;
padding:3px 6px;vertical-align:top;text-align:left}} .shots{{display:flex;gap:12px;align-items:flex-start}}
.shot{{position:relative;overflow:hidden;outline:1px solid #888;flex:none}} .cap{{position:absolute;z-index:2;
right:0;background:#000;color:#fff;padding:2px 6px}} .box{{position:absolute;border:3px solid}}
.box span{{color:#fff;padding:0 4px;font-weight:bold}} pre{{font:8px/8px monospace}}</style>
<h1>{e(r['url'])} at {w} px</h1>
<p>Score {r['score']:.3f} (share of the first {settled.SCREENS} screens hidden); pixel check: content in Chrome
and background in Snowlark on {r.get('pixel_share')} of the blocks. {'<b>Doubtful.</b>' if r['doubtful'] else ''}</p>
<ul>{''.join('<li>' + e(n) + '</li>' for n in r['notes'])}</ul>
<p>Chrome settled {r['chrome'].get('settled_ms')} ms after load ({r['chrome'].get('seconds')} s in all); Snowlark
{r['snowlark'].get('settled_ms')} ms from the start ({r['snowlark'].get('seconds')} s).</p>
<table><tr><th>#</th><th>group</th><th>where it starts</th><th>share</th><th>elements</th><th>detail</th>
<th>path</th></tr>{''.join(rows)}</table>
<div class=shots>{pic}</div>
<h2>Pixel map</h2><p># content only in Chrome, + in both, - only in Snowlark</p><pre>{e(pmap)}</pre>
"""
    with open(path, "w", encoding="utf-8") as f:
        f.write(doc)


def summary(outroot, results):
    """summary.json and summary.md: groups ranked across pages, then pages ranked by score."""
    sites = [{"url": r["url"], "width": r["width"], "findings": r["findings"], "doubtful": r["doubtful"]}
             for r in results]
    groups = findings.rank_groups(sites)
    ranked = sorted(results, key=lambda r: (r["doubtful"], -r["score"]))
    data = {"pages": len(results), "errors": sum(r["status"] != "ok" for r in results),
            "doubtful": sum(r["doubtful"] for r in results), "groups": groups,
            "sites": [{k: r.get(k) for k in ("url", "width", "status", "score", "pixel_share", "doubtful",
                                             "notes", "seconds", "memory_gb", "over_memory", "timed_out")} | {"top": r["findings"][:5]} for r in ranked]}
    with open(os.path.join(outroot, "summary.json"), "w", encoding="utf-8") as f:
        json.dump(data, f, indent=1)
    lines = [f"# visdiff: {data['pages']} pages, {data['errors']} errors, {data['doubtful']} doubtful", "",
             "## Causes, ranked by hidden share summed over pages (doubtful pages left out)", "",
             "| cause | pages | share | largest |", "|---|---|---|---|"]
    for g in groups:
        t = g["top"][0]
        lines.append(f"| {g['title']} | {g['pages']} | {g['share']:.2f} | {t['url']} ({t['width']}): "
                     f"`{t['element']}` {t['detail']} |")
    for g in groups:
        lines += ["", f"### {g['title']}", ""]
        for t in g["top"]:
            lines.append(f"- {t['share']:.3f} {t['url']} ({t['width']}): `{t['element']}` x{t['elements']}: "
                         f"{t['detail']}")
    failed = [r for r in results if r.get("over_memory") or r.get("timed_out")]
    lines += ["", "## Snowlark's own failures: memory that reached the cap, renders past the time limit", ""]
    for r in sorted(failed, key=lambda r: -(r.get("memory_gb") or 0)):
        what = "memory reached the cap" if r.get("over_memory") else "past the time limit"
        lines.append(f"- {r['url']} ({r['width']}): {what}, peak {r.get('memory_gb')} GB")
    peaks = sorted((r for r in results if r.get("memory_gb")), key=lambda r: -r["memory_gb"])[:10]
    lines += ["", "Highest peak memory: " + ", ".join(f"{r['url']} ({r['width']}) {r['memory_gb']} GB"
                                                     for r in peaks)]
    pixel_only = sorted((r for r in results if not r["doubtful"] and (r.get("pixel_share") or 0) >= 0.05
                         and r["score"] < 0.01), key=lambda r: -r["pixel_share"])
    lines += ["", "## Pixel-only differences (no element finding; frames masked): look at the screenshots", ""]
    for r in pixel_only:
        rep = os.path.join(slug(r["url"]), str(r["width"]), "report.html").replace("\\", "/")
        lines.append(f"- {r['pixel_share']:.3f} [{r['url']}]({rep}) ({r['width']})")
    lines += ["", "## Pages by score", "", "| page | width | score | pixels | top finding | notes |",
              "|---|---|---|---|---|---|"]
    for r in ranked:
        top = r["findings"][0] if r["findings"] else None
        tf = f"{top['group']}: `{top['element']}` {top['detail']}" if top else ""
        rep = os.path.join(slug(r["url"]), str(r["width"]), "report.html").replace("\\", "/")
        lines.append(f"| [{r['url']}]({rep}) | {r['width']} | {r['score']:.3f} | {r.get('pixel_share')} | "
                     f"{tf.replace('|', '/')} | {'; '.join(r['notes'])[:200].replace('|', '/')} |")
    with open(os.path.join(outroot, "summary.md"), "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")
    return data


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("url", nargs="?")
    ap.add_argument("--corpus", help="file with one URL per line (# comments)")
    ap.add_argument("--out", default="visdiff-out")
    ap.add_argument("--widths", default="1280,390", help="CSS widths, comma-separated (default 1280,390)")
    ap.add_argument("--settle", type=int, default=10000,
                    help="ms after the load event to wait at most for the page to settle (default 10000)")
    ap.add_argument("--timeout", type=int, default=30000, help="ms to wait for the load event (default 30000)")
    ap.add_argument("--max-nodes", type=int, default=20000)
    ap.add_argument("--jobs", type=int, default=6, help="pages captured at the same time (default 6)")
    ap.add_argument("--max-memory-gb", type=float, default=settled.DEFAULT_MEMORY_GB,
                    help="memory cap per Snowlark render, in GB (default %(default)s; 0: none)")
    ap.add_argument("--snowlark", default=settled.default_snowlark(), help="snowlark-cli.exe")
    ap.add_argument("--chrome", default=CHROME)
    ap.add_argument("--recompare", action="store_true",
                    help="no capture: compare the snapshots an earlier run left in --out again")
    ap.add_argument("--retry-errors", action="store_true",
                    help="with --recompare: capture again the pages whose capture failed")
    args = ap.parse_args(argv)
    widths = [int(w) for w in args.widths.split(",") if w.strip()]
    if args.url:
        urls = [args.url]
    elif args.corpus:
        urls = read_corpus(args.corpus)
    elif args.recompare:
        old = load_json(os.path.join(args.out, "summary.json")) or {"sites": []}
        urls = list(dict.fromkeys(s["url"] for s in old["sites"]))
    else:
        ap.error("a URL, --corpus or --recompare is needed")
    capturing = not args.recompare or args.retry_errors
    if capturing and not os.path.exists(args.snowlark):
        ap.error(f"snowlark-cli not found: {args.snowlark} (build it, or pass --snowlark)")
    os.makedirs(args.out, exist_ok=True)
    ua = settled.snowlark_user_agent(args.snowlark) if capturing else None
    jobs = [(u, w) for u in urls for w in widths]
    results = []
    started = time.time()

    def job(uw):
        url, width = uw
        outdir = os.path.join(args.out, slug(url), str(width))
        try:
            return run_one(url, width, outdir, args, ua)
        except Exception as e:  # one broken page must not end the run
            os.makedirs(outdir, exist_ok=True)
            return analyse(url, width, outdir, None, {"error": f"{type(e).__name__}: {e}"}, None, {}, None,
                           settled.SCREENS)

    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        futures = [pool.submit(job, uw) for uw in jobs]
        for n, fut in enumerate(concurrent.futures.as_completed(futures), 1):
            r = fut.result()
            results.append(r)
            print(f"[{n}/{len(jobs)}] {r['url']} {r['width']}: {first_line(r)}", flush=True)
    data = summary(args.out, results)
    print(f"{len(jobs)} pages in {time.time() - started:.0f} s; {data['errors']} errors, {data['doubtful']} doubtful")
    print("summary:", os.path.abspath(os.path.join(args.out, "summary.md")))
    return 0


if __name__ == "__main__":
    sys.exit(main())
