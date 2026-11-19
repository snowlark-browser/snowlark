# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Tests: the comparison on hand-made snapshots (no browser), and the two known bugs end to end (Chrome and
snowlark-cli on pages served from tests/pages by an HTTP server this test starts and stops).

    python tools/layout-diff/tests/test_layout_diff.py
"""

import argparse
import functools
import http.server
import os
import shutil
import sys
import tempfile
import threading
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))

import capture  # noqa: E402
import compare  # noqa: E402
import run  # noqa: E402
from chrome import CHROME  # noqa: E402

PROPS = ["display", "position", "float", "vertical-align", "align-self", "align-items", "flex-direction",
         "line-height", "font-size", "max-height"]
DEFAULTS = {"display": "block", "position": "static", "float": "none", "vertical-align": "baseline",
            "align-self": "auto", "align-items": "normal", "flex-direction": "row", "line-height": "20px",
            "font-size": "16px", "max-height": "none"}


class Snap:
    """Builds a walker-shaped snapshot: add() in document order."""

    def __init__(self):
        self.nodes = []

    def add(self, parent, tag, rect, text=None, **styles):
        st = dict(DEFAULTS)
        st.update({k.replace("_", "-"): v for k, v in styles.items()})
        n = {"pa": parent, "p": "", "t": tag, "id": "", "c": "", "r": list(rect),
             "s": [st[p] for p in PROPS]}
        if text:
            n["tx"], n["tl"] = "h" + text, len(text)
        self.nodes.append(n)
        return len(self.nodes) - 1

    def snap(self):
        return {"props": PROPS, "nodes": self.nodes, "url": "x", "scroll": [800, 600]}


def page(card_h, img_h, img_w=640, checkbox_y=0, extra=False):
    s = Snap()
    html = s.add(-1, "html", (0, 0, 800, 100 + card_h + 40))
    body = s.add(html, "body", (0, 0, 800, 100 + card_h + 40))
    s.add(body, "h1", (0, 0, 800, 40), text="Title")
    if extra:
        s.add(body, "div", (0, 40, 800, 30))
    off = 30 if extra else 0
    card = s.add(body, "div", (0, 40 + off, 800, card_h))
    s.add(card, "img", (80, 40 + off, img_w, img_h), display="block", max_height="540px" if img_h <= 540 else "none")
    s.add(body, "footer", (0, 40 + off + card_h, 800, 20), text="end")
    line = s.add(body, "p", (0, 60 + off + card_h, 800, 20), text="label")
    s.add(line, "input", (4, 63 + off + card_h + checkbox_y, 13, 13), display="inline-block")
    return s.snap()


class CompareTests(unittest.TestCase):
    def test_identical(self):
        d = compare.Diff(page(540, 540), page(540, 540))
        self.assertEqual(d.roots(), [])
        self.assertEqual(d.raw(), [])

    def test_child_size_is_the_root_not_its_ancestors(self):
        d = compare.Diff(page(540, 540), page(1152, 1152))
        roots = d.roots()
        self.assertEqual(roots[0]["label"], "img")
        self.assertEqual(roots[0]["kinds"], ["size"])
        self.assertIn(("max-height", "540px", "none"), [tuple(x) for x in roots[0]["props"]["differing"]])
        # The card grew because of the image; the footer and the line moved because of the card.
        self.assertEqual(len(roots), 1)
        self.assertTrue(roots[0]["why"]["height"] is None)
        # The raw list is ancestor first: <html> comes before the image.
        self.assertEqual(d.raw()[0]["label"], "html")

    def test_checkbox_moved_in_its_line(self):
        d = compare.Diff(page(540, 540), page(540, 540, checkbox_y=-3))
        roots = d.roots()
        self.assertEqual(len(roots), 1)
        self.assertEqual(roots[0]["label"], "input")
        self.assertEqual(roots[0]["kinds"], ["baseline"])
        self.assertIn("vertical-align", [p for p, _ in roots[0]["props"]["relevant"]])

    def test_missing_element_does_not_misalign_siblings(self):
        d = compare.Diff(page(540, 540, extra=True), page(540, 540))
        roots = d.roots()
        self.assertEqual([r["kinds"] for r in roots], [["missing"]])
        self.assertEqual(roots[0]["label"], "div")

    def test_tolerance(self):
        d = compare.Diff(page(540, 540), page(540.6, 540.6))
        self.assertEqual(d.roots(), [])
        d = compare.Diff(page(540, 540), page(542, 542), tol=3)
        self.assertEqual(d.roots(), [])

    def test_accumulated_sub_tolerance_differences(self):
        def rows(h):
            s = Snap()
            html = s.add(-1, "html", (0, 0, 800, 10 * h))
            for k in range(10):
                s.add(html, "div", (0, k * h, 800, h), text="row")
            return s.snap()

        roots = compare.Diff(rows(20), rows(20.4)).roots()
        self.assertEqual(roots[0]["kinds"], ["accumulated"])
        self.assertEqual(roots[0]["source"]["label"], "div")

    def test_overlay_follows_its_containing_block(self):
        # A dropdown: a block holding an inline-flex button, and a checkbox overlay sized to the block
        # (position: absolute; height: 100%). The line box grows around the unchanged button.
        def dropdown(h):
            s = Snap()
            html = s.add(-1, "html", (0, 0, 800, 100))
            box = s.add(html, "div", (10, 10, 32, h), position="relative")
            s.add(box, "input", (10, 10, 32, h), position="absolute", display="block")
            s.add(box, "label", (10, 10, 32, 32), display="inline-flex")
            return s.snap()

        roots = compare.Diff(dropdown(32), dropdown(35.65)).roots()
        self.assertEqual([(r["label"], r["kinds"]) for r in roots], [("label", ["baseline"])])

    def test_norm(self):
        self.assertEqual(compare.norm('"Times New Roman", serif'), compare.norm("Times New Roman,serif"))
        self.assertEqual(compare.norm("12.0001px"), "12px")
        self.assertEqual(compare.norm("auto", "min-width"), "0px")


def have_engines():
    return os.path.exists(CHROME) and os.path.exists(capture.default_snowlark())


@unittest.skipUnless(have_engines(), "needs Chrome and a built snowlark-cli.exe")
class KnownBugTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        handler = functools.partial(QuietHandler, directory=os.path.join(HERE, "pages"))
        cls.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.base = f"http://127.0.0.1:{cls.server.server_address[1]}/"
        cls.out = tempfile.mkdtemp(prefix="layout-diff-test-")

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        shutil.rmtree(cls.out, ignore_errors=True)

    def diff(self, name, width):
        args = argparse.Namespace(width=width, settle=300, timeout=20000, tol=1.0, first=10, max_nodes=20000,
                                  snowlark=capture.default_snowlark(), chrome=CHROME, serial=False,
                                  recompare=False)
        result = run.run_one(self.base + name, os.path.join(self.out, name), args)
        self.assertEqual(result["status"], "ok", result.get("warnings"))
        self.assertTrue(os.path.exists(os.path.join(self.out, name, "report.html")))
        return result

    def test_bug_24_checkbox(self):
        result = self.diff("checkbox.html", 800)
        first = run.headline(result["roots"])
        if first is None:
            self.skipTest("no difference: bug 24 is fixed")
        self.assertEqual(first["label"], "input")
        self.assertEqual(first["kinds"], ["baseline"])
        relevant = [p for p, _ in first["props"]["relevant"]]
        self.assertIn("vertical-align", relevant)
        self.assertIn("margin-bottom", relevant)
        self.assertTrue(os.path.exists(os.path.join(self.out, "checkbox.html", "side-by-side.png")))

    def test_bug_23_video_card(self):
        result = self.diff("video-card.html", 1593)
        first = run.headline(result["roots"])
        if first is None:
            self.skipTest("no difference: bug 23 is fixed")
        self.assertIn(first["label"], ("img.card-media-picture", "div.card-media.is-video.is-sized"))
        self.assertIn("size", first["kinds"])
        differing = [p for p, _, _ in first["props"]["differing"]]
        self.assertIn("max-height", differing)
        # The card's and the footer's changes are consequences, not further roots.
        labels = [r["label"] for r in result["roots"] if r["kinds"] != ["inline-box"]]
        self.assertNotIn("footer", labels)
        self.assertNotIn("article", labels)


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass


if __name__ == "__main__":
    unittest.main()
