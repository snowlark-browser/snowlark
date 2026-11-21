# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""The feature sweep: what breaks across many sites, ranked by how many sites it breaks.

    python tools/sweep/run.py --out <dir> [--stages styles,supports,layout,scroll,report]

Stages (each reuses what an earlier one left in `<dir>`):
- styles:   every author stylesheet of each corpus site, as headless Chrome ends up with it;
- supports: what those sheets use that Chrome accepts and Snowlark refuses (supports.py);
- layout:   tools/layout-diff over the corpus at each `--widths`, root causes grouped by feature (layout.py);
- scroll:   each site scrolled in Snowlark's window at each `--scales`; pinned rasters, blank rows (scroll.py);
- report:   `<dir>/report.md` and `<dir>/sweep.json` from all of the above.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parent))
sys.path.insert(0, str(HERE.parent / "census"))

import devenv  # noqa: E402
import layout  # noqa: E402
import scroll  # noqa: E402
import supports  # noqa: E402
from cdp import DEFAULT_CHROME  # noqa: E402
from corpus import read_corpus, slug  # noqa: E402

STAGES = ("styles", "supports", "layout", "scroll", "report")


def stage_styles(args, urls):
    from cdp import Chrome

    root = args.out / "styles"
    with Chrome(args.chrome) as chrome:
        for url in urls:
            d = root / slug(url)
            if (d / "index.json").exists() and not args.again:
                continue
            idx = supports.collect_styles(chrome, url, d)
            size = sum(s["bytes"] for s in idx["sheets"])
            print(f"styles {url}: {len(idx['sheets'])} sheets, {size // 1024} KB, {idx['seconds']} s"
                  + (f" ({'; '.join(idx['errors'][:2])})" if idx["errors"] else ""), flush=True)


def stage_supports(args, urls):
    sites = [slug(u) for u in urls]
    u = supports.usage_of(args.out / "styles", sites)
    (args.out / "usage.json").write_text(json.dumps(u.to_json()), encoding="utf-8")
    items = supports.build_items(u)
    print(f"supports: {len(items['decl'])} declarations, {len(items['sel'])} selectors, "
          f"{len(items['media'])} media queries, {len(items['rule'])} rules", flush=True)
    c, s = supports.probe(items, args.chrome, str(args.snowlark), args.out)
    (args.out / "probe.json").write_text(json.dumps({"items": items, "chrome": c, "snowlark": s}),
                                         encoding="utf-8")
    write_supports(args.out, items, c, s, u)


def write_supports(out: Path, items, c, s, u):
    fails = supports.failures(items, c, s, u)
    groups = supports.ranked(supports.blame(fails, supports.pass_counts(items, c, s)))
    gaps = supports.property_gaps(items, s, u)
    (out / "supports.json").write_text(json.dumps({"groups": groups, "property_gaps": gaps,
                                                   "failures": fails}, indent=1), encoding="utf-8")
    print(f"supports: {len(fails)} items fail in {len(groups)} groups; {len(gaps)} of Chrome's "
          f"{len(items['prop'])} properties unknown", flush=True)


def stage_layout(args, urls):
    corpus = args.out / "corpus.txt"
    corpus.write_text("\n".join(urls) + "\n", encoding="utf-8")
    layout.run_widths(corpus, args.out / "layout", args.widths, str(args.snowlark), args.chrome)
    summary = layout.summarize(args.out / "layout", args.widths)
    (args.out / "layout.json").write_text(json.dumps(summary, indent=1), encoding="utf-8")


def stage_scroll(args, urls):
    path = args.out / "scroll.json"
    results = json.loads(path.read_text(encoding="utf-8")) if path.exists() and not args.again else []
    done = {(r["site"], r["scale"]) for r in results}
    logs = args.out / "scroll"
    logs.mkdir(parents=True, exist_ok=True)
    window_exe = str(Path(args.snowlark).with_name("snowlark.exe"))
    for scale in args.scales:
        for url in urls:
            site = slug(url)
            if (site, scale) in done:
                continue
            r = scroll.scroll_site(window_exe, url, scale, logs / f"{site}@{scale}.log", load=args.load,
                                   secs=args.secs)
            r.update(site=site, url=url, scale=scale)
            results.append(r)
            path.write_text(json.dumps(results, indent=1), encoding="utf-8")
            print(f"scroll {site} @{scale}: rasters {r['rasters']}, pinned {r['pinned']}, frames {r['frames']}, "
                  f"uncovered {r['uncovered_frames']} ({r['uncovered_rows']} rows)"
                  + (f" ERROR {r['error']}" if r.get("error") else ""), flush=True)


def stage_report(args, urls):
    lines = [f"# Feature sweep ({time.strftime('%Y-%m-%d')}, {len(urls)} sites)", ""]
    sweep = {}
    p = args.out / "supports.json"
    if p.exists():
        d = json.loads(p.read_text(encoding="utf-8"))
        sweep["supports"] = {"groups": d["groups"], "property_gaps": d["property_gaps"]}
        lines += ["## CSS that Chrome accepts and Snowlark refuses", "",
                  "| feature | sites | items | examples |", "|---|---|---|---|"]
        for g in d["groups"][:args.top]:
            lines.append(f"| `{g['key']}` | {len(g['sites'])}: {_names(g['sites'])} | {g['items']} | "
                         f"{_cell('; '.join(g['examples']))} |")
        lines += ["", "### Properties Chrome knows and Snowlark does not", "",
                  "| property | sites |", "|---|---|"]
        for r in d["property_gaps"][:args.top]:
            lines.append(f"| `{r['property']}` | {len(r['sites'])}: {_names(r['sites'])} |")
        lines.append("")
    p = args.out / "layout.json"
    if p.exists():
        d = json.loads(p.read_text(encoding="utf-8"))
        sweep["layout"] = d
        for w, part in d.items():
            title = "all widths" if w == "all" else f"width {w}"
            lines += [f"## Layout root causes by feature ({title})", ""]
            if "status" in part:
                ok = sum(1 for v in part["status"].values() if v == "ok")
                lines += [f"{ok} of {len(part['status'])} sites compared.", ""]
            lines += ["| group | sites | first difference on | roots | examples |", "|---|---|---|---|---|"]
            for g in part["groups"][:args.top]:
                lines.append(f"| `{g['key']}` | {len(g['sites'])} | {len(g['first'])} | {g['roots']} | "
                             f"{_cell('; '.join(g['examples'][:2]))} |")
            lines.append("")
    p = args.out / "scroll.json"
    if p.exists():
        results = json.loads(p.read_text(encoding="utf-8"))
        sweep["scroll"] = results
        lines += ["## Scrolling in the window", "",
                  "No frames means nothing was drawn while the wheel turned: the document did not scroll (a page "
                  "that scrolls an inner box, or one that did not load).", "",
                  "| site | scale | rasters | pinned | frames | uncovered frames | blank rows share |",
                  "|---|---|---|---|---|---|---|"]
        for r in scroll.rank(results):
            lines.append(f"| {r['site']} | {r['scale']} | {r['rasters']} | {r['pinned']} ({r['pinned_share']:.0%}) | "
                         f"{r['frames']} | {r['uncovered_frames']} | {r['uncovered_share']:.2%} |")
        failed = [r for r in results if r.get("error")]
        if failed:
            lines += ["", "Not measured: " + ", ".join(f"{r['site']} @{r['scale']} ({r['error']})" for r in failed)]
        lines.append("")
    (args.out / "report.md").write_text("\n".join(lines), encoding="utf-8")
    (args.out / "sweep.json").write_text(json.dumps(sweep, indent=1), encoding="utf-8")
    print("report:", args.out / "report.md")


def _names(sites, n=4):
    return ", ".join(sites[:n]) + (", ..." if len(sites) > n else "")


def _cell(text, n=200):
    text = text.replace("|", "\\|").replace("\n", " ")
    return text if len(text) <= n else text[:n] + "…"


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--stages", default=",".join(STAGES))
    ap.add_argument("--corpus", type=Path, default=HERE.parent / "census" / "corpus.txt")
    ap.add_argument("--sites", help="a corpus subset: comma-separated URLs or site names")
    ap.add_argument("--snowlark", type=Path, default=devenv.exe("iter"), help="snowlark-cli.exe")
    ap.add_argument("--chrome", default=DEFAULT_CHROME)
    ap.add_argument("--widths", default="1280,400")
    ap.add_argument("--scales", default="1.0,1.5")
    ap.add_argument("--load", type=float, default=10, help="s a window page loads before scrolling")
    ap.add_argument("--secs", type=float, default=12, help="s of wheel input per site")
    ap.add_argument("--top", type=int, default=40, help="rows per table in the report")
    ap.add_argument("--again", action="store_true", help="redo work an earlier run left in --out")
    args = ap.parse_args(argv)
    args.widths = [int(w) for w in args.widths.split(",")]
    args.scales = [float(s) for s in args.scales.split(",")]
    stages = [s.strip() for s in args.stages.split(",") if s.strip()]
    for s in stages:
        if s not in STAGES:
            ap.error(f"unknown stage {s}")
    urls = read_corpus(args.corpus)
    if args.sites:
        want = {w.strip() for w in args.sites.split(",")}
        urls = [u for u in urls if u in want or slug(u) in want]
    args.out.mkdir(parents=True, exist_ok=True)
    if any(s in stages for s in ("supports", "layout", "scroll")) and not args.snowlark.exists():
        ap.error(f"snowlark-cli not found: {args.snowlark} (build it, or pass --snowlark)")
    for s in STAGES:
        if s in stages:
            globals()[f"stage_{s}"](args, urls)
    return 0


if __name__ == "__main__":
    sys.exit(main())
