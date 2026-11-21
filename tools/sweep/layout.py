# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Part 2: layout-diff over the corpus, its root causes grouped by the CSS feature of the box.

`run_widths` runs `tools/layout-diff/run.py --corpus` once per width. `group` reads every site's
`result.json` and files each root cause under one key:
- `missing`/`extra` elements under `content: missing <tag>` (scripts, ads and consent walls swing these);
- a root whose computed values differ under `value: <property>` (the value is the likely cause);
- text-level kinds (`text-wrap`, `text-height`, `text-width`, `accumulated`, `inline-box`, `baseline`) under
  `text: <kind>`;
- the rest under `<kind>: <layout context>`, the most specific of subgrid, grid item/container, flex
  item/container, container queries, position, float, table, transform, aspect-ratio, overflow,
  inline-block, block flow.
Keys are ranked by the number of sites where they occur among the roots, and by how many sites have one as
their first difference.
"""

from __future__ import annotations

import json
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
LAYOUT_DIFF = HERE.parent / "layout-diff" / "run.py"
TEXT_KINDS = ("text-wrap", "text-height", "text-width", "accumulated", "inline-box", "baseline")


def run_widths(corpus: Path, out: Path, widths: list[int], snowlark: str, chrome: str | None = None,
               extra: list[str] | None = None) -> None:
    for w in widths:
        cmd = [sys.executable, str(LAYOUT_DIFF), "--corpus", str(corpus), "--out", str(out / f"w{w}"),
               "--width", str(w), "--snowlark", snowlark, *(extra or [])]
        if chrome:
            cmd += ["--chrome", chrome]
        print("layout:", " ".join(cmd[1:3]), "width", w, flush=True)
        subprocess.run(cmd, check=False)


def _styles(item: dict) -> dict[str, str]:
    """The element's and its parent's computed values (Chrome's where it has them)."""
    out = {}
    for row in item.get("styles", []):
        prop, va, vb = row[0], row[1], row[2]
        v = va if va is not None else vb
        if v is not None:
            out[prop] = v
    return out


def context(item: dict) -> str:
    """The most specific layout feature the box takes part in."""
    s = _styles(item)
    d, pd = s.get("display", ""), s.get("parent:display", "")
    tracks = " ".join(s.get(k, "") for k in ("grid-template-columns", "grid-template-rows",
                                             "parent:grid-template-columns", "parent:grid-template-rows"))
    if "subgrid" in tracks:
        return "subgrid"
    if pd.endswith("grid"):
        return "grid item"
    if d.endswith("grid"):
        return "grid container"
    if pd.endswith("flex"):
        direction = s.get("parent:flex-direction", "row")
        return f"flex item ({'column' if direction.startswith('column') else 'row'})"
    if d.endswith("flex"):
        return "flex container"
    if s.get("container-type", "normal") != "normal" or s.get("parent:container-type", "normal") != "normal":
        return "container queries"
    pos = s.get("position", "static")
    if pos in ("sticky", "fixed", "absolute"):
        return f"position: {pos}"
    if s.get("float", "none") != "none":
        return "float"
    if d.startswith("table") or d == "inline-table" or pd.startswith("table"):
        return "table"
    if s.get("transform", "none") != "none":
        return "transform"
    if s.get("aspect-ratio", "auto") != "auto":
        return "aspect-ratio"
    if any(s.get(k, "visible") not in ("visible", "") for k in ("overflow-x", "overflow-y")):
        return "overflow"
    if d == "inline-block":
        return "inline-block"
    if d == "inline":
        return "inline"
    return "block flow"


def group_key(item: dict, kind: str) -> str:
    if kind in ("missing", "extra"):
        tag = item.get("label", "").split("#")[0].split(".")[0]
        return f"content: {kind} <{tag}>"
    differing = [p for p, _, _ in (item.get("props") or {}).get("differing", [])]
    if differing:
        return f"value: {differing[0]}"
    if kind in TEXT_KINDS:
        return f"text: {kind}"
    return f"{kind}: {context(item)}"


def group(roots_by_site: dict[str, list[dict]], first_by_site: dict[str, int | None]) -> list[dict]:
    """Ranks group keys over sites. `roots_by_site[site]` is a result.json's `roots`."""
    groups: dict[str, dict] = {}
    for site, roots in roots_by_site.items():
        first = first_by_site.get(site)
        for n, item in enumerate(roots):
            for kind in item.get("kinds", []):
                key = group_key(item, kind)
                g = groups.setdefault(key, {"key": key, "sites": set(), "first": set(), "roots": 0,
                                            "examples": []})
                g["sites"].add(site)
                g["roots"] += 1
                if n == first:
                    g["first"].add(site)
                if len(g["examples"]) < 4 and not any(e.startswith(site + " ") for e in g["examples"]):
                    g["examples"].append(f"{site} {item.get('label', '')} delta {item.get('delta')}"
                                         + _differing_text(item))
    rows = [{**g, "sites": sorted(g["sites"]), "first": sorted(g["first"])} for g in groups.values()]
    rows.sort(key=lambda r: (-len(r["sites"]), -len(r["first"]), r["key"]))
    return rows


def _differing_text(item: dict) -> str:
    diff = (item.get("props") or {}).get("differing", [])
    if not diff:
        return ""
    return " | " + "; ".join(f"{p}: {a} -> {b}" for p, a, b in diff[:3])


def read_results(width_dir: Path) -> tuple[dict, dict, dict]:
    """(roots by site, first index by site, status by site) from one width's corpus run."""
    roots, first, status = {}, {}, {}
    for f in sorted(width_dir.glob("*/result.json")):
        site = f.parent.name
        try:
            r = json.loads(f.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        status[site] = r.get("status")
        if r.get("status") == "ok":
            roots[site] = r.get("roots") or []
            first[site] = r.get("first")
    return roots, first, status


def summarize(out: Path, widths: list[int]) -> dict:
    """Groups each width's results; returns {width: {"status": ..., "groups": [...]}} and an `all` entry."""
    summary = {}
    all_roots, all_first = defaultdict(list), {}
    for w in widths:
        roots, first, status = read_results(out / f"w{w}")
        summary[str(w)] = {"status": status, "groups": group(roots, first)}
        for site, rs in roots.items():
            base = len(all_roots[site])
            all_roots[site].extend(rs)
            if first.get(site) is not None and site not in all_first:
                all_first[site] = base + first[site]
    summary["all"] = {"groups": group(dict(all_roots), all_first)}
    return summary
