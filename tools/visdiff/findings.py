# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Content Chrome shows and Snowlark does not: matching, classification and ranking of two snapshots.

A content element is one with its own text, an image, a canvas, a video, an SVG, a form control, a
background image or (when large) a background colour. Chrome shows it when its effective opacity is above
`MIN_OPACITY`, its visibility is `visible` and part of it at least `MIN_AREA` px² lies in the region (the
first `screens` screens of the page) inside its clip. Each such element is looked up in Snowlark's
snapshot (`match`) and, when Snowlark does not show it, given a state and a cause element (`classify`).
The elements are then grouped by cause element: one finding per element where the difference starts,
with the area of its content (on a grid of `CELL` px cells, so nested content counts once).
"""

import re

MIN_OPACITY = 0.05
MIN_AREA = 64  # px², visible part of a content element
MIN_BG_AREA = 20000  # px², for an element whose only content is a background colour
CELL = 16

BLOCKED = [
    (r"access denied|attention required|are you a robot|verify you are human|just a moment|"
     r"press (and|&) hold|bot detection|request blocked|enable javascript and cookies|captcha", "bot check"),
    (r"before you continue|cookie(s)? (settings|preferences|policy)|we use cookies|accept all|"
     r"manage (your )?(privacy|cookies|consent)|your privacy choices", "consent/cookie prompt"),
]

# Consent and cookie banners: shown or closed depending on where the page thinks the visitor is and on
# whether the decline click found its button, so they are left out of the element findings.
CONSENT = re.compile(r"cookie|consent|gdpr|onetrust|didomi|truste|cmp-|-cmp|mscmp|sp_message|privacy-?banner",
                     re.I)


def in_consent_banner(snap, i):
    """Whether element `i` of the snapshot is, or is inside, a consent or cookie banner."""
    for a in snap.ancestors(i):
        n = snap.nodes[a]
        if CONSENT.search(n.get("id") or "") or CONSENT.search(n.get("c") or ""):
            return True
    return False


# Cause groups, in the order the summary lists them.
GROUPS = {
    "animation": "hidden by a CSS animation (opacity or visibility)",
    "transition": "hidden by a transition that did not run",
    "class": "hidden: classes or data attributes differ (a script set them in one engine only)",
    "inline": "hidden by an inline style (script)",
    "style": "hidden by the stylesheet",
    "display-none": "display: none",
    "zero-size": "zero size",
    "clipped": "clipped by an overflow ancestor",
    "off-page": "outside the page",
    "image": "image not loaded",
    "missing": "missing elements (not created)",
}


def label(rec):
    s = rec["t"]
    if rec.get("id"):
        s += "#" + rec["id"]
    if rec.get("c"):
        s += "." + ".".join(rec["c"].split()[:3])
    return s


def is_content(rec):
    k = rec.get("k", "")
    # Frames are left to the pixel check, which masks them: their contents are ads, consent dialogs and
    # embeds that differ between runs.
    if not k or rec["t"] in ("iframe", "frame"):
        return False
    if k == "g":
        return rec["r"][2] * rec["r"][3] >= MIN_BG_AREA
    return True


def visible_rect(rec, region):
    """The part of the element's box inside its clip and the region, as [x0, y0, x1, y1], or None."""
    x, y, w, h = rec["r"]
    x0, y0, x1, y1 = x, y, x + w, y + h
    for c in (rec.get("cl"), region):
        if c:
            x0, y0, x1, y1 = max(x0, c[0]), max(y0, c[1]), min(x1, c[2]), min(y1, c[3])
    if x1 - x0 <= 0 or y1 - y0 <= 0:
        return None
    return [x0, y0, x1, y1]


def shown(rec, region=None):
    """Whether the engine paints the element's content where it can be seen (region None: anywhere on the
    page). Returns the visible rect, or None."""
    if rec.get("d") == "none" or rec.get("v", "visible") != "visible":
        return None
    if rec.get("eo", 1) <= MIN_OPACITY:
        return None
    vr = visible_rect(rec, region)
    if not vr or (vr[2] - vr[0]) * (vr[3] - vr[1]) < MIN_AREA:
        return None
    return vr


class Snapshot:
    def __init__(self, snap):
        self.snap = snap
        self.nodes = snap["nodes"]
        self.by_path = {}
        self.by_id = {}
        self.by_text = {}
        self.by_src = {}
        for i, n in enumerate(self.nodes):
            self.by_path.setdefault(n["p"], i)
            if n.get("id"):
                self.by_id.setdefault(n["id"], []).append(i)
            if n.get("tx"):
                self.by_text.setdefault((n["t"], n["tx"]), []).append(i)
            if n.get("src"):
                self.by_src.setdefault((n["t"], n["src"]), []).append(i)

    def ancestors(self, i):
        """Indices from `i` up to the root."""
        while i >= 0:
            yield i
            i = self.nodes[i]["pa"]


def _compatible(c, o):
    if c["t"] != o["t"]:
        return False
    if c.get("tx") and o.get("tx") and c["tx"] != o["tx"]:
        return False
    if c.get("src") and o.get("src") and c["src"] != o["src"]:
        return False
    return True


def _closest(c, cands, ours):
    """The candidate whose path shares the longest prefix with `c`'s, then the nearest in y."""
    def score(i):
        p = ours.nodes[i]["p"]
        n = 0
        for a, b in zip(c["p"].split("/"), p.split("/")):
            if a != b:
                break
            n += 1
        return (-n, abs(ours.nodes[i]["r"][1] - c["r"][1]))
    return min(cands, key=score)


def match(c, ours):
    """Snowlark's record for Chrome's record `c`: the same path with compatible content, else the same id,
    the same own text, or the same image source. Returns an index into `ours.nodes`, or None."""
    i = ours.by_path.get(c["p"])
    if i is not None and _compatible(c, ours.nodes[i]):
        return i
    if c.get("id"):
        cands = [j for j in ours.by_id.get(c["id"], []) if _compatible(c, ours.nodes[j])]
        if cands:
            return _closest(c, cands, ours)
    if c.get("tx"):
        cands = ours.by_text.get((c["t"], c["tx"]))
        if cands:
            return _closest(c, cands, ours)
    if c.get("src"):
        cands = ours.by_src.get((c["t"], c["src"]))
        if cands:
            return _closest(c, cands, ours)
    return None


def _match_wrapper(c, ours):
    """A looser match for ancestors (no content to compare): the same path and tag."""
    i = ours.by_path.get(c["p"])
    if i is not None and ours.nodes[i]["t"] == c["t"]:
        return i
    return match(c, ours)


def _state_attributes(rec):
    """The record's data and ARIA attributes as `name=value` tokens, with values that are ids, timestamps or
    tracking data (long, or digits) cut to the name: those differ on every load, unlike a state such as
    `data-visible=true`."""
    out = set()
    for tok in (rec.get("da") or "").split():
        name, _, value = tok.partition("=")
        if len(value) > 16 or re.search(r"\d{4}", value):
            tok = name
        out.add(tok)
    return out


def _state_diff(c, n):
    """How Chrome's record `c` and Snowlark's `n` differ in what scripts set: classes and data attributes.
    Returns a description, or None."""
    parts = []
    if c.get("c", "") != n.get("c", ""):
        cc, nc = set(c.get("c", "").split()), set(n.get("c", "").split())
        parts.append("class " + " ".join(["+" + x for x in sorted(cc - nc)] + ["-" + x for x in sorted(nc - cc)])[:160])
    if "da" in c or "da" in n:
        ca, na = _state_attributes(c), _state_attributes(n)
        if ca != na:
            parts.append("attributes " + " ".join(["+" + x for x in sorted(ca - na)] +
                                                   ["-" + x for x in sorted(na - ca)])[:160])
    return "; ".join(parts) or None


def _script_state(start, ours, rev, theirs):
    """The nearest ancestor-or-self of Snowlark's `start` whose classes or data attributes differ from its
    counterpart in Chrome (+ only in Chrome, - only in Snowlark): (description) or None."""
    for a in ours.ancestors(start):
        if a not in rev:
            continue
        d = _state_diff(theirs.nodes[rev[a]], ours.nodes[a])
        if d:
            return f"on {label(ours.nodes[a])}: {d}"
    return None


def _hidden_cause(kind, oi, ours, rev, theirs):
    """For an element Snowlark hides (`kind` "opacity" or "visibility"): the ancestor-or-self where the
    hiding starts, its group and a description."""
    nodes = ours.nodes
    start = oi
    for a in ours.ancestors(oi):
        n = nodes[a]
        if kind == "opacity" and n.get("op", 1) <= MIN_OPACITY:
            start = a
            break
        if kind == "visibility":
            parent = n["pa"]
            if parent < 0 or nodes[parent].get("v", "visible") == "visible":
                start = a
                break
    n = nodes[start]
    c = theirs.nodes[rev[start]] if start in rev else None
    if n.get("an"):
        return start, "animation", f"{kind} from animation `{n['an']}`"
    # A script that set a class or attribute in Chrome only explains an animation or transition missing.
    state = _script_state(start, ours, rev, theirs)
    if state:
        also = f" (Chrome applies animation `{c['an']}`)" if c is not None and c.get("an") else ""
        return start, "class", f"{kind}; {state}{also}"
    if c is not None and c.get("an"):
        return start, "animation", f"{kind}; Chrome applies animation `{c['an']}`, Snowlark none"
    if n.get("tr") and (kind in n["tr"] or "all" in n["tr"]):
        return start, "transition", f"{kind} with a transition on `{n['tr']}`"
    if n.get("ia"):
        return start, "inline", f"{kind} set by an inline style"
    return start, "style", f"{kind} from the stylesheet"


def _display_none(oi, ours, rev, theirs):
    """For an element under display none in Snowlark: the highest such ancestor-or-self."""
    start = oi
    for a in ours.ancestors(oi):
        if ours.nodes[a].get("d") == "none":
            start = a
    n = ours.nodes[start]
    how = "display: none"
    state = _script_state(start, ours, rev, theirs)
    if state:
        how += f"; {state}"
    elif n.get("ia"):
        how += " from an inline style"
    return ("display-none", rev.get(start), start, how)


def classify(ci, theirs, ours, fwd, rev, page_w):
    """Snowlark's state for Chrome's shown content element `ci`: None when Snowlark shows it too, else
    (group, cause index in Chrome's snapshot or None, cause index in Snowlark's or None, detail)."""
    c = theirs.nodes[ci]
    oi = fwd.get(ci)
    if oi is None:
        # Missing: the nearest Chrome ancestor that Snowlark has. Display none there hides the subtree.
        for a in theirs.ancestors(c["pa"]):
            oa = fwd.get(a)
            if oa is None:
                continue
            for b in ours.ancestors(oa):
                if ours.nodes[b].get("d") == "none":
                    return _display_none(b, ours, rev, theirs)
            return ("missing", a, oa, f"no counterpart under {label(theirs.nodes[a])}")
        return ("missing", None, None, "no counterpart")
    o = ours.nodes[oi]
    if o.get("d") == "none":
        return _display_none(oi, ours, rev, theirs)
    if o.get("eo", 1) <= MIN_OPACITY:
        start, group, how = _hidden_cause("opacity", oi, ours, rev, theirs)
        return (group, rev.get(start), start, how)
    if o.get("v", "visible") != "visible":
        start, group, how = _hidden_cause("visibility", oi, ours, rev, theirs)
        return (group, rev.get(start), start, how)
    if "i" in c.get("k", "") and "I" in o.get("k", ""):
        ext = re.search(r"\.([A-Za-z0-9]{2,5})$", o.get("src", "") or "")
        return ("image", ci, oi, f"image not loaded ({ext.group(1).lower() if ext else 'no extension'}): "
                f"{o.get('src', '')[:120]}")
    x, y, w, h = o["r"]
    if w * h <= 0:
        return ("zero-size", ci, oi, f"{w:g}x{h:g} in Snowlark, {c['r'][2]:g}x{c['r'][3]:g} in Chrome")
    if x + w <= 0 or y + h <= 0 or x >= page_w:
        return ("off-page", ci, oi,
                f"box at ({x:g}, {y:g}) in Snowlark, ({c['r'][0]:g}, {c['r'][1]:g}) in Chrome")
    if not shown(o):
        # Clipped: the nearest clipping ancestor.
        for a in ours.ancestors(o["pa"]):
            n = ours.nodes[a]
            if n.get("ov"):
                return ("clipped", rev.get(a), a, f"clipped by {label(n)} ({n['r'][2]:g}x{n['r'][3]:g} in Snowlark)")
        return ("clipped", ci, oi, "clipped")
    return None


def _kind(rec):
    return (rec.get("t"), tuple(sorted((rec.get("c") or "").split())))


def stand_ins(ours):
    """Snowlark's shown elements by tag and class list, with their boxes: a rotating carousel or a
    slideshow shows another slide of the same kind where Chrome shows this one."""
    out = {}
    for n in ours.nodes:
        if n.get("c") and shown(n):
            out.setdefault(_kind(n), []).append(n["r"])
    return out


def has_stand_in(c, index, tolerance=2):
    """Whether Snowlark shows an element of the same tag and classes in the box Chrome shows `c` in."""
    for r in index.get(_kind(c), ()):
        if all(abs(a - b) <= tolerance for a, b in zip(r, c["r"])):
            return True
    return False


def build_mapping(theirs, ours):
    """(Chrome index -> Snowlark index, Snowlark index -> first Chrome index) for every Chrome element that
    has a counterpart."""
    fwd, rev = {}, {}
    for i, c in enumerate(theirs.nodes):
        j = match(c, ours) if c.get("k") else _match_wrapper(c, ours)
        if j is not None:
            fwd[i] = j
            rev.setdefault(j, i)
    return fwd, rev


def _cells(rect):
    x0, y0, x1, y1 = rect
    return {(cx, cy) for cx in range(int(x0) // CELL, int(max(x0, x1 - 1)) // CELL + 1)
            for cy in range(int(y0) // CELL, int(max(y0, y1 - 1)) // CELL + 1)}


def compare(chrome_snap, snowlark_snap, screens=3):
    """The findings for one page: a list of dicts, largest area first."""
    theirs, ours = Snapshot(chrome_snap), Snapshot(snowlark_snap)
    vw, vh = chrome_snap["viewport"]
    region = [0, 0, vw, vh * screens]
    page_w = max(snowlark_snap["scroll"][0], snowlark_snap["viewport"][0])
    fwd, rev = build_mapping(theirs, ours)
    index = stand_ins(ours)
    groups = {}
    for ci, c in enumerate(theirs.nodes):
        if not is_content(c):
            continue
        vr = shown(c, region)
        if not vr:
            continue
        state = classify(ci, theirs, ours, fwd, rev, page_w)
        if state is None or has_stand_in(c, index) or in_consent_banner(theirs, ci):
            continue
        group, cause_c, cause_o, detail = state
        key = (group, cause_c if cause_c is not None else ("o", cause_o) if cause_o is not None else ("c", ci))
        f = groups.get(key)
        if f is None:
            cause = theirs.nodes[cause_c] if cause_c is not None else (
                ours.nodes[cause_o] if cause_o is not None else c)
            f = groups[key] = {
                "group": group,
                "element": label(cause),
                "path": cause["p"],
                "detail": detail,
                "cells": set(),
                "elements": 0,
                "rect": list(vr),
                "first": label(c),
            }
        f["cells"] |= _cells(vr)
        f["elements"] += 1
        r = f["rect"]
        f["rect"] = [min(r[0], vr[0]), min(r[1], vr[1]), max(r[2], vr[2]), max(r[3], vr[3])]
    findings = []
    for f in groups.values():
        f["area"] = len(f.pop("cells")) * CELL * CELL
        f["share"] = round(f["area"] / float(region[2] * region[3]), 4)
        findings.append(f)
    findings.sort(key=lambda f: -f["area"])
    return findings


def page_notes(chrome_snap, snowlark_snap):
    """Warnings that make a page's findings doubtful (bot checks, consent walls, different content)."""
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
        if chrome_snap["url"].split("?")[0].rstrip("/") != snowlark_snap["url"].split("?")[0].rstrip("/"):
            notes.append(f"final URLs differ: {chrome_snap['url']} vs {snowlark_snap['url']}")
        nc, no = len(chrome_snap["nodes"]), len(snowlark_snap["nodes"])
        if abs(nc - no) > 0.3 * max(nc, no, 1):
            notes.append(f"element counts differ a lot ({nc} in Chrome, {no} in Snowlark)")
        if chrome_snap.get("title") != snowlark_snap.get("title"):
            notes.append(f"titles differ: {chrome_snap.get('title')!r} vs {snowlark_snap.get('title')!r}")
    return notes


def pixel_share(pixels, masks=()):
    """The share of the blocks of the pixel map (outside `masks`, [x0, y0, x1, y1] each) that are content
    in Chrome and background in Snowlark."""
    if not pixels or not pixels.get("map"):
        return None
    b = pixels["block"]
    blocks = only = 0
    for row, line in enumerate(pixels["map"]):
        cy = row * b + b / 2.0
        spans = [(m[0], m[2]) for m in masks if m[1] <= cy < m[3]]
        for col, ch in enumerate(line):
            cx = col * b + b / 2.0
            if any(x0 <= cx < x1 for x0, x1 in spans):
                continue
            blocks += 1
            only += ch == "#"
    return round(only / float(blocks), 4) if blocks else None


def site_score(findings):
    """One number to rank pages by: the hidden content's share of the region. Missing elements count
    half (scripts, A/B tests and ads swing those). The pixel check is reported beside it, not in it."""
    s = 0.0
    for f in findings:
        s += f["share"] * (0.5 if f["group"] == "missing" else 1.0)
    return round(min(1.0, s), 4)


def frame_rects(*snaps):
    """The boxes of the frames either engine shows, for the pixel check to leave out."""
    out = []
    for snap in snaps:
        if not snap:
            continue
        for n in snap["nodes"]:
            if n["t"] in ("iframe", "frame", "object", "embed") and n["r"][2] * n["r"][3] > 0:
                x, y, w, h = n["r"]
                out.append([x, y, x + w, y + h])
    return out


def rank_groups(sites):
    """Across pages: per group, the pages it appears on, its total share, and its largest findings.
    `sites` is a list of {"url", "width", "findings", "doubtful"}; doubtful pages are left out."""
    out = {}
    for s in sites:
        if s.get("doubtful"):
            continue
        for f in s["findings"]:
            g = out.setdefault(f["group"], {"group": f["group"], "title": GROUPS.get(f["group"], f["group"]),
                                            "pages": set(), "share": 0.0, "top": []})
            g["pages"].add((s["url"], s["width"]))
            g["share"] += f["share"]
            g["top"].append({"url": s["url"], "width": s["width"], **{k: f[k] for k in
                            ("element", "detail", "share", "elements")}})
    ranked = []
    for g in out.values():
        g["pages"] = len(g["pages"])
        g["share"] = round(g["share"], 4)
        g["top"] = sorted(g["top"], key=lambda t: -t["share"])[:15]
        ranked.append(g)
    ranked.sort(key=lambda g: (-g["share"], -g["pages"]))
    return ranked
