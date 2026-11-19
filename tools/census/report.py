# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Stage 4: report.md from the stages' JSON."""

from __future__ import annotations

import re

from corpus import match_kind
from surface import head, is_ecmascript, member_name

# Names whose rank the report shows as if they were missing: APIs whose absence broke real sites
# before they were implemented (ReadableStream: linear.app's hero; window.screen: reddit).
VALIDATE = ["ReadableStream", "Screen", "window.screen", "WritableStream", "TransformStream"]


def _md(text: str) -> str:
    return str(text).replace("|", "\\|").replace("\n", " ")


_WORKER_TWINS = {"WorkerNavigator": "Navigator", "WorkerLocation": "Location"}


def _normalize(key: str) -> str:
    """One row for a member a window and a worker share: `self.x`, `WorkerGlobalScope.prototype.x`
    and `Window.prototype.x` are `window.x`; `WorkerNavigator.prototype.x` is
    `Navigator.prototype.x`."""
    h, _, rest = key.partition(".")
    if h == "self":
        return "window." + rest
    if h in ("Window", "WorkerGlobalScope", "DedicatedWorkerGlobalScope") and rest.startswith(
            "prototype."):
        return "window." + rest[len("prototype."):]
    if h in _WORKER_TWINS:
        return _WORKER_TWINS[h] + ("." + rest if rest else "")
    return key


def missing_rows(surfaces: dict, diff: dict, use: dict, snowlark_names: set[str], runtime: dict):
    """One row per missing key (window and worker merged), with its site references."""
    rows = {}
    for scope in ("window", "worker"):
        d = diff.get(scope)
        if not d:
            continue
        chrome = surfaces["chrome"][scope]["members"]
        for key in d["missing"]:
            norm = _normalize(key)
            row = rows.setdefault(norm, {"key": norm, "kind": chrome[key], "scopes": [],
                                         "members": 0})
            if scope not in row["scopes"]:
                row["scopes"].append(scope)
            row["members"] = max(row["members"], d["missing_members_of"].get(key, 0))
            sites = use.get(scope, {}).get(key, [])
            row["sites"] = sorted(set(row.get("sites", [])) | set(sites))
    absent = {scope: {k for k in d["missing"] if "." not in k}
              for scope, d in diff.items() if d}
    failing = {}
    for site, r in runtime.items():
        for n in r.get("reference_errors", []) + r.get("type_error_names", []):
            failing.setdefault(n, set()).add(site)
    for row in rows.values():
        name = member_name(row["key"]) if "." in row["key"] else row["key"]
        row["shared"] = "." in row["key"] and name in snowlark_names
        # Matched by its bare name, and that name is a plain word (`config`, `when`): over-counted.
        row["generic"] = match_kind(row["key"]) == "name" and bool(re.fullmatch(r"[a-z]+", name))
        row["ecmascript"] = is_ecmascript(row["key"])
        hits = set(failing.get(name, ()))
        if row["kind"] in ("interface", "namespace") and "." not in row["key"]:
            # A missing interface also failed where a script read one of its members off undefined
            # (`performance.timing.navigationStart`: "reading 'navigationStart'").
            # Its inherited members count while the ancestor is missing too (PerformanceNavigationTiming's
            # `responseStart` is PerformanceResourceTiming's).
            for scope in row["scopes"]:
                chrome = surfaces["chrome"][scope]
                parents = chrome.get("parents", {})
                chain, h = set(), row["key"]
                while h and h not in chain and (h == row["key"] or h in absent.get(scope, ())):
                    chain.add(h)
                    h = parents.get(h, "")
                for k in chrome["members"]:
                    if head(k) in chain and "." in k:
                        hits |= failing.get(member_name(k), set())
        row["runtime"] = sorted(hits)
    return sorted(rows.values(), key=lambda r: (-len(r["sites"]), r["key"]))


def capture_verdict(img: dict, chrome: dict) -> str:
    """BLANK (nothing drawn, or under 1% ink where Chrome draws over 5%), sparse (under 40% of
    Chrome's ink: whole parts missing), no, or ? (no capture)."""
    if not img or "ink" not in img:
        return "?"
    ink, cink = img["ink"], chrome.get("ink")
    if img["blank"] or (cink is not None and ink < 0.01 and cink > 0.05):
        return "**BLANK**"
    if cink is not None and cink > 0.05 and ink < 0.4 * cink:
        return "**sparse**"
    return "no"


def _main(row: dict) -> bool:
    """Whether a row belongs in table 1: a web-platform member whose count can be trusted, or one
    that failed at runtime."""
    return not row["ecmascript"] and (bool(row["runtime"]) or not (row["shared"] or row["generic"]))


def rank_of(key: str, rows: list[dict], use: dict) -> tuple[int, int] | None:
    """(rank, site count) the key would have among the unshared web-platform missing rows."""
    sites = set(use.get("window", {}).get(key, [])) | set(use.get("worker", {}).get(key, []))
    pool = [r for r in rows if _main(r) and r["key"] != key]
    better = sum(1 for r in pool if len(r["sites"]) > len(sites))
    return better + 1, len(sites)


def render(meta: dict, surfaces: dict, diff: dict, use: dict, runtime: dict, fetched: dict,
           top: int = 40) -> str:
    n_sites = len(fetched) or len(runtime) or 1
    snowlark_names = {
        member_name(k) for scope in ("window", "worker") if surfaces["snowlark"].get(scope)
        for k in surfaces["snowlark"][scope]["members"]
    }
    rows = missing_rows(surfaces, diff, use, snowlark_names, runtime)
    web = [r for r in rows if not r["ecmascript"]]
    unshared = [r for r in web if _main(r)]
    shared = [r for r in web if not _main(r)]
    ecma = [r for r in rows if r["ecmascript"]]
    L = []
    w = L.append
    w("# Snowlark missing-API census")
    w("")
    w(f"Generated {meta['date']} by `tools/census/run.py` (Snowlark {meta.get('snowlark_commit', '?')}, "
      f"{meta.get('chrome', 'Chrome ?')}). Corpus: {len(fetched)} sites fetched by Chrome, "
      f"{len(runtime)} rendered by Snowlark. Stage time {meta.get('run_minutes', '?')} min "
      "(the last surface, fetch, runtime and usage runs). "
      "See README.md for how to read it.")
    w("")
    for scope in ("window", "worker"):
        d = diff.get(scope)
        if not d:
            w(f"- **{scope}**: no diff ({_md(meta.get(scope + '_error', 'one side did not enumerate'))})")
            continue
        c = len(surfaces["chrome"][scope]["members"])
        o = len(surfaces["snowlark"][scope]["members"])
        mi = len(d["missing"])
        mm = sum(d["missing_members_of"].values())
        w(f"- **{scope}**: Chrome {c} members, Snowlark {o}; missing in Snowlark: {mi} keys "
          f"({len(d['missing_members_of'])} whole interfaces/namespaces standing for {mm} more members); "
          f"Snowlark-only: {len(d['extra'])}; kind differs: {len(d['kind'])}; stubs: {len(d['stubs'])}.")
    w("")

    w(f"## 1. Top missing APIs by site count")
    w("")
    w("Members Chrome exposes and Snowlark does not, ranked by how many corpus sites' scripts reference "
      "the name. A missing interface stands for all its members (count in parentheses). "
      "**Runtime** lists sites where Snowlark threw a ReferenceError/TypeError naming it. "
      "Members matched by their bare name are in 1b when Snowlark has that name on another "
      "interface or the name is a plain word (their counts cannot tell the interfaces apart), "
      "unless they failed at runtime; ECMAScript built-ins (V8 version gaps) are in 1c.")
    w("")
    w("| # | API | kind | sites | runtime | where | example sites |")
    w("|---|---|---|---|---|---|---|")
    for i, r in enumerate(unshared[:top], 1):
        kind = r["kind"] + (f" ({r['members']})" if r["members"] else "")
        w(f"| {i} | `{r['key']}` | {kind} | {len(r['sites'])}/{n_sites} | "
          f"{', '.join(r['runtime']) or ''} | {'+'.join(r['scopes'])} | "
          f"{', '.join(r['sites'][:4])}{' ...' if len(r['sites']) > 4 else ''} |")
    rest = [r for r in unshared[top:] if r["sites"]]
    if rest:
        w("")
        w(f"Also referenced ({len(rest)} more): " + ", ".join(
            f"`{r['key']}` {len(r['sites'])}" for r in rest[:150]) + (" ..." if len(rest) > 150 else ""))
    w("")
    proven = [r for r in web if r["runtime"]]
    w("### 1a. Missing APIs that failed at runtime")
    w("")
    w("Rows of table 1 (at any rank) with a runtime failure in Snowlark on some corpus site: the "
      "first to implement.")
    w("")
    if proven:
        w("| API | kind | sites referencing | failed on |")
        w("|---|---|---|---|")
        for r in sorted(proven, key=lambda r: (-len(r["runtime"]), -len(r["sites"]), r["key"])):
            w(f"| `{r['key']}` | {r['kind']} | {len(r['sites'])}/{n_sites} | "
              f"{', '.join(r['runtime'])} |")
    else:
        w("none")
    w("")
    w("### 1b. Missing members with shared or generic names (counts unreliable)")
    w("")
    w("| API | kind | sites (name anywhere) | why |")
    w("|---|---|---|---|")
    for r in shared[:30]:
        why = "name on another interface in Snowlark" if r["shared"] else "plain-word name"
        w(f"| `{r['key']}` | {r['kind']} | {len(r['sites'])}/{n_sites} | {why} |")
    if len(shared) > 30:
        w(f"| ... {len(shared) - 30} more (see diff.json) | | | |")
    w("")
    w("### 1c. Missing ECMAScript built-ins (V8 version)")
    w("")
    w(", ".join(f"`{r['key']}` {len(r['sites'])}" for r in ecma) or "none")
    w("")
    stubs = []
    for scope in ("window", "worker"):
        d = diff.get(scope)
        if d:
            stubs += [(scope, *s) for s in d["stubs"]]
    w("### 1d. Stubs: present in Snowlark, but undefined or null where Chrome has a value")
    w("")
    if stubs:
        w("| scope | getter | Chrome | Snowlark |")
        w("|---|---|---|---|")
        for scope, k, c, o in stubs:
            w(f"| {scope} | `{k}` | {c} | {o} |")
    else:
        w("none")
    w("")
    w("### 1e. Would the census have caught past breakage?")
    w("")
    w("Rank among the web-platform rows of table 1 that each name would have had if it were missing "
      "(it is implemented now; its references are counted all the same).")
    w("")
    w("| API | sites | would-be rank |")
    w("|---|---|---|")
    for key in VALIDATE:
        rk, n = rank_of(key, rows, use)
        w(f"| `{key}` | {n}/{n_sites} | {rk} of {len(unshared) + 1} |")
    w("")

    w("## 2. Per-site runtime results (Snowlark `--render`, content blocking off)")
    w("")
    w("**Ink**: the share of sampled pixels of the first viewport (1280x800) that differ from its "
      "dominant colour, for Snowlark / for Chrome's screenshot of the same page during the fetch. "
      "**Blank**: BLANK when Snowlark's ink is under 0.05%, or under 1% where Chrome's is over 5%; "
      "sparse when it is under 40% of Chrome's (whole parts of the page missing; a consent dialog "
      "or a different page served to each can also do it). **Settle**: ms to load and settle, or the timeout. "
      "**Exc.**: uncaught exceptions (and unhandled rejections); **err**: other console.error "
      "calls. **First exception**: text and where it was thrown. **Missing names**: from "
      "ReferenceError / TypeError messages. **Chrome**: exceptions Chrome itself threw on the "
      "same page during the fetch.")
    w("")
    w("| site | blank | ink | settle | exc. | err | first exception | missing names | elements | Chrome |")
    w("|---|---|---|---|---|---|---|---|---|---|")
    for site in sorted(runtime):
        r = runtime[site]
        img = r.get("image") or {}
        cimg = fetched.get(site, {}).get("chrome_image") or {}
        blank = capture_verdict(img, cimg)
        ink = (f"{img['ink'] * 100:.1f}%" if img and "ink" in img else "?") + " / " + (
            f"{cimg['ink'] * 100:.1f}%" if "ink" in cimg else "?")
        if r.get("killed"):
            settle = "**hung** (killed)"
        elif r.get("settle_ms") is None:
            settle = f"**crashed** (exit {r.get('exit_code')})"
        elif r.get("timeout") or not r.get("loaded"):
            settle = f"**no settle** ({r['settle_ms']} ms{'' if r.get('loaded') else ', no load'})"
        else:
            settle = f"{r['settle_ms']} ms"
        first = r["exceptions"][0] if r["exceptions"] else None
        first_text = ""
        if first:
            first_text = _md(first["text"][:160])
            if first["source"]:
                src = first["source"]
                first_text += f" <br>`{_md(src if len(src) < 90 else '...' + src[-87:])}`"
        if r.get("panic"):
            first_text = "panic: " + _md(r["panic"][:150]) + (" / " + first_text if first_text else "")
        names = ", ".join(r.get("reference_errors", []) + r.get("type_error_names", []))
        ev = r.get("eval") or {}
        chrome_exc = len(fetched.get(site, {}).get("chrome_exceptions", []))
        w(f"| {site} | {blank} | {ink} | {settle} | {len(r['exceptions'])} | {len(r['errors'])} | "
          f"{first_text} | {_md(names[:120])} | {ev.get('elements', '')} | {chrome_exc} |")
    w("")

    w("## 3. Snowlark-only members")
    w("")
    w("Members Snowlark exposes and Chrome does not: non-standard additions, members on the wrong "
      "object (e.g. a [Global] interface's members on its prototype instead of the instance), or "
      "stale IDL. Should be close to none.")
    w("")
    for scope in ("window", "worker"):
        d = diff.get(scope)
        if not d:
            continue
        groups: dict[str, list[str]] = {}
        for k in d["extra"]:
            groups.setdefault(head(k), []).append(k)
        w(f"**{scope}**: {len(d['extra'])} keys")
        w("")
        if groups:
            w("| interface | count | members |")
            w("|---|---|---|")
            for h, ks in sorted(groups.items(), key=lambda kv: (-len(kv[1]), kv[0])):
                more = d["extra_members_of"].get(h, 0)
                shown = ", ".join(f"`{k}`" for k in ks[:12]) + (" ..." if len(ks) > 12 else "")
                w(f"| `{h}` | {len(ks)}{f' (+{more} members)' if more else ''} | {shown} |")
            w("")
    w("### Kind differences (present in both, but method/getter/setter/data differ)")
    w("")
    for scope in ("window", "worker"):
        d = diff.get(scope)
        if not d or not d["kind"]:
            continue
        w(f"**{scope}** ({len(d['kind'])}): " + ", ".join(
            f"`{k}` {c}→{o}" for k, c, o in d["kind"][:80]) + (" ..." if len(d["kind"]) > 80 else ""))
        w("")
    return "\n".join(L) + "\n"
