# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Offline tests of the sweep's parsing and grouping: `python -m unittest discover tools/sweep`."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import cssparse  # noqa: E402
import layout  # noqa: E402
import scroll  # noqa: E402
import supports  # noqa: E402


def events(text):
    return list(cssparse.scan(text))


class ScanTest(unittest.TestCase):
    def test_declarations_and_selectors(self):
        ev = events("a, b > c:hover { color: red; margin: 0 auto !important }")
        self.assertIn(("selector", "a", False), ev)
        self.assertIn(("selector", "b > c:hover", False), ev)
        self.assertIn(("decl", "color", "red"), ev)
        self.assertIn(("decl", "margin", "0 auto"), ev)

    def test_comments_strings_and_urls_do_not_break_blocks(self):
        ev = events('/* a { color: blue } */ a::before { content: "}{;"; background: url(data:x;y) } b { top: 0 }')
        self.assertNotIn(("decl", "color", "blue"), ev)
        self.assertIn(("decl", "content", '"}{;"'), ev)
        self.assertIn(("decl", "background", "url(data:x;y)"), ev)
        self.assertIn(("selector", "b", False), ev)
        self.assertIn(("decl", "top", "0"), ev)

    def test_at_rules(self):
        ev = events("@import url(x.css) screen and (min-width: 1px); @layer a, b;"
                    "@media (width >= 600px) { .x { display: grid } }"
                    "@font-face { font-family: f; src: url(f.woff2) }"
                    "@keyframes spin { from { transform: rotate(0) } to { transform: rotate(1turn) } }")
        self.assertIn(("at", "import", "url(x.css) screen and (min-width: 1px)"), ev)
        self.assertIn(("at", "layer", "a, b"), ev)
        self.assertIn(("at", "media", "(width >= 600px)"), ev)
        self.assertIn(("selector", ".x", False), ev)
        self.assertIn(("decl", "display", "grid"), ev)
        self.assertIn(("decl", "font-family", "f"), ev)
        self.assertIn(("decl", "transform", "rotate(1turn)"), ev)
        # Keyframe selectors are not selectors.
        self.assertFalse([e for e in ev if e[0] == "selector" and e[1] in ("from", "to")])

    def test_nesting(self):
        ev = events(".card { color: red; &:hover { color: blue } .title & { top: 0 } "
                    "@media (min-width: 1px) { left: 0 } padding: 1px }")
        self.assertIn(("selector", ".card", False), ev)
        self.assertIn(("selector", "&:hover", True), ev)
        self.assertIn(("selector", ".title &", True), ev)
        self.assertIn(("at", "media", "(min-width: 1px)"), ev)
        for d in (("decl", "color", "red"), ("decl", "color", "blue"), ("decl", "left", "0"),
                  ("decl", "padding", "1px")):
            self.assertIn(d, ev)

    def test_custom_properties_are_skipped(self):
        self.assertEqual(events(":root { --x: { a: b }; --y: 1px }"),
                         [("selector", ":root", False)])


class TokenTest(unittest.TestCase):
    def test_value_tokens(self):
        t = cssparse.value_tokens("width", "calc(100cqw - 2px) url('a(b).png') 'x(y)'")
        self.assertIn("fn:calc()", t)
        self.assertIn("unit:cqw", t)
        self.assertIn("unit:px", t)
        self.assertNotIn("fn:a()", t)
        self.assertNotIn("fn:x()", t)
        self.assertEqual(cssparse.value_tokens("text-wrap", "balance"), ["kw:text-wrap:balance"])
        self.assertEqual(cssparse.value_tokens("color", "#fff"), [])

    def test_selector_tokens(self):
        t = cssparse.selector_tokens('a:has(> img):not([href^=":x"])::-webkit-scrollbar')
        self.assertEqual(t, ["attr:^=", "pseudo::has()", "pseudo::not()", "pseudo:::-webkit-scrollbar"])
        self.assertIn("nesting:&", cssparse.selector_tokens("&:hover"))

    def test_media_tokens(self):
        self.assertEqual(supports.media_tokens("(width >= 600px)"), ["mf:width", "media:range-syntax"])
        self.assertEqual(supports.media_tokens("screen and (prefers-reduced-motion: reduce)"),
                         ["mf:prefers-reduced-motion", "media:screen"])

    def test_value_shape(self):
        self.assertEqual(cssparse.value_shape("1px solid #abcdef"), "Npx solid #hex")


class BlameTest(unittest.TestCase):
    def test_a_token_that_never_passes_is_the_feature(self):
        fails = [
            {"kind": "decl", "text": "width: calc(10cqw + 1px)", "tokens": ["fn:calc()", "unit:cqw", "unit:px"],
             "sites": ["a"]},
            {"kind": "decl", "text": "height: 5cqw", "tokens": ["unit:cqw"], "sites": ["b"]},
        ]
        groups = supports.blame(fails, {"fn:calc()": 10, "unit:px": 100})
        self.assertEqual(list(groups), ["unit:cqw"])
        self.assertEqual(groups["unit:cqw"]["sites"], {"a", "b"})
        self.assertEqual(groups["unit:cqw"]["items"], 2)

    def test_a_combination_that_fails_is_its_own_group(self):
        fails = [{"kind": "decl", "text": "grid-area: 1 / 2", "tokens": [], "sites": ["a"]}]
        self.assertEqual(list(supports.blame(fails, {})), ["decl:grid-area: N / N"])
        fails = [{"kind": "decl", "text": "x: foo(1px)", "tokens": ["fn:foo()", "unit:px"], "sites": ["a"]}]
        groups = supports.blame(fails, {"fn:foo()": 3, "unit:px": 50})
        self.assertEqual(list(groups), ["decl:x: foo(Npx)"])

    def test_failures_and_property_gaps(self):
        u = supports.Usage()
        u.add_text("s1", "a { scroll-snap-type: x mandatory; width: 1cqw } b:has(i) { color: red }")
        u.add_text("s2", "a { scroll-snap-type: y; width: 1px }")
        items = supports.build_items(u)
        items["prop"] = ["scroll-snap-type", "width"]
        ok_all = {k: [1] * len(items[k]) for k in ("decl", "sel", "media", "prop")}
        ok_all["rule"] = [[1, "CSSRule", -1]] * len(items["rule"])
        ours = {k: list(v) for k, v in ok_all.items()}
        ours["prop"] = [0, 1]
        for n, (p, v) in enumerate(items["decl"]):
            if p == "scroll-snap-type" or "cqw" in v:
                ours["decl"][n] = 0
        fails = supports.failures(items, ok_all, ours, u)
        groups = supports.blame(fails, supports.pass_counts(items, ok_all, ours))
        self.assertEqual(groups["prop:scroll-snap-type"]["sites"], {"s1", "s2"})
        self.assertEqual(groups["unit:cqw"]["sites"], {"s1"})
        gaps = supports.property_gaps(items, ours, u)
        self.assertEqual(gaps, [{"property": "scroll-snap-type", "sites": ["s1", "s2"]}])


def root(kinds, styles, differing=(), label="div.x"):
    return {"kinds": kinds, "label": label, "delta": [0, 0, 10, 0],
            "styles": [(p, v, v, False) for p, v in styles.items()],
            "props": {"differing": list(differing), "relevant": []}}


class LayoutTest(unittest.TestCase):
    def test_context(self):
        self.assertEqual(layout.context(root(["size"], {"display": "grid",
                                                         "grid-template-columns": "subgrid"})), "subgrid")
        self.assertEqual(layout.context(root(["size"], {"display": "block", "parent:display": "inline-grid"})),
                         "grid item")
        self.assertEqual(layout.context(root(["size"], {"display": "block", "parent:display": "flex",
                                                         "parent:flex-direction": "column"})),
                         "flex item (column)")
        self.assertEqual(layout.context(root(["position"], {"display": "block", "position": "sticky"})),
                         "position: sticky")
        self.assertEqual(layout.context(root(["size"], {"display": "block", "overflow-x": "hidden"})), "overflow")
        self.assertEqual(layout.context(root(["size"], {"display": "block"})), "block flow")

    def test_group_key(self):
        self.assertEqual(layout.group_key(root(["missing"], {}, label="img.logo"), "missing"),
                         "content: missing <img>")
        r = root(["size"], {"display": "block"}, differing=[("max-height", "540px", "none")])
        self.assertEqual(layout.group_key(r, "size"), "value: max-height")
        self.assertEqual(layout.group_key(root(["text-wrap"], {"display": "block"}), "text-wrap"),
                         "text: text-wrap")
        self.assertEqual(layout.group_key(root(["size"], {"display": "flex"}), "size"), "size: flex container")

    def test_group_ranks_by_sites(self):
        grid = root(["size"], {"display": "block", "parent:display": "grid"})
        flex = root(["size"], {"display": "flex"})
        rows = layout.group({"a": [grid, flex], "b": [grid], "c": [grid, grid]}, {"a": 1, "b": 0, "c": None})
        self.assertEqual(rows[0]["key"], "size: grid item")
        self.assertEqual(rows[0]["sites"], ["a", "b", "c"])
        self.assertEqual(rows[0]["first"], ["b"])
        self.assertEqual(rows[0]["roots"], 4)
        self.assertEqual(rows[1]["key"], "size: flex container")
        self.assertEqual(rows[1]["first"], ["a"])


class ScrollTest(unittest.TestCase):
    def test_parse_log(self):
        lines = [
            "[tab 1] raster 1440x900 band 0..900: 3.1 ms (4 tiles, 4 painted, 0 reused, 2 layers, pinned, 0 compositor animations; ...)\n",
            "[tab 1] raster 1440x900 band 0..900: 2.0 ms (4 tiles, 1 painted, 3 reused, 2 layers, 0 compositor animations; ...)\n",
            "[gpu] frame 7 interval 16.60 wait 0.10 compose 0.50 present 0.20 ms page 1440x5000 area 1440x820\n",
            "[gpu] frame 8 interval 16.60 wait 0.10 compose 0.50 present 0.20 ms page 1440x5000 area 1440x820 UNCOVERED 41 rows at scroll 300\n",
            "[gpu] scroll: 2 frames, interval p50 16.60 p95 16.60 max 16.60 ms\n",
        ]
        r = scroll.parse_log(lines)
        self.assertEqual((r["rasters"], r["pinned"], r["frames"]), (2, 1, 2))
        self.assertEqual((r["uncovered_frames"], r["uncovered_rows"], r["worst_rows"]), (1, 41, 41))
        self.assertEqual(r["pinned_share"], 0.5)
        self.assertEqual(r["uncovered_share"], round(41 / 1640, 4))

    def test_rank(self):
        rows = [
            {"site": "a", "pinned_share": 0.0, "uncovered_share": 0.2},
            {"site": "b", "pinned_share": 1.0, "uncovered_share": 0.0},
            {"site": "c", "error": "no window"},
        ]
        self.assertEqual([r["site"] for r in scroll.rank(rows)], ["b", "a"])


if __name__ == "__main__":
    unittest.main()
