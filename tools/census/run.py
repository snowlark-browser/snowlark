# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Missing-API census: what Chrome exposes that Snowlark lacks, ranked by how many real sites use it,
plus what actually breaks when Snowlark renders those sites.

    python tools/census/run.py --out <dir> [--snowlark target/release/snowlark-cli.exe]
        [--chrome <chrome.exe>] [--corpus tools/census/corpus.txt] [--stages surface,fetch,runtime]
        [--jobs 4] [--timeout 60] [--report tools/census/report.md]

Stages (each writes JSON under <dir>; a stage left out reuses what an earlier run wrote there):
  surface   enumerate Chrome's and Snowlark's window and worker surfaces; diff them
  fetch     load each corpus site in headless Chrome, save its scripts (<dir>/sites/<site>/)
  runtime   render each corpus site in Snowlark headless; exceptions, blankness, settle time
The report (usage counts from the saved scripts, and the tables) is always rebuilt.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import datetime
import json
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import corpus  # noqa: E402
import report  # noqa: E402
import runtime  # noqa: E402
import surface  # noqa: E402
from cdp import DEFAULT_CHROME, Chrome  # noqa: E402

REPO = HERE.parent.parent


def log(msg: str) -> None:
    print(f"[census {time.strftime('%H:%M:%S')}] {msg}", flush=True)


def read_json(path: Path, default=None):
    return json.loads(path.read_text(encoding="utf-8")) if path.exists() else default


def write_json(path: Path, value) -> None:
    path.write_text(json.dumps(value, indent=1, sort_keys=True), encoding="utf-8")


def stage_surface(args, out: Path, meta: dict) -> None:
    with surface.SurfaceServer() as server:
        log("surface: Chrome")
        with Chrome(args.chrome) as chrome:
            chrome_s = surface.chrome_surface(chrome, server)
            meta["chrome"] = f"Chrome {chrome.major}"
        log("surface: Snowlark")
        snowlark_s = surface.snowlark_surface(args.snowlark, server, out)
    write_json(out / "surface-chrome.json", chrome_s)
    write_json(out / "surface-snowlark.json", snowlark_s)
    for name, s in (("chrome", chrome_s), ("snowlark", snowlark_s)):
        if s.get("workerError"):
            log(f"surface: {name} worker: {s['workerError']}")
            meta[f"{name}_worker_error"] = s["workerError"]


def stage_fetch(args, out: Path, urls: list[str], meta: dict) -> None:
    sites = out / "sites"
    with Chrome(args.chrome) as chrome:
        meta["chrome"] = f"Chrome {chrome.major}"
        for url in urls:
            name = corpus.slug(url)
            t = time.time()
            index = corpus.fetch_site(chrome, url, sites / name, timeout=args.fetch_timeout)
            kb = sum(r["bytes"] for r in index["resources"]) // 1024
            log(f"fetch: {name}: {len(index['resources'])} scripts/documents, {kb} KiB, "
                f"{time.time() - t:.0f} s{' (no load event)' if not index.get('loaded') else ''}")


def stage_runtime(args, out: Path, urls: list[str]) -> None:
    sites = out / "sites"

    def one(url):
        name = corpus.slug(url)
        r = runtime.render_site(args.snowlark, url, sites / name, args.timeout)
        state = "hung" if r["killed"] else ("no settle" if r["timeout"] else f"{r['settle_ms']} ms")
        img = r.get("image") or {}
        log(f"runtime: {name}: {state}, {len(r['exceptions'])} exceptions, "
            f"{'BLANK' if img.get('blank') else 'ink ' + str(img.get('ink'))}")
        return name, r

    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        list(pool.map(one, urls))


def collect(out: Path, urls: list[str]) -> tuple[dict, dict]:
    fetched, rendered = {}, {}
    for url in urls:
        d = out / "sites" / corpus.slug(url)
        if (d / "index.json").exists():
            fetched[d.name] = read_json(d / "index.json")
            if (d / "chrome.png").exists():
                fetched[d.name]["chrome_image"] = runtime.analyze_png(d / "chrome.png")
        if (d / "snowlark.json").exists():
            rendered[d.name] = read_json(d / "snowlark.json")
    return fetched, rendered


def snowlark_commit() -> str:
    try:
        return subprocess.run(["git", "rev-parse", "--short", "HEAD"], cwd=REPO, capture_output=True,
                              text=True, check=True).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return "?"


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    p.add_argument("--out", required=True, type=Path)
    p.add_argument("--snowlark", default=str(REPO / "target" / "release" / "snowlark-cli.exe"))
    p.add_argument("--chrome", default=DEFAULT_CHROME)
    p.add_argument("--corpus", type=Path, default=HERE / "corpus.txt")
    p.add_argument("--ignore", type=Path, default=HERE / "ignore.txt")
    p.add_argument("--stages", default="surface,fetch,runtime")
    p.add_argument("--jobs", type=int, default=4, help="Snowlark renders at once")
    p.add_argument("--timeout", type=int, default=60, help="seconds per Snowlark render")
    p.add_argument("--fetch-timeout", type=float, default=45, help="seconds per Chrome page load")
    p.add_argument("--report", type=Path, default=HERE / "report.md")
    p.add_argument("--sites", help="comma-separated corpus subset (slugs or URLs)")
    args = p.parse_args()

    started = time.time()
    out: Path = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    all_urls = corpus.read_corpus(args.corpus)
    urls = all_urls
    if args.sites:
        want = set(args.sites.split(","))
        urls = [u for u in urls if u in want or corpus.slug(u) in want]
    stages = set(filter(None, args.stages.split(",")))
    meta = read_json(out / "meta.json", {})
    meta["date"] = datetime.date.today().isoformat()
    meta["snowlark_commit"] = snowlark_commit()
    stage_times = meta.setdefault("stage_seconds", {})

    for stage, fn in (("surface", lambda: stage_surface(args, out, meta)),
                      ("fetch", lambda: stage_fetch(args, out, urls, meta)),
                      ("runtime", lambda: stage_runtime(args, out, urls))):
        if stage in stages:
            t = time.time()
            fn()
            stage_times[stage] = round(time.time() - t)
            write_json(out / "meta.json", meta)

    surfaces = {"chrome": read_json(out / "surface-chrome.json"),
                "snowlark": read_json(out / "surface-snowlark.json")}
    if not surfaces["chrome"] or not surfaces["snowlark"]:
        log("no surfaces yet: run the surface stage")
        return 1
    diff = surface.diff_surfaces(surfaces["chrome"], surfaces["snowlark"],
                                 surface.load_ignore(args.ignore))
    write_json(out / "diff.json", diff)

    log("usage: tokenizing the saved scripts")
    t = time.time()
    # The report covers the whole corpus, whatever subset this run's stages touched.
    fetched, rendered = collect(out, all_urls)
    tokens = {name: corpus.site_tokens(out / "sites" / name) for name in fetched}
    use = {}
    for scope in ("window", "worker"):
        s = surfaces["chrome"].get(scope)
        use[scope] = corpus.usage(s["members"], tokens) if s else {}
    write_json(out / "usage.json", use)
    stage_times["usage"] = round(time.time() - t)

    meta["run_minutes"] = round(sum(stage_times.values()) / 60, 1)
    write_json(out / "meta.json", meta)
    text = report.render(meta, surfaces, diff, use, rendered, fetched)
    args.report.write_text(text, encoding="utf-8", newline="\n")
    log(f"report: {args.report} ({time.time() - started:.0f} s this run)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
