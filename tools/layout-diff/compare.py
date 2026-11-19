# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Comparison of two layout snapshots (walker.js output): Chrome as the reference ("a"), Snowlark ("b").

1. Alignment. The trees are aligned top-down: under each matched parent, the child lists are aligned with a
   longest-matching-subsequence on "tag#id.classes" signatures, then (inside replaced runs) on tags alone,
   so one missing or extra element does not shift the nth-of-type of every later sibling.
2. Deltas. For a matched element: dw, dh (size) and rdx, rdy, its position change relative to its parent's
   (or, for position:absolute, to the nearest positioned ancestor's; for fixed, to the viewport's), so
   everything inside a moved box does not count as moved.
3. Explanations. Differences cascade; each difference is tested against the usual ways it can be a
   consequence of another one:
   - width from the parent (block-level boxes fill their container; proportional for percentages),
     or from the content (shrink-to-fit boxes: inline-level, floats, abspos, and blocks inside them);
   - height from the content (the in-flow children's or the own text's extent changed by as much),
     or from the parent (stretched flex/grid items, table cells);
   - position from the previous in-flow sibling (it grew or moved), from a missing/extra sibling, or
     from the parent's size change (centring, end alignment) or its own size change (centred box).
   Whatever is left unexplained is a root cause. Text height changes at an unchanged width are "text-wrap"
   roots (line breaking or font metrics), width changes of content-sized boxes with own text are
   "text-width" roots (glyph advances), visible elements present on one side only are "missing"/"extra".
4. Two lists come out: root causes in document order (the first one is "the first difference"), and the
   raw differences in document order with consequences collapsed the simple way (an element under a
   differing ancestor is listed only if its size differs by more than the ancestor's).
"""

import difflib
import functools
import re

GEOMETRY = {"width", "height", "top", "right", "bottom", "left"}
INITIAL = {
    "position": "static", "float": "none", "box-sizing": "content-box",
    "min-width": ("auto", "0px"), "min-height": ("auto", "0px"), "max-width": "none", "max-height": "none",
    "margin-top": "0px", "margin-right": "0px", "margin-bottom": "0px", "margin-left": "0px",
    "padding-top": "0px", "padding-right": "0px", "padding-bottom": "0px", "padding-left": "0px",
    "border-top-width": "0px", "border-right-width": "0px", "border-bottom-width": "0px",
    "border-left-width": "0px", "top": "auto", "right": "auto", "bottom": "auto", "left": "auto",
    "vertical-align": "baseline", "aspect-ratio": "auto", "object-fit": "fill",
    "object-position": "50% 50%", "overflow-x": "visible", "overflow-y": "visible",
    "flex-direction": "row", "flex-wrap": "nowrap", "flex-grow": "0", "flex-shrink": "1",
    "flex-basis": "auto", "justify-content": "normal", "align-items": "normal", "align-self": "auto",
    "align-content": "normal", "row-gap": "normal", "column-gap": "normal",
    "grid-template-columns": "none", "grid-template-rows": "none", "grid-column-start": "auto",
    "grid-column-end": "auto", "grid-row-start": "auto", "grid-row-end": "auto",
    "border-top-left-radius": "0px", "transform": "none", "visibility": "visible", "contain": "none",
    "scrollbar-gutter": "auto", "container-type": "normal",
    "letter-spacing": "normal", "white-space": "normal", "font-style": "normal", "text-align": "start",
}
# The properties that decide each kind of difference (the element's own; "parent:" ones its parent's).
RELEVANT = {
    "size": [
        "display", "position", "float", "box-sizing", "width", "height", "min-width", "min-height",
        "max-width", "max-height", "aspect-ratio", "object-fit", "padding-top", "padding-bottom",
        "padding-left", "padding-right", "border-top-width", "border-bottom-width", "overflow-x",
        "overflow-y", "flex-grow", "flex-shrink", "flex-basis", "align-self", "grid-column-start",
        "grid-column-end", "grid-row-start", "grid-row-end", "line-height", "font-size", "contain",
        "scrollbar-gutter", "container-type", "parent:container-type",
        "parent:display", "parent:flex-direction", "parent:flex-wrap", "parent:align-items",
        "parent:grid-template-columns", "parent:grid-template-rows",
    ],
    "position": [
        "display", "position", "float", "vertical-align", "margin-top", "margin-right", "margin-bottom",
        "margin-left", "top", "right", "bottom", "left", "line-height", "font-size", "transform",
        "align-self", "parent:display", "parent:flex-direction", "parent:justify-content",
        "parent:align-items", "parent:text-align", "parent:line-height", "parent:font-size",
        "parent:row-gap", "parent:column-gap",
    ],
    "text-wrap": [
        "width", "font-family", "font-size", "font-weight", "font-style", "line-height", "letter-spacing",
        "white-space", "padding-left", "padding-right",
    ],
    "text-width": ["font-family", "font-size", "font-weight", "font-style", "letter-spacing", "white-space"],
    "baseline": [
        "display", "vertical-align", "margin-top", "margin-bottom", "height", "line-height", "font-size",
        "padding-top", "padding-bottom", "border-top-width", "border-bottom-width", "overflow-y",
        "parent:line-height", "parent:font-size",
    ],
    "accumulated": ["line-height", "font-size", "font-family"],
    "inline-box": ["line-height", "font-size", "font-family"],
    "text-height": ["font-family", "font-size", "line-height"],
    "missing": ["display", "position", "visibility"],
    "extra": ["display", "position", "visibility"],
}


# Listed for a kind even at their initial values.
ALWAYS = {"baseline": ["display", "vertical-align"]}


class Side:
    def __init__(self, snap):
        self.snap = snap
        self.nodes = snap["nodes"]
        self.index = {p: i for i, p in enumerate(snap["props"])}
        self.children = [[] for _ in self.nodes]
        for i, n in enumerate(self.nodes):
            if n["pa"] >= 0:
                self.children[n["pa"]].append(i)

    def style(self, i, prop):
        k = self.index.get(prop)
        s = self.nodes[i]["s"]
        return s[k] if k is not None and k < len(s) else ""

    def area(self, i):
        r = self.nodes[i]["r"]
        return r[2] > 0 and r[3] > 0

    def shown(self, i):
        """Has an area and is not visibility: hidden (for missing and extra elements)."""
        return self.area(i) and self.style(i, "visibility") not in ("hidden", "collapse")

    def label(self, i):
        n = self.nodes[i]
        s = n["t"]
        if n["id"]:
            s += "#" + n["id"]
        if n["c"]:
            s += "." + ".".join(n["c"].split()[:4])
        return s


def signature(n):
    return n["t"] + ("#" + n["id"] if n["id"] else "") + ("." + n["c"] if n["c"] else "")


def align(a, b):
    """Returns (pairs a->b, missing a-roots, extra b-roots)."""
    pairs, missing, extra = {}, [], []
    if not a.nodes or not b.nodes:
        return pairs, list(range(min(1, len(a.nodes)))), list(range(min(1, len(b.nodes))))
    pairs[0] = 0
    stack = [(0, 0)]
    while stack:
        ia, ib = stack.pop()
        ca, cb = a.children[ia], b.children[ib]
        matched = []
        sm = difflib.SequenceMatcher(
            None, [signature(a.nodes[i]) for i in ca], [signature(b.nodes[i]) for i in cb], autojunk=False
        )
        for op, i1, i2, j1, j2 in sm.get_opcodes():
            if op == "equal":
                matched.extend(zip(ca[i1:i2], cb[j1:j2]))
                continue
            sa, sb = ca[i1:i2], cb[j1:j2]
            inner = difflib.SequenceMatcher(
                None, [a.nodes[i]["t"] for i in sa], [b.nodes[i]["t"] for i in sb], autojunk=False
            )
            for op2, k1, k2, l1, l2 in inner.get_opcodes():
                if op2 == "equal":
                    matched.extend(zip(sa[k1:k2], sb[l1:l2]))
                else:
                    missing.extend(sa[k1:k2])
                    extra.extend(sb[l1:l2])
        for x, y in matched:
            pairs[x] = y
            stack.append((x, y))
    return pairs, sorted(missing), sorted(extra)


@functools.lru_cache(maxsize=65536)
def norm(v, prop=""):
    if prop in ("min-width", "min-height") and v.strip() == "auto":
        v = "0px"  # Chrome resolves auto to 0px outside flex and grid items
    v = re.sub(r"\s+", " ", v.strip()).replace('"', "").replace("'", "")
    v = re.sub(r", ", ",", v)
    return re.sub(r"(-?\d+(?:\.\d+)?)px", lambda m: "%gpx" % (round(float(m.group(1)) * 2) / 2), v)


def is_initial(prop, value):
    init = INITIAL.get(prop)
    if init is None:
        return False
    return norm(value) in ((init,) if isinstance(init, str) else init)


def content_sized(display, float_, position):
    return (
        display.startswith("inline")
        or display in ("table-cell", "table-caption")
        or float_ not in ("none", "")
        or position in ("absolute", "fixed")
    )


class Diff:
    def __init__(self, snap_a, snap_b, tol=1.0):
        self.a, self.b = Side(snap_a), Side(snap_b)
        self.tol = tol
        self.te = max(2 * tol, 1.5)  # tolerance for "explained by"
        self.pairs, self.missing, self.extra = align(self.a, self.b)
        self.unsupported = self._unsupported_props()
        self._deltas()
        self._explain()

    def _unsupported_props(self):
        """Properties Snowlark's getComputedStyle leaves empty where Chrome has values (not compared)."""
        out = []
        for prop in self.a.snap["props"]:
            if prop not in self.b.index:
                out.append(prop)
                continue
            empty = sum(1 for ia, ib in self.pairs.items() if self.b.style(ib, prop) == "" and self.a.style(ia, prop))
            if self.pairs and empty >= 0.9 * len(self.pairs):
                out.append(prop)
        return out

    def _deltas(self):
        a, b = self.a, self.b
        self.d = {}
        for ia, ib in self.pairs.items():
            ra, rb = a.nodes[ia]["r"], b.nodes[ib]["r"]
            self.d[ia] = [rb[0] - ra[0], rb[1] - ra[1], rb[2] - ra[2], rb[3] - ra[3]]
        # Positions relative to the box they are placed against.
        self.rel = {}
        for ia in self.pairs:
            dx, dy = self.d[ia][0], self.d[ia][1]
            ref = self._reference(ia)
            if ref is not None:
                dx -= self.d[ref][0]
                dy -= self.d[ref][1]
            elif a.style(ia, "position") == "fixed":
                pass  # the viewport did not move
            self.rel[ia] = (dx, dy)
        # Content-sized context: content-sized boxes and the block boxes inside them.
        self.stf = {}
        for ia in sorted(self.pairs):
            pa = a.nodes[ia]["pa"]
            own = content_sized(a.style(ia, "display"), a.style(ia, "float"), a.style(ia, "position"))
            self.stf[ia] = own or (pa in self.stf and self.stf[pa] and a.style(ia, "position") != "fixed")

    def _reference(self, ia):
        """The matched box `ia` is positioned against: parent, positioned ancestor, or None (viewport)."""
        a = self.a
        pos = a.style(ia, "position")
        p = a.nodes[ia]["pa"]
        if pos == "fixed":
            return None
        if pos == "absolute":
            while p >= 0 and a.style(p, "position") in ("static", "") and a.nodes[p]["t"] != "html":
                p = a.nodes[p]["pa"]
        return p if p >= 0 and p in self.pairs else None

    def differs(self, ia):
        dx, dy, dw, dh = self.d[ia]
        rdx, rdy = self.rel[ia]
        t = self.tol
        size = abs(dw) > t or abs(dh) > t
        a_vis = self.a.area(ia) or self.b.area(self.pairs[ia])
        pos = a_vis and (abs(rdx) > t or abs(rdy) > t)
        return size, pos

    def _in_flow(self, i):
        return self.a.style(i, "position") not in ("absolute", "fixed")

    def _close(self, x, y):
        return abs(x - y) <= self.te

    def _explain(self):
        a, b, d = self.a, self.b, self.d
        te, tol = self.te, self.tol
        order = sorted(self.pairs)
        self.why = {}  # ia -> {aspect: explanation or None (root)}
        missing_or_extra_parent = {}
        for i in self.missing:
            if a.area(i):
                missing_or_extra_parent.setdefault(a.nodes[i]["pa"], []).append(("a", i))
        for j in self.extra:
            if b.area(j):
                missing_or_extra_parent.setdefault(("b", b.nodes[j]["pa"]), []).append(("b", j))

        # Height: bottom-up (children first).
        hcon = {}
        for ia in reversed(order):
            dh = d[ia][3]
            if abs(dh) <= tol:
                continue
            kids = [c for c in a.children[ia] if c in self.pairs and self._in_flow(c)]
            explained = None
            ext_a = self._extent(a, ia, kids, None)
            ext_b = self._extent(b, self.pairs[ia], [self.pairs[c] for c in kids], None)
            if ext_a is not None and ext_b is not None and abs((ext_b - ext_a) - dh) <= te:
                # The child that ends lowest must not itself be stretched by this box.
                last = max(kids, key=lambda c: b.nodes[self.pairs[c]]["r"][1] + b.nodes[self.pairs[c]]["r"][3], default=None)
                # (A stretched child, like a table cell, explains it when its own content does.)
                if last is not None and (not self._stretched(ia, last) or hcon.get(last)):
                    # Named after the child whose height changed most (the last one if none did).
                    grew = max(kids, key=lambda c: abs(d[c][3]))
                    explained = "content (" + a.label(grew if abs(d[grew][3]) > tol else last) + ")"
            n = a.nodes[ia]
            if explained is None and "tx" in n:
                nb = b.nodes[self.pairs[ia]]
                if "th" in n and "th" in nb:
                    if abs((nb["th"] - n["th"]) - dh) <= te and abs(nb["th"] - n["th"]) > tol:
                        explained = "text"
                elif not any(abs(d[c][3]) > tol or abs(self.rel[c][1]) > tol for c in kids):
                    # No text rects on one side (Range.getClientRects): no element child grew or
                    # moved, so the own text did.
                    explained = "text"
            if explained is None and self._unmatched_kids(ia):
                explained = "missing/extra child"
            hcon[ia] = explained
        # Everything else top-down.
        for ia in order:
            size, pos = self.differs(ia)
            if not size and not pos:
                continue
            dx, dy, dw, dh = d[ia]
            p = a.nodes[ia]["pa"]
            why = {}
            pd = d.get(p) if p in self.pairs else None
            n, nb = a.nodes[ia], b.nodes[self.pairs[ia]]
            # Width.
            if abs(dw) > tol:
                from_parent = None
                if pd is not None and abs(pd[2]) > tol:
                    pw = a.nodes[p]["r"][2]
                    prop = pd[2] * (n["r"][2] / pw) if pw > 0 else pd[2]
                    if self._close(dw, pd[2]) or self._close(dw, prop):
                        from_parent = "parent width (" + a.label(p) + ")"
                from_content = self._width_from_content(ia)
                if self.stf[ia]:
                    w = from_content or from_parent
                else:
                    w = from_parent or (from_content if self._content_child_is_stf(ia) else None)
                if w is None and "tx" in n and self.stf[ia]:
                    w = "TEXT-WIDTH"
                if w is None:
                    w = self._from_containing_block(ia, 2)
                why["width"] = w
            # Height.
            if abs(dh) > tol:
                h = hcon.get(ia)
                if h is None and pd is not None and self._stretched(p, ia) and self._close(dh, pd[3]):
                    h = "parent height (" + a.label(p) + ")"
                if h is None:
                    h = self._from_containing_block(ia, 3)
                if h == "text":
                    if abs(dw) > tol or (pd is not None and abs(pd[2]) > tol):
                        h = "text rewrapped at another width"
                    else:
                        h = "TEXT-WRAP" if abs(dh) >= 0.5 * self._line_height(ia) else "TEXT-HEIGHT"
                why["height"] = h
            # Position.
            if pos:
                why["position"] = self._position_explained(ia, missing_or_extra_parent)
            self.why[ia] = why

    ATOMIC = {"img", "input", "button", "select", "textarea", "video", "canvas", "svg", "iframe", "object",
              "embed", "progress", "meter"}

    def _atomic_inline(self, ia):
        """An inline-level box aligned on the baseline that is not an inline box: inline-block and
        friends, or a replaced element / form control."""
        a = self.a
        disp = a.style(ia, "display")
        if a.style(ia, "vertical-align") not in ("baseline", ""):
            return False
        return disp in ("inline-block", "inline-flex", "inline-grid", "inline-table") or (
            disp == "inline" and a.nodes[ia]["t"] in self.ATOMIC)

    def _inline_box_only(self, ia):
        """An inline box (or <br>) whose rect differs while its contents do not, and whose own text (if
        any) neither moved nor got wider: its getBoundingClientRect() or font content area differs,
        nothing visible did."""
        a, d, tol = self.a, self.d, self.tol
        if a.style(ia, "display") != "inline" and a.nodes[ia]["t"] != "br":
            return False
        if "tx" in a.nodes[ia] and (abs(d[ia][2]) > tol or self.differs(ia)[1]):
            return False  # its own text is wider or moved
        return all(not any(self.differs(c)) for c in a.children[ia] if c in self.pairs)

    def _from_containing_block(self, ia, k):
        """An absolutely positioned box whose width (k=2) or height (k=3) changed with its containing
        block's (insets or a percentage size against it)."""
        if self.a.style(ia, "position") not in ("absolute", "fixed"):
            return None
        ref = self._reference(ia)
        if ref is None or abs(self.d[ref][k]) <= self.tol:
            return None
        rw = self.a.nodes[ref]["r"][k]
        prop = self.d[ref][k] * (self.a.nodes[ia]["r"][k] / rw) if rw > 0 else self.d[ref][k]
        if self._close(self.d[ia][k], self.d[ref][k]) or self._close(self.d[ia][k], prop):
            return "containing block (" + self.a.label(ref) + ")"
        return None

    def _line_height(self, ia):
        a = self.a
        lh, fs = a.style(ia, "line-height"), a.style(ia, "font-size")
        try:
            if lh.endswith("px"):
                return float(lh[:-2])
            f = float(fs[:-2]) if fs.endswith("px") else 16.0
            return f * float(lh) if re.fullmatch(r"[\d.]+", lh) else f * 1.2
        except ValueError:
            return 19.2

    def _source(self, ia):
        """Follows the largest small height change down from `ia` to where it starts."""
        a, d = self.a, self.d
        while True:
            kids = [c for c in a.children[ia] if c in self.pairs and abs(d[c][3]) > 0.05]
            if not kids:
                return ia
            ia = max(kids, key=lambda c: abs(d[c][3]))

    def _extent(self, side, i, kids, _):
        top = side.nodes[i]["r"][1]
        # Boxes with a width or a height count: an empty full-width block still marks where the content
        # ends (and an inline box that should be a block has no width but its height).
        bottoms = [side.nodes[c]["r"][1] + side.nodes[c]["r"][3] for c in kids
                   if side.nodes[c]["r"][2] > 0 or side.nodes[c]["r"][3] > 0]
        if not bottoms:
            return None
        return max(bottoms) - top

    def _stretched(self, parent, child):
        a = self.a
        pd = a.style(parent, "display")
        cd = a.style(child, "display")
        if cd == "table-cell":
            return True
        align = a.style(child, "align-self")
        if align in ("auto", "normal", ""):
            align = a.style(parent, "align-items")
        stretch = align in ("normal", "stretch", "")
        if pd in ("flex", "inline-flex"):
            return stretch and a.style(parent, "flex-direction") in ("row", "row-reverse", "")
        if pd in ("grid", "inline-grid"):
            return stretch
        return False

    def _width_from_content(self, ia):
        a, b, d = self.a, self.b, self.d
        kids = [c for c in a.children[ia] if c in self.pairs and self._in_flow(c)]
        vis = [c for c in kids if a.area(c)]
        if not vis:
            return None

        def ext(side, items):
            boxes = [side.nodes[c]["r"] for c in items if side.area(c)]
            if not boxes:
                return None
            return max(r[0] + r[2] for r in boxes) - min(r[0] for r in boxes)

        ea, eb = ext(a, vis), ext(b, [self.pairs[c] for c in vis])
        if ea is None or eb is None:
            return None
        if abs((eb - ea) - d[ia][2]) <= self.te and abs(eb - ea) > self.tol:
            widest = max(vis, key=lambda c: abs(d[c][2]))
            return "content (" + a.label(widest) + ")"
        return None

    def _content_child_is_stf(self, ia):
        d = self.d
        return any(
            c in self.pairs and self.stf[c] and abs(d[c][2] - d[ia][2]) <= self.te for c in self.a.children[ia]
        )

    def _unmatched_kids(self, ia):
        a, b = self.a, self.b
        ib = self.pairs[ia]
        return any(a.area(i) and a.nodes[i]["pa"] == ia for i in self.missing) or any(
            b.area(j) and b.nodes[j]["pa"] == ib for j in self.extra
        )

    def _position_explained(self, ia, unmatched):
        a, d = self.a, self.d
        rdx, rdy = self.rel[ia]
        dw, dh = d[ia][2], d[ia][3]
        p = a.nodes[ia]["pa"]
        if p < 0 or p not in self.pairs:
            return None
        siblings = a.children[p]
        k = siblings.index(ia)
        prev = None
        for s in reversed(siblings[:k]):
            if s in self.pairs and a.area(s) and self._in_flow(s):
                prev = s
                break
        # A missing or extra visible sibling before this one.
        for side, i in unmatched.get(p, []) + unmatched.get(("b", self.pairs[p]), []):
            if side == "a" and i < ia and (prev is None or i > prev):
                return "missing sibling (" + a.label(i) + ")"
            if side == "b":
                jb = self.pairs[ia]
                jprev = self.pairs[prev] if prev is not None else -1
                if jprev < i < jb:
                    return "extra sibling (" + self.b.label(i) + ")"
        pd = d[p]
        if prev is not None:
            sx, sy = self.rel[prev]
            sd = d[prev]
            px = [sx, sx + sd[2], sx + sd[2] / 2]
            py = [sy, sy + sd[3], sy + sd[3] / 2, sy + (sd[3] - dh) / 2]
            if any(self._close(rdx, v) for v in px) and any(self._close(rdy, v) for v in py):
                if any(abs(v) > self.tol for v in (sx, sy, sd[2], sd[3])):
                    return "previous sibling (" + a.label(prev) + ")"
                if max(abs(sd[2]), abs(sd[3])) > 0.05 and (abs(rdx - sx - sd[2]) < abs(rdx) or abs(rdy - sy - sd[3]) < abs(rdy)):
                    return "ACCUMULATED"
        cx = [pd[2], pd[2] / 2, (pd[2] - dw) / 2, pd[2] - dw, -dw, -dw / 2]
        cy = [pd[3], pd[3] / 2, (pd[3] - dh) / 2, pd[3] - dh, -dh, -dh / 2]
        okx = abs(rdx) <= self.te or any(self._close(rdx, v) for v in cx)
        oky = abs(rdy) <= self.te or any(self._close(rdy, v) for v in cy)
        if okx and oky:
            if abs(rdx) <= self.te and abs(rdy) <= self.te:
                return "within the explanation tolerance"
            return "parent size (" + a.label(p) + ")"
        return None

    # Results.

    def roots(self, limit=None):
        """Root causes in document order (similar ones grouped), the first `limit` of them as items."""
        a, b = self.a, self.b
        found = []  # (a index or None, b index or None, kinds, why override)
        for ia in sorted(self.why):
            why = self.why[ia]
            kinds = []
            if "width" in why and why["width"] in (None, "TEXT-WIDTH"):
                kinds.append("text-width" if why["width"] == "TEXT-WIDTH" else "size")
            if "height" in why and why["height"] in (None, "TEXT-WRAP", "TEXT-HEIGHT"):
                kinds.append({"TEXT-WRAP": "text-wrap", "TEXT-HEIGHT": "text-height"}.get(why["height"], "size"))
            if "position" in why and why["position"] in (None, "ACCUMULATED"):
                kinds.append("accumulated" if why["position"] else "position")
            kinds = list(dict.fromkeys(kinds))
            if kinds and self._inline_box_only(ia):
                kinds = ["inline-box"]
            if kinds == ["position"] and self._atomic_inline(ia) and abs(self.rel[ia][0]) <= self.tol:
                kinds = ["baseline"]
            if kinds and set(kinds) <= {"text-height", "text-wrap", "size"} and abs(self.d[ia][2]) <= self.tol:
                # A line box that changed height while the atomic inlines on it kept their size and
                # their offset from the line's top: the text's baseline moved relative to them, so
                # their baseline (CSS 2 10.8.1, vertical-align) is the suspect.
                atom = next((c for c in a.children[ia] if c in self.pairs and self._atomic_inline(c)
                             and not any(self.differs(c))), None)
                if atom is not None:
                    why = {"position": "unchanged in its line, while the line box of " + a.label(ia)
                           + " changed height by %.2f px around it" % self.d[ia][3]}
                    found.append((atom, self.pairs[atom], ["baseline"], why))
                    continue
            if kinds:
                found.append((ia, self.pairs[ia], kinds, None))
        found += [(i, None, ["missing"], None) for i in self.missing if a.shown(i)]
        found += [(None, j, ["extra"], None) for j in self.extra if b.shown(j)]
        found.sort(key=self._order)
        # The same difference on many similar elements (list items, table rows): listed once.
        grouped, seen = [], {}
        for entry in found:
            ia, ib, kinds, why = entry
            n = a.nodes[ia] if ia is not None else b.nodes[ib]
            delta = self.d[ia] if ia is not None and ib is not None else [0, 0, 0, 0]
            why = why if why is not None else self.why.get(ia, {})
            key = (tuple(kinds), n["t"], n["c"], round(delta[2]), round(delta[3]),
                   tuple(sorted((k, re.sub(r"\(.*", "", str(v))) for k, v in why.items())))
            if key in seen:
                seen[key][1] += 1
                continue
            seen[key] = [entry, 1]
            grouped.append(seen[key])
        out = []
        for (ia, ib, kinds, why), repeats in grouped[:limit]:
            item = self._item(ia, ib, kinds)
            if why is not None:
                item["why"] = why
            item["repeats"] = repeats
            if "accumulated" in kinds:
                sib = a.children[a.nodes[ia]["pa"]]
                prev = max((s for s in sib if s < ia and s in self.pairs and a.area(s)), default=None)
                src = self._source(prev) if prev is not None else ia
                item["source"] = self._item(src, self.pairs[src], ["text-height"])
                item["source"]["props"] = self.involved(src, self.pairs[src], ["text-height", "size"])
            out.append(item)
        return out

    def raw(self, limit=None):
        """Differences in document order, ancestor first; an element under a differing ancestor is kept
        only when its own size differs by more than that ancestor's."""
        a, d, tol = self.a, self.d, self.tol
        listed = {}
        found = []
        for ia in sorted(self.pairs):
            size, pos = self.differs(ia)
            if not size and not pos:
                continue
            anc = a.nodes[ia]["pa"]
            while anc >= 0 and anc not in listed:
                anc = a.nodes[anc]["pa"]
            listed[ia] = True
            if anc >= 0:
                if not (abs(d[ia][2]) > abs(d[anc][2]) + tol or abs(d[ia][3]) > abs(d[anc][3]) + tol):
                    continue
            found.append((ia, self.pairs[ia], (["size"] if size else []) + (["position"] if pos else []), None))
        found += [(i, None, ["missing"], None) for i in self.missing if a.shown(i)]
        found += [(None, j, ["extra"], None) for j in self.extra if self.b.shown(j)]
        found.sort(key=self._order)
        return [self._item(ia, ib, kinds) for ia, ib, kinds, _ in found[:limit]]

    def _order(self, entry):
        ia, ib = entry[0], entry[1]
        if ia is not None:
            return (ia, 0)
        # An extra element sorts after its matched parent.
        if not hasattr(self, "_inv"):
            self._inv = {y: x for x, y in self.pairs.items()}
        return (self._inv.get(self.b.nodes[ib]["pa"], 0), 1 + ib)

    def _item(self, ia, ib, kinds):
        a, b = self.a, self.b
        item = {
            "kinds": kinds,
            "a": ia,
            "b": ib,
            "path": a.nodes[ia]["p"] if ia is not None else b.nodes[ib]["p"],
            "label": a.label(ia) if ia is not None else b.label(ib),
            "chrome": a.nodes[ia]["r"] if ia is not None else None,
            "snowlark": b.nodes[ib]["r"] if ib is not None else None,
            "why": self.why.get(ia, {}) if ia is not None else {},
        }
        if ia is not None and ib is not None:
            item["delta"] = [round(v, 2) for v in self.d[ia]]
            item["rel"] = [round(v, 2) for v in self.rel[ia]]
            item["text"] = {
                "chrome": [a.nodes[ia].get("th"), a.nodes[ia].get("tn")],
                "snowlark": [b.nodes[ib].get("th"), b.nodes[ib].get("tn")],
            }
        item["styles"] = self.styles(ia, ib)
        item["props"] = self.involved(ia, ib, kinds)
        return item

    def styles(self, ia, ib):
        """[(prop, chrome value, snowlark value, differs)] for the element and its parent."""
        a, b = self.a, self.b
        rows = []
        for prop in a.snap["props"] if ia is not None else b.snap["props"]:
            va = a.style(ia, prop) if ia is not None else None
            vb = b.style(ib, prop) if ib is not None else None
            rows.append(self._row(prop, va, vb))
        pa = a.nodes[ia]["pa"] if ia is not None else -1
        if pa >= 0:
            pb = self.pairs.get(pa)
            for prop in ("display", "container-type", "flex-direction", "flex-wrap", "justify-content", "align-items",
                         "grid-template-columns", "grid-template-rows", "text-align", "line-height", "font-size"):
                rows.append(self._row("parent:" + prop, a.style(pa, prop), b.style(pb, prop) if pb is not None else None))
        return rows

    def _row(self, prop, va, vb):
        base = prop.split(":")[-1]
        differs = (
            va is not None and vb is not None and base not in self.unsupported and norm(va, base) != norm(vb, base)
        )
        if differs and base in GEOMETRY:
            try:
                differs = abs(float(va.rstrip("px")) - float(vb.rstrip("px"))) > self.tol
            except ValueError:
                pass
        return (prop, va, vb, differs)

    def involved(self, ia, ib, kinds):
        """The CSS properties to look at: differing computed values (not the resolved geometry, which
        mirrors the boxes), then the kind's deciding properties that have non-initial values."""
        rows = {r[0]: r for r in self.styles(ia, ib)}
        diff = [p for p, r in rows.items() if r[3] and p.split(":")[-1] not in GEOMETRY]
        rel = [p for k in kinds for p in ALWAYS.get(k, []) if p in rows and p not in diff]
        rel = list(dict.fromkeys(rel))
        for k in kinds:
            for p in RELEVANT.get(k, []):
                r = rows.get(p)
                if r is None or p in diff or p in rel:
                    continue
                v = r[1] if r[1] is not None else r[2]
                if v and not is_initial(p.split(":")[-1], v) and p.split(":")[-1] not in GEOMETRY:
                    rel.append(p)
        return {
            "differing": [(p, rows[p][1], rows[p][2]) for p in diff],
            "relevant": [(p, rows[p][1] if rows[p][1] is not None else rows[p][2]) for p in rel],
        }

    def summary(self):
        return {
            "chrome_elements": len(self.a.nodes),
            "snowlark_elements": len(self.b.nodes),
            "matched": len(self.pairs),
            "missing": sum(1 for i in self.missing if self.a.shown(i)),
            "extra": sum(1 for j in self.extra if self.b.shown(j)),
            "differing": sum(1 for ia in self.pairs if any(self.differs(ia))),
            "unsupported_props": self.unsupported,
        }
