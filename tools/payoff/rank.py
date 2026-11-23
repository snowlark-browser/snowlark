# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Ranks spec areas and features by measured payoff.

Reads what the measuring tools left behind and writes one ranked list:

- `tools/census` (its `--out` folder): web APIs Snowlark lacks, by the corpus sites whose scripts use them and
  the sites where Snowlark threw naming them; blank or sparse renders, hangs and uncaught exceptions per site.
- `tools/sweep` (its `sweep.json`): CSS Chrome accepts and Snowlark refuses, layout-diff root causes at two
  widths, pinned rasters and blank rows while scrolling.
- a WPT results file (`.jsonl`, one test file per line, as `tools/wpt/run.py` writes and keeps as baselines):
  failing subtests per directory.
- `areas.json`: which spec area or feature each finding belongs to, the area's WPT directories and its size.
- optionally a reports file: issues users reported, each attached to an area or standing as an item of its own.

Every finding becomes a signal (a key, a site, a weight); signals whose key an area matches count for that area,
the others become an item of their own (a missing API, a refused CSS feature) or, where no feature can be named
(a blank render nothing explains, an exception no area claims), a row of the unattributed table. An item's score:

    (SITE * sum over its sites of the strongest signal there + USER * user reports + OTHER * other reports
     + WPT * min(failing subtests, WPT_CAP) / WPT_CAP) / size

with size S = 1, M = 2, L = 4. The weights are at the top of `areas.json`.

    python tools/payoff/rank.py --census <dir> --sweep <dir>/sweep.json --wpt <results.jsonl> --out <dir>

writes `<out>/payoff.md` and `<out>/payoff.json`. Standard library only.
"""

from __future__ import annotations

import argparse
import json
import math
import re
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import urlparse

HERE = Path(__file__).resolve().parent
TOOLS = HERE.parent

SIZES = {"S": 1, "M": 2, "L": 4}

# The default weight of each kind of signal: what one site counts for an item.
DEFAULT_WEIGHTS = {
    "site": 10,         # points per site-weight unit
    "user_report": 15,  # points per issue a user reported
    "other_report": 3,  # points per issue found otherwise (tests, tools)
    "wpt": 10,          # points for WPT_CAP failing subtests
    "wpt_cap": 2000,
    "broken": 3.0,      # a blank, sparse or hung render this item explains
    "throws": 2.0,      # Snowlark threw naming it
    "exception": 1.0,   # an uncaught exception an area claims
    "layout_first": 0.6,  # the site's first layout difference (its size is unknown: often a pixel)
    "layout": 0.2,      # a layout difference further down
    "scroll": 1.0,      # pinned rasters or blank rows while scrolling
    "css": 0.5,         # a rule or declaration the site uses is dropped
    "mention": 0.1,     # the site's scripts mention a missing API
}


@dataclass
class Signal:
    key: str      # "api: X", "css: prop:x", "layout: size: flex item (row)", "scroll: pinned", "exception: ..."
    site: str
    weight: float
    detail: str = ""


@dataclass
class Item:
    id: str
    title: str
    size: str = "S"
    wpt: list[str] = field(default_factory=list)
    spec: str = ""
    sites: dict[str, float] = field(default_factory=dict)
    evidence: dict[str, set[str]] = field(default_factory=dict)  # signal key -> sites
    reports: list[dict] = field(default_factory=list)
    wpt_failing: int = 0
    wpt_total: int = 0
    score: float = 0.0

    def add(self, s: Signal) -> None:
        self.sites[s.site] = max(self.sites.get(s.site, 0.0), s.weight)
        self.evidence.setdefault(s.key, set()).add(s.site)


# ----- sites ------------------------------------------------------------------------------------------

def site_name(name_or_url: str) -> str:
    """One name per site across the tools: the host without "www.", plus the path if any
    ("www.apple.com" and "apple.com" are "apple.com"; "google.com_maps" stays)."""
    s = name_or_url
    if "://" in s:
        u = urlparse(s)
        s = (u.hostname or "") + (("_" + u.path.strip("/").replace("/", "_")) if u.path.strip("/") else "")
    return s.removeprefix("www.")


# ----- census -----------------------------------------------------------------------------------------

def _read(path: Path, default=None):
    return json.loads(path.read_text(encoding="utf-8")) if path.exists() else default


def census_signals(rows: list[dict], runtime: dict, verdicts: dict, chrome_exceptions: dict,
                   w: dict) -> tuple[list[Signal], list[dict]]:
    """Signals from the census: `rows` as the census report's `missing_rows`, `runtime` its per-site
    results, `verdicts` site -> BLANK / sparse / hung / crashed / "", `chrome_exceptions` site -> how many
    exceptions Chrome threw on the page. Returns the signals and the per-site breakage the missing APIs
    do not explain (the unattributed rows)."""
    out: list[Signal] = []
    explained: dict[str, list[str]] = {}
    # One failing name can match several missing interfaces (a read of `length` off undefined matches every
    # missing list interface): a site's failure is shared among the rows it matches, not counted in full by each.
    claims: dict[str, int] = {}
    for r in rows:
        if not r.get("ecmascript"):
            for site in r.get("runtime", ()):
                claims[site] = claims.get(site, 0) + 1
    for r in rows:
        if r.get("ecmascript"):
            key = "ecmascript: " + r["key"]
        elif (r.get("shared") or r.get("generic")) and not r.get("runtime"):
            continue  # counted by a bare name another interface has: not evidence
        else:
            key = "api: " + r["key"]
        failed = set(r.get("runtime", ()))
        for site in r.get("sites", ()):
            if site not in failed:
                out.append(Signal(key, site_name(site), w["mention"]))
        for site in failed:
            broken = verdicts.get(site, "") in ("BLANK", "sparse", "hung")
            n = max(1, claims.get(site, 1))
            out.append(Signal(key, site_name(site), (w["broken"] if broken else w["throws"]) / n,
                              "threw" + (f", {verdicts[site]}" if broken else "") + (f", 1/{n}" if n > 1 else "")))
            explained.setdefault(site, []).append(r["key"])
    unattributed = []
    for site, r in sorted(runtime.items()):
        exc = r.get("exceptions", [])
        extra = len(exc) - chrome_exceptions.get(site, 0)
        for e in exc:
            text = normalize_exception(e.get("text", ""))
            if text:
                out.append(Signal("exception: " + text, site_name(site), w["exception"],
                                  e.get("source", "")))
        v = verdicts.get(site, "")
        if v or extra > 0:
            unattributed.append({
                "site": site_name(site), "verdict": v, "exceptions": len(exc), "chrome_exceptions":
                chrome_exceptions.get(site, 0), "explained_by": sorted(explained.get(site, [])),
                "first": normalize_exception(exc[0].get("text", "")) if exc else ""})
    return out, unattributed


def normalize_exception(text: str) -> str:
    """An exception message without what varies between pages: quoted strings, numbers, URLs."""
    t = re.sub(r"^(Uncaught( \(in promise\))?:?\s*)", "", text.strip())
    t = re.sub(r"https?://\S+", "<url>", t)
    t = re.sub(r"\b\d+(\.\d+)?\b", "<n>", t)
    return t[:160]


def load_census(folder: Path, w: dict) -> tuple[list[Signal], list[dict], dict]:
    sys.path.insert(0, str(TOOLS / "census"))
    import report  # noqa: E402 (the census's own report code decides what counts as missing)
    import runtime as census_runtime  # noqa: E402
    from surface import member_name  # noqa: E402

    surfaces = {"chrome": _read(folder / "surface-chrome.json"),
                "snowlark": _read(folder / "surface-snowlark.json")}
    diff = _read(folder / "diff.json", {})
    use = _read(folder / "usage.json", {})
    rendered, fetched = {}, {}
    for d in sorted((folder / "sites").iterdir()) if (folder / "sites").exists() else []:
        if (d / "snowlark.json").exists():
            rendered[d.name] = _read(d / "snowlark.json")
        if (d / "index.json").exists():
            fetched[d.name] = _read(d / "index.json")
            png = d / "chrome.png"
            if png.exists():
                fetched[d.name]["chrome_image"] = census_runtime.analyze_png(png)
    names = {member_name(k) for scope in ("window", "worker") if (surfaces["snowlark"] or {}).get(scope)
             for k in surfaces["snowlark"][scope]["members"]}
    rows = report.missing_rows(surfaces, diff, use, names, rendered) if surfaces["chrome"] else []
    verdicts, chrome_exc = {}, {}
    for site, r in rendered.items():
        v = report.capture_verdict(r.get("image") or {}, fetched.get(site, {}).get("chrome_image") or {})
        v = v.strip("*") if v not in ("no", "?") else ""
        if r.get("killed"):
            v = "hung"
        elif r.get("settle_ms") is None:
            v = "crashed"
        verdicts[site] = v
        chrome_exc[site] = len(fetched.get(site, {}).get("chrome_exceptions", []))
    signals, unattributed = census_signals(rows, rendered, verdicts, chrome_exc, w)
    meta = _read(folder / "meta.json", {})
    return signals, unattributed, {"sites": sorted(site_name(s) for s in rendered), "date": meta.get("date"),
                                   "commit": meta.get("snowlark_commit")}


# ----- sweep ------------------------------------------------------------------------------------------

def sweep_signals(sweep: dict, w: dict) -> tuple[list[Signal], list[dict]]:
    """Signals from the sweep's `sweep.json`: refused CSS, layout root causes (the first one a site has
    weighs more), scrolling. `content:` layout groups (an element only one engine has: mostly scripts
    taking another path) go to the unattributed rows."""
    out: list[Signal] = []
    unattributed = []
    for g in sweep.get("supports", {}).get("groups", []):
        ex = "; ".join(g.get("examples", [])[:2])
        for site in g.get("sites", []):
            out.append(Signal("css: " + g["key"], site_name(site), w["css"], ex))
    for g in sweep.get("layout", {}).get("all", {}).get("groups", []):
        if g["key"].startswith("content:"):
            unattributed.append({"key": "layout: " + g["key"], "sites": sorted(map(site_name, g["sites"]))})
            continue
        first = {site_name(s) for s in g.get("first", [])}
        for site in g.get("sites", []):
            s = site_name(site)
            out.append(Signal("layout: " + g["key"], s, w["layout_first"] if s in first else w["layout"]))
    for r in sweep.get("scroll", []):
        s = site_name(r.get("site") or r.get("url", ""))
        if r.get("pinned"):
            out.append(Signal("scroll: pinned rasters", s, w["scroll"], f"scale {r.get('scale')}: "
                              f"{r.get('pinned_example', '')}"))
        if r.get("uncovered_frames"):
            out.append(Signal("scroll: blank rows", s, w["scroll"], f"scale {r.get('scale')}"))
    return out, unattributed


# ----- WPT --------------------------------------------------------------------------------------------

def wpt_path(url: str) -> str:
    """"http://web-platform.test:8000/css/css-grid/x.html?y" -> "css/css-grid/x.html"."""
    return urlparse(url).path.lstrip("/")


def load_wpt(lines) -> dict[str, tuple[int, int]]:
    """Per test file: (failing subtests, subtests). A file whose harness failed (error, timeout) with no
    subtests counts as one failing subtest."""
    files = {}
    for line in lines:
        line = line.strip()
        if not line:
            continue
        rec = json.loads(line)
        res = rec.get("result") or {}
        tests = res.get("tests") or []
        if tests:
            fail = sum(1 for t in tests if t.get("status") != 0)
            files[wpt_path(rec["url"])] = (fail, len(tests))
        else:
            files[wpt_path(rec["url"])] = (0 if res.get("status") == 0 else 1, 1)
    return files


def wpt_count(files: dict[str, tuple[int, int]], prefixes: list[str]) -> tuple[int, int]:
    """Failing and total subtests in the files under any of `prefixes` (each file counted once); a prefix
    that starts with "!" leaves its files out ("shadow-dom/", "!shadow-dom/reference-target/")."""
    fail = total = 0
    take = [p for p in prefixes if not p.startswith("!")]
    leave = [p[1:] for p in prefixes if p.startswith("!")]
    for path, (f, t) in files.items():
        if any(path.startswith(p) for p in take) and not any(path.startswith(p) for p in leave):
            fail += f
            total += t
    return fail, total


def wpt_directories(files: dict[str, tuple[int, int]], depth: int = 2) -> list[tuple[str, int, int]]:
    """(directory, failing, total) at `depth` path segments, most failing first."""
    agg: dict[str, list[int]] = {}
    for path, (f, t) in files.items():
        parts = path.split("/")
        d = "/".join(parts[:min(depth, len(parts) - 1)]) + "/"
        a = agg.setdefault(d, [0, 0])
        a[0] += f
        a[1] += t
    return sorted(((d, f, t) for d, (f, t) in agg.items()), key=lambda x: (-x[1], x[0]))


# ----- ranking ----------------------------------------------------------------------------------------

def auto_item(key: str) -> Item:
    """The item of a signal no area claims: the feature its key names."""
    kind, _, rest = key.partition(": ")
    titles = {"api": "Missing API: ", "ecmascript": "JavaScript built-in (V8 version): ", "css": "CSS: ",
              "layout": "Layout: ", "scroll": "Scrolling: ", "exception": "Uncaught: "}
    return Item(id=key, title=titles.get(kind, "") + rest, size="S")


def rank(signals: list[Signal], areas: list[dict], reports: list[dict], wpt: dict | None,
         w: dict, ignore: list[str] = ()) -> tuple[list[Item], list[Signal]]:
    """Folds signals and reports into items and scores them. Signals whose key an `ignore` expression
    matches (non-standard or obsolete members no spec asks for) are dropped. Exceptions no area claims are
    not items (one message is not a feature): they are returned for the unattributed table."""
    drop = [re.compile(p) for p in ignore]
    signals = [s for s in signals if not any(p.search(s.key) for p in drop)]
    items: dict[str, Item] = {}
    compiled = []
    for a in areas:
        items[a["id"]] = Item(id=a["id"], title=a["title"], size=a.get("size", "M"), wpt=a.get("wpt", []),
                              spec=a.get("spec", ""))
        compiled.append((a["id"], [re.compile(p) for p in a.get("match", [])]))
    unclaimed: list[Signal] = []
    for s in signals:
        owner = next((aid for aid, pats in compiled if any(p.search(s.key) for p in pats)), None)
        if owner is None:
            if s.key.startswith("exception: "):
                unclaimed.append(s)
                continue
            item = items.get(s.key) or items.setdefault(s.key, auto_item(s.key))
        else:
            item = items[owner]
        item.add(s)
    for r in reports:
        if r.get("area") and r["area"] in items:
            items[r["area"]].reports.append(r)
        else:
            rid = "report:" + str(r["id"])
            items[rid] = Item(id=rid, title=r.get("title", rid), size=r.get("size", "M"),
                              wpt=r.get("wpt", []), reports=[r])
    for item in items.values():
        if wpt is not None and item.wpt:
            item.wpt_failing, item.wpt_total = wpt_count(wpt, item.wpt)
        item.score = score(item, w)
    ranked = sorted((i for i in items.values() if i.score > 0),
                    key=lambda i: (-i.score, -len(i.sites), i.id))
    return ranked, unclaimed


def score(item: Item, w: dict) -> float:
    user = sum(1 for r in item.reports if r.get("user"))
    other = len(item.reports) - user
    wpt = min(item.wpt_failing, w["wpt_cap"]) / w["wpt_cap"]
    raw = (w["site"] * sum(item.sites.values()) + w["user_report"] * user + w["other_report"] * other
           + w["wpt"] * wpt)
    return round(raw / SIZES.get(item.size, 2), 2)


# ----- output -----------------------------------------------------------------------------------------

def _md(text) -> str:
    return str(text).replace("|", "\\|").replace("\n", " ")


def item_json(i: Item) -> dict:
    return {"id": i.id, "title": i.title, "score": i.score, "size": i.size, "spec": i.spec,
            "sites": sorted(i.sites), "site_weights": {s: round(v, 2) for s, v in sorted(i.sites.items())},
            "reports": [r["id"] for r in i.reports], "user_reports": [r["id"] for r in i.reports if r.get("user")],
            "wpt": i.wpt, "wpt_failing": i.wpt_failing, "wpt_total": i.wpt_total,
            "evidence": {k: sorted(v) for k, v in sorted(i.evidence.items(), key=lambda kv: -len(kv[1]))}}


def render(ranked: list[Item], unattributed: list[dict], unclaimed: list[Signal], meta: dict,
           top: int = 60) -> str:
    L = []
    w = L.append
    w("# Payoff ranking")
    w("")
    w(f"Generated {meta['date']} by `tools/payoff/rank.py` in {meta['seconds']} s from: census "
      f"{meta.get('census_date', '?')} ({meta.get('census_sites', 0)} sites, Snowlark "
      f"{meta.get('census_commit', '?')}), sweep {meta.get('sweep', '-')}, WPT {meta.get('wpt', '-')}. "
      "Score = (sites weighted by their strongest signal + reports + failing WPT subtests) / size; see "
      "README.md.")
    w("")
    w("| # | item | score | sites | reports | WPT failing | size | strongest evidence |")
    w("|---|---|---|---|---|---|---|---|")
    for n, i in enumerate(ranked[:top], 1):
        ev = sorted(i.evidence.items(), key=lambda kv: -len(kv[1]))[:3]
        evs = "; ".join(f"`{_md(k)}` {len(v)}" for k, v in ev)
        sites = f"{len(i.sites)}: " + ", ".join(sorted(i.sites, key=lambda s: -i.sites[s])[:5]) + (
            " ..." if len(i.sites) > 5 else "") if i.sites else "0"
        reps = ", ".join(str(r["id"]) + ("*" if r.get("user") else "") for r in i.reports)
        wp = f"{i.wpt_failing}/{i.wpt_total}" if i.wpt else ""
        w(f"| {n} | {_md(i.title)} | {i.score} | {_md(sites)} | {reps} | {wp} | {i.size} | {evs} |")
    if len(ranked) > top:
        w("")
        w(f"{len(ranked) - top} more items in payoff.json.")
    w("")
    w("Reports marked * were reported by users.")
    w("")
    w("## Unattributed breakage")
    w("")
    w("Sites that render blank or sparse, hang, crash or throw more than Chrome on the same page, with what "
      "the census could tie to it. A row nothing explains needs locating (layout-diff, the console) before it "
      "can join an area.")
    w("")
    w("| site | render | exceptions (Chrome) | explained by | first exception |")
    w("|---|---|---|---|---|")
    for u in [u for u in unattributed if "site" in u]:
        w(f"| {u['site']} | {u['verdict'] or 'ok'} | {u['exceptions']} ({u['chrome_exceptions']}) | "
          f"{_md(', '.join(u['explained_by']))} | {_md(u['first'][:120])} |")
    w("")
    groups = [u for u in unattributed if "key" in u]
    if groups:
        w("Layout groups of elements only one engine has (scripts taking another path): " + ", ".join(
            f"`{_md(g['key'])}` {len(g['sites'])}" for g in groups[:30]))
        w("")
    by_msg: dict[str, set[str]] = {}
    for s in unclaimed:
        by_msg.setdefault(s.key, set()).add(s.site)
    if by_msg:
        w("Uncaught exceptions no area claims, by sites:")
        w("")
        for k, v in sorted(by_msg.items(), key=lambda kv: (-len(kv[1]), kv[0]))[:40]:
            w(f"- {len(v)}: `{_md(k[len('exception: '):][:140])}` ({', '.join(sorted(v)[:4])})")
        w("")
    return "\n".join(L) + "\n"


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--census", type=Path, help="tools/census --out folder")
    ap.add_argument("--sweep", type=Path, help="tools/sweep sweep.json")
    ap.add_argument("--wpt", type=Path, help="WPT results .jsonl (a baseline)")
    ap.add_argument("--areas", type=Path, default=HERE / "areas.json")
    ap.add_argument("--reports", type=Path, help="reported issues (JSON), optional")
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--top", type=int, default=60)
    args = ap.parse_args(argv)

    t0 = time.time()
    cfg = _read(args.areas)
    w = dict(DEFAULT_WEIGHTS, **cfg.get("weights", {}))
    signals, unattributed = [], []
    meta = {"date": time.strftime("%Y-%m-%d")}
    if args.census:
        s, u, m = load_census(args.census, w)
        signals += s
        unattributed += u
        meta.update(census_date=m["date"], census_sites=len(m["sites"]), census_commit=m["commit"])
    if args.sweep:
        s, u = sweep_signals(_read(args.sweep), w)
        signals += s
        unattributed += u
        meta["sweep"] = time.strftime("%Y-%m-%d", time.localtime(args.sweep.stat().st_mtime))
    wpt = None
    if args.wpt:
        with args.wpt.open(encoding="utf-8") as f:
            wpt = load_wpt(f)
        meta["wpt"] = f"{args.wpt.stem} ({len(wpt)} files)"
    reports = _read(args.reports, {}).get("reports", []) if args.reports else []
    ranked, unclaimed = rank(signals, cfg.get("areas", []), reports, wpt, w, cfg.get("ignore", []))
    meta["seconds"] = round(time.time() - t0)
    args.out.mkdir(parents=True, exist_ok=True)
    (args.out / "payoff.md").write_text(render(ranked, unattributed, unclaimed, meta, args.top),
                                        encoding="utf-8", newline="\n")
    doc = {"meta": meta, "weights": w, "items": [item_json(i) for i in ranked],
           "unattributed": unattributed,
           "unclaimed_exceptions": sorted({s.key for s in unclaimed}),
           "wpt_directories": [list(x) for x in wpt_directories(wpt)[:200]] if wpt else []}
    (args.out / "payoff.json").write_text(json.dumps(doc, indent=1), encoding="utf-8", newline="\n")
    print(f"payoff: {len(ranked)} items, {meta['seconds']} s -> {args.out / 'payoff.md'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
