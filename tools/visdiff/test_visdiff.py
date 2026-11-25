# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Tests of visdiff's matching, classification and ranking on hand-made snapshots (no browser needed), and
of the memory cap on the processes a run starts.

    python tools/visdiff/test_visdiff.py
"""

import os
import subprocess
import sys
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import findings  # noqa: E402
import limits  # noqa: E402


def node(pa, p, t="div", r=(0, 0, 100, 20), k="", **kw):
    n = {"pa": pa, "p": p, "t": t, "id": "", "c": "", "r": list(r), "k": k, "d": "block", "v": "visible",
         "op": 1, "eo": 1}
    n.update(kw)
    return n


def page(nodes, width=1280):
    return {"nodes": nodes, "viewport": [width, 800], "scroll": [width, 3000], "url": "https://a.test/",
            "title": "t", "text": ""}


def base(extra_chrome=(), extra_ours=()):
    """html > body, then the given children of body (index 1)."""
    c = [node(-1, "/html[1]", "html", (0, 0, 1280, 3000)), node(0, "/html[1]/body[1]", "body", (0, 0, 1280, 3000))]
    o = [dict(n) for n in c]
    c += [dict(n) for n in extra_chrome]
    o += [dict(n) for n in extra_ours]
    return page(c), page(o)


B = "/html[1]/body[1]"


class Matching(unittest.TestCase):
    def test_same_path(self):
        c, o = base([node(1, B + "/p[1]", "p", k="t", tx="aa")], [node(1, B + "/p[1]", "p", k="t", tx="aa")])
        fwd, _ = findings.build_mapping(findings.Snapshot(c), findings.Snapshot(o))
        self.assertEqual(fwd[2], 2)

    def test_shifted_path_matches_by_text(self):
        # Snowlark has an extra paragraph first, so the same text sits at p[2].
        c, o = base([node(1, B + "/p[1]", "p", k="t", tx="aa")],
                    [node(1, B + "/p[1]", "p", k="t", tx="zz"), node(1, B + "/p[2]", "p", k="t", tx="aa")])
        fwd, _ = findings.build_mapping(findings.Snapshot(c), findings.Snapshot(o))
        self.assertEqual(fwd[2], 3)

    def test_id_and_image_source(self):
        c, o = base([node(1, B + "/div[1]", id="x"), node(1, B + "/img[1]", "img", k="i", src="https://a/b.png")],
                    [node(1, B + "/div[3]", id="x"), node(1, B + "/img[4]", "img", k="i", src="https://a/b.png")])
        fwd, _ = findings.build_mapping(findings.Snapshot(c), findings.Snapshot(o))
        self.assertEqual(fwd[2], 2)
        self.assertEqual(fwd[3], 3)

    def test_incompatible_text_does_not_match(self):
        c, o = base([node(1, B + "/p[1]", "p", k="t", tx="aa")], [node(1, B + "/p[1]", "p", k="t", tx="bb")])
        fwd, _ = findings.build_mapping(findings.Snapshot(c), findings.Snapshot(o))
        self.assertNotIn(2, fwd)


class Classification(unittest.TestCase):
    def one(self, c, o):
        fs = findings.compare(c, o)
        self.assertEqual(len(fs), 1, fs)
        return fs[0]

    def test_shown_in_both_is_no_finding(self):
        c, o = base([node(1, B + "/p[1]", "p", k="t", tx="aa")], [node(1, B + "/p[1]", "p", k="t", tx="aa")])
        self.assertEqual(findings.compare(c, o), [])

    def test_stuck_animation_groups_under_the_animated_ancestor(self):
        card_c = node(1, B + "/article[1]", "article", (0, 0, 400, 300), an="rise", c="card")
        card_o = node(1, B + "/article[1]", "article", (0, 0, 400, 300), an="rise", c="card", op=0, eo=0)
        kids_c = [node(2, B + "/article[1]/h2[1]", "h2", (0, 0, 400, 40), k="t", tx="h"),
                  node(2, B + "/article[1]/img[1]", "img", (0, 40, 400, 260), k="i", src="https://a/x.webp")]
        kids_o = [dict(kids_c[0], eo=0), dict(kids_c[1], eo=0)]
        c, o = base([card_c] + kids_c, [card_o] + kids_o)
        f = self.one(c, o)
        self.assertEqual(f["group"], "animation")
        self.assertEqual(f["element"], "article.card")
        self.assertEqual(f["elements"], 2)
        self.assertEqual(f["area"], 400 * 304)  # 16 px cells: 25 x 19, nested content counted once

    def test_animation_only_chrome_applies(self):
        c, o = base([node(1, B + "/div[1]", k="b", src="https://a/x.png", an="enter")],
                    [node(1, B + "/div[1]", k="b", src="https://a/x.png", op=0, eo=0)])
        f = self.one(c, o)
        self.assertEqual(f["group"], "animation")
        self.assertIn("Chrome applies animation `enter`", f["detail"])

    def test_class_difference(self):
        c, o = base([node(1, B + "/div[1]", k="t", tx="a", c="reveal is-visible")],
                    [node(1, B + "/div[1]", k="t", tx="a", c="reveal", op=0, eo=0)])
        f = self.one(c, o)
        self.assertEqual(f["group"], "class")
        self.assertIn("class +is-visible", f["detail"])

    def test_data_attribute_on_an_ancestor_explains_a_missing_animation(self):
        c, o = base([node(1, B + "/section[1]", "section", da="data-in=true"),
                     node(2, B + "/section[1]/div[1]", k="b", src="https://a/x.png", an="enter")],
                    [node(1, B + "/section[1]", "section"),
                     node(2, B + "/section[1]/div[1]", k="b", src="https://a/x.png", op=0, eo=0)])
        f = self.one(c, o)
        self.assertEqual(f["group"], "class")
        self.assertIn("on section: attributes +data-in=true", f["detail"])
        self.assertIn("Chrome applies animation `enter`", f["detail"])

    def test_tracking_ids_that_differ_are_not_script_state(self):
        # An id or timestamp differs on every load; it does not say why the content is hidden.
        c, o = base([node(1, B + "/section[1]", "section", da="data-trace=97fc41e3-3697 data-at=1795715186574"),
                     node(2, B + "/section[1]/div[1]", k="b", src="https://a/x.png", an="enter")],
                    [node(1, B + "/section[1]", "section", da="data-trace=1a2b3c4d-0001 data-at=1795715199999"),
                     node(2, B + "/section[1]/div[1]", k="b", src="https://a/x.png", op=0, eo=0)])
        self.assertEqual(self.one(c, o)["group"], "animation")

    def test_display_none_hides_the_subtree(self):
        # Snowlark does not walk under display none, so the paragraph has no counterpart.
        c, o = base([node(1, B + "/div[1]", c="menu"), node(2, B + "/div[1]/p[1]", "p", k="t", tx="a")],
                    [node(1, B + "/div[1]", c="menu", d="none")])
        f = self.one(c, o)
        self.assertEqual(f["group"], "display-none")
        self.assertEqual(f["element"], "div.menu")

    def test_missing_groups_under_the_parent(self):
        c, o = base([node(1, B + "/main[1]", "main"), node(2, B + "/main[1]/p[1]", "p", k="t", tx="a"),
                     node(2, B + "/main[1]/p[2]", "p", (0, 30, 100, 20), k="t", tx="b")],
                    [node(1, B + "/main[1]", "main")])
        f = self.one(c, o)
        self.assertEqual((f["group"], f["element"], f["elements"]), ("missing", "main", 2))

    def test_image_not_loaded(self):
        c, o = base([node(1, B + "/img[1]", "img", (0, 0, 300, 200), k="i", src="https://a/b.avif")],
                    [node(1, B + "/img[1]", "img", (0, 0, 300, 200), k="I", src="https://a/b.avif")])
        f = self.one(c, o)
        self.assertEqual(f["group"], "image")
        self.assertIn("(avif)", f["detail"])

    def test_zero_size_and_clipped(self):
        c, o = base([node(1, B + "/p[1]", "p", k="t", tx="a")], [node(1, B + "/p[1]", "p", (0, 0, 100, 0), k="t", tx="a")])
        self.assertEqual(self.one(c, o)["group"], "zero-size")
        c, o = base([node(1, B + "/div[1]", ov=1), node(2, B + "/div[1]/p[1]", "p", k="t", tx="a")],
                    [node(1, B + "/div[1]", "div", (0, 0, 100, 0), ov=1),
                     node(2, B + "/div[1]/p[1]", "p", k="t", tx="a", cl=[0, 0, 100, 0])])
        f = self.one(c, o)
        self.assertEqual((f["group"], f["element"]), ("clipped", "div"))

    def test_carousel_on_another_slide_is_ignored(self):
        # Chrome shows slide 1 in view; Snowlark has moved on to slide 2, which sits where slide 1 was.
        def slides(x):
            return [node(1, B + "/div[1]", "div", (0, 0, 400, 100), ov=1),
                    node(2, B + "/div[1]/div[1]", "div", (x, 0, 400, 100), k="t", tx="a", c="slide"),
                    node(2, B + "/div[1]/div[2]", "div", (x + 400, 0, 400, 100), k="t", tx="b", c="slide")]
        c, o = base(slides(0), slides(-400))
        o["nodes"][3]["cl"] = o["nodes"][4]["cl"] = [0, 0, 400, 100]
        self.assertEqual(findings.compare(c, o), [])
        # Without a slide of the same kind there, slide 1 is reported.
        o["nodes"][4]["c"] = "other"
        self.assertEqual(self.one(c, o)["group"], "off-page")

    def test_consent_banner_is_left_out(self):
        c, o = base([node(1, B + "/div[1]", id="onetrust-banner-sdk"), node(2, B + "/div[1]/p[1]", "p", k="t", tx="a")],
                    [node(1, B + "/div[1]", id="onetrust-banner-sdk", d="none")])
        self.assertEqual(findings.compare(c, o), [])
        c["nodes"][2]["id"] = o["nodes"][2]["id"] = "news"
        self.assertEqual(self.one(c, o)["group"], "display-none")

    def test_not_shown_in_chrome_is_ignored(self):
        # Hidden, transparent, clipped away or below the region in Chrome: nothing to compare.
        for extra in ({"eo": 0}, {"v": "hidden"}, {"cl": [0, 0, 0, 0]}, {"r": [0, 2500, 100, 20]}):
            c, o = base([node(1, B + "/p[1]", "p", k="t", tx="a", **extra)], [])
            self.assertEqual(findings.compare(c, o), [], extra)

    def test_frames_are_left_out(self):
        c, o = base([node(1, B + "/iframe[1]", "iframe", (0, 0, 600, 400), k="g")], [])
        self.assertEqual(findings.compare(c, o), [])


class Ranking(unittest.TestCase):
    def test_findings_sorted_by_area_and_score(self):
        c, o = base([node(1, B + "/p[1]", "p", (0, 0, 100, 20), k="t", tx="a"),
                     node(1, B + "/p[2]", "p", (0, 100, 800, 400), k="t", tx="b")],
                    [node(1, B + "/p[1]", "p", k="t", tx="a", op=0, eo=0),
                     node(1, B + "/p[2]", "p", (0, 100, 800, 400), k="t", tx="b", op=0, eo=0)])
        fs = findings.compare(c, o)
        self.assertEqual([f["element"] for f in fs], ["p", "p"])
        self.assertGreater(fs[0]["area"], fs[1]["area"])
        total = sum(f["share"] for f in fs)
        self.assertAlmostEqual(findings.site_score(fs), round(total, 4))
        self.assertAlmostEqual(findings.site_score([dict(fs[0], group="missing")]), round(fs[0]["share"] / 2, 4))

    def test_rank_groups_leaves_doubtful_pages_out(self):
        f = {"group": "animation", "element": "a", "detail": "d", "share": 0.2, "elements": 1}
        g = {"group": "missing", "element": "b", "detail": "d", "share": 0.6, "elements": 1}
        sites = [{"url": "u1", "width": 1280, "findings": [f, g], "doubtful": False},
                 {"url": "u2", "width": 1280, "findings": [dict(f, share=0.3)], "doubtful": False},
                 {"url": "u3", "width": 1280, "findings": [dict(g, share=0.9)], "doubtful": True}]
        ranked = findings.rank_groups(sites)
        self.assertEqual([r["group"] for r in ranked], ["missing", "animation"])
        anim = ranked[1]
        self.assertEqual((anim["pages"], anim["share"]), (2, 0.5))
        self.assertEqual(anim["top"][0]["url"], "u2")

    def test_pixel_share_with_frame_masks(self):
        px = {"block": 8, "map": ["##..", "##..", "....", "...."]}
        self.assertEqual(findings.pixel_share(px), 0.25)
        # A frame over the left half: 8 of 16 blocks left, none of them content only in Chrome.
        self.assertEqual(findings.pixel_share(px, [[0, 0, 16, 32]]), 0.0)
        self.assertIsNone(findings.pixel_share(None))


@unittest.skipUnless(sys.platform == "win32", "job objects are Windows only")
class Limits(unittest.TestCase):
    # Grows 10 MB at a time, as a page whose memory runs away does.
    ALLOCATE = ("import time\nb = []\nwhile sum(map(len, b)) < %d: b.append(bytearray(10 * 2**20))\n"
                "time.sleep(0.2)\nprint('allocated')")

    def run_in_job(self, job, mb):
        p = subprocess.Popen([sys.executable, "-c", self.ALLOCATE % (mb * 2**20)], stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL)
        self.assertTrue(job.add(p.pid))
        out, _ = p.communicate(timeout=60)
        return p.returncode, out

    def test_memory_cap_stops_a_process_past_it(self):
        with limits.Job(300 * 2**20) as job:
            code, out = self.run_in_job(job, 600)
            self.assertNotEqual(code, 0)
            self.assertNotIn(b"allocated", out)
            self.assertTrue(job.over_cap())
        # Under the cap the same process runs, and the job reports its peak.
        with limits.Job(300 * 2**20) as job:
            code, out = self.run_in_job(job, 50)
            self.assertEqual(code, 0)
            self.assertIn(b"allocated", out)
            self.assertFalse(job.over_cap())
            self.assertGreater(job.peak_bytes(), 50 * 2**20)

    def test_run_ends_a_process_past_the_time_limit(self):
        t = time.time()
        r = limits.run([sys.executable, "-c", "import time; time.sleep(60)"], 1.0)
        self.assertEqual(r["exit"], "timeout")
        self.assertLess(time.time() - t, 30)
        r = limits.run([sys.executable, "-c", "print('done')"], 30.0, 300 * 2**20)
        self.assertEqual((r["exit"], r["out"].strip(), r["over_cap"]), (0, b"done", False))

    def test_closing_the_job_ends_its_processes(self):
        job = limits.Job()
        p = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        self.assertTrue(job.add(p.pid))
        job.close()
        deadline = time.time() + 10
        while p.poll() is None and time.time() < deadline:
            time.sleep(0.05)
        self.assertIsNotNone(p.poll())


if __name__ == "__main__":
    unittest.main()
