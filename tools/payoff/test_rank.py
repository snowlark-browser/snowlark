# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Offline tests of the payoff ranking: `python -m unittest discover tools/payoff`."""

import json
import re
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import rank  # noqa: E402

W = dict(rank.DEFAULT_WEIGHTS)


def wpt_line(path, statuses, harness=0):
    return json.dumps({"url": "http://web-platform.test:8000/" + path, "ms": 1, "result": {
        "status": harness, "message": None,
        "tests": [{"name": f"t{i}", "status": s, "message": None} for i, s in enumerate(statuses)]}})


class SiteNameTest(unittest.TestCase):
    def test_one_name_across_tools(self):
        self.assertEqual(rank.site_name("www.apple.com"), "apple.com")
        self.assertEqual(rank.site_name("apple.com"), "apple.com")
        self.assertEqual(rank.site_name("https://www.google.com/maps"), "google.com_maps")
        self.assertEqual(rank.site_name("www.google.com_maps"), "google.com_maps")


class WptTest(unittest.TestCase):
    def setUp(self):
        self.files = rank.load_wpt([
            wpt_line("css/css-grid/a.html", [0, 1, 1]),
            wpt_line("css/css-grid/sub/b.html", [0, 0]),
            wpt_line("css/css-flexbox/c.html?x=1", [1]),
            json.dumps({"url": "http://web-platform.test:8000/dom/d.html",
                        "result": {"status": 2, "message": "timeout", "tests": []}}),
            "",
        ])

    def test_files_and_harness_failures(self):
        self.assertEqual(self.files["css/css-grid/a.html"], (2, 3))
        self.assertEqual(self.files["css/css-flexbox/c.html"], (1, 1))
        self.assertEqual(self.files["dom/d.html"], (1, 1))

    def test_prefixes_and_exclusions(self):
        self.assertEqual(rank.wpt_count(self.files, ["css/css-grid/"]), (2, 5))
        self.assertEqual(rank.wpt_count(self.files, ["css/"]), (3, 6))
        self.assertEqual(rank.wpt_count(self.files, ["css/", "css/css-grid/"]), (3, 6))  # counted once
        self.assertEqual(rank.wpt_count(self.files, ["css/", "!css/css-grid/sub/"]), (3, 4))

    def test_directories(self):
        dirs = rank.wpt_directories(self.files, 2)
        self.assertEqual(dirs[0], ("css/css-grid/", 2, 5))
        self.assertIn(("dom/", 1, 1), dirs)


class CensusTest(unittest.TestCase):
    def test_signals(self):
        rows = [
            {"key": "Navigator.prototype.share", "sites": ["a.com", "b.com"], "runtime": ["b.com"]},
            {"key": "window.foo", "sites": ["a.com"], "runtime": [], "generic": True},
            {"key": "Iterator.concat", "sites": ["c.com"], "runtime": [], "ecmascript": True},
        ]
        runtime = {"b.com": {"exceptions": [{"text": "Uncaught TypeError: x.share is not a function",
                                             "source": "b.js:1"}]},
                   "c.com": {"exceptions": []}}
        verdicts = {"b.com": "BLANK", "c.com": "hung"}
        sig, una = rank.census_signals(rows, runtime, verdicts, {"b.com": 0}, W)
        keys = {(s.key, s.site): s.weight for s in sig}
        self.assertEqual(keys[("api: Navigator.prototype.share", "a.com")], W["mention"])
        self.assertEqual(keys[("api: Navigator.prototype.share", "b.com")], W["broken"])
        self.assertNotIn(("api: window.foo", "a.com"), keys)  # a plain-word name is no evidence
        self.assertIn(("ecmascript: Iterator.concat", "c.com"), keys)
        self.assertIn(("exception: TypeError: x.share is not a function", "b.com"), keys)
        by_site = {u["site"]: u for u in una}
        self.assertEqual(by_site["b.com"]["explained_by"], ["Navigator.prototype.share"])
        self.assertEqual(by_site["c.com"]["verdict"], "hung")

    def test_a_failure_several_rows_match_is_shared(self):
        rows = [{"key": "A", "sites": ["x.com"], "runtime": ["x.com"]},
                {"key": "B", "sites": ["x.com"], "runtime": ["x.com"]},
                {"key": "C", "sites": ["y.com"], "runtime": ["y.com"]}]
        sig, _ = rank.census_signals(rows, {}, {}, {}, W)
        keys = {(s.key, s.site): s.weight for s in sig}
        self.assertEqual(keys[("api: A", "x.com")], W["throws"] / 2)
        self.assertEqual(keys[("api: C", "y.com")], W["throws"])

    def test_normalize_exception(self):
        self.assertEqual(rank.normalize_exception("Uncaught (in promise): Error 404 at https://x.com/a.js"),
                         "Error <n> at <url>")


class SweepTest(unittest.TestCase):
    def test_signals(self):
        sweep = {
            "supports": {"groups": [{"key": "prop:field-sizing", "sites": ["a.com"], "examples": ["x"]}]},
            "layout": {"all": {"groups": [
                {"key": "size: flex item (row)", "sites": ["www.a.com", "b.com"], "first": ["b.com"]},
                {"key": "content: extra <div>", "sites": ["a.com"], "first": []}]}},
            "scroll": [{"site": "a.com", "scale": 1.0, "pinned": 3, "uncovered_frames": 0},
                       {"site": "b.com", "scale": 1.5, "pinned": 0, "uncovered_frames": 2}],
        }
        sig, una = rank.sweep_signals(sweep, W)
        keys = {(s.key, s.site): s.weight for s in sig}
        self.assertEqual(keys[("css: prop:field-sizing", "a.com")], W["css"])
        self.assertEqual(keys[("layout: size: flex item (row)", "a.com")], W["layout"])
        self.assertEqual(keys[("layout: size: flex item (row)", "b.com")], W["layout_first"])
        self.assertIn(("scroll: pinned rasters", "a.com"), keys)
        self.assertIn(("scroll: blank rows", "b.com"), keys)
        self.assertEqual(una, [{"key": "layout: content: extra <div>", "sites": ["a.com"]}])


class RankTest(unittest.TestCase):
    AREAS = [
        {"id": "flex", "title": "Flex", "size": "M", "wpt": ["css/css-flexbox/"],
         "match": ["^layout: .*flex", "^css: .*flex"]},
        {"id": "big", "title": "Big", "size": "L", "match": ["^api: Big"]},
    ]

    def test_merge_score_and_order(self):
        S = rank.Signal
        signals = [
            S("layout: size: flex item (row)", "a.com", 1.0),
            S("css: prop:flex-wrap", "a.com", 0.5),   # same site, weaker: counts once
            S("css: prop:flex-wrap", "b.com", 0.5),
            S("api: Big", "a.com", 3.0),
            S("css: prop:field-sizing", "c.com", 0.5),  # no area: an item of its own
            S("exception: boom", "d.com", 1.0),      # no area: unattributed, not an item
        ]
        reports = [{"id": "r1", "user": True, "area": "flex"},
                   {"id": "r2", "user": False, "title": "Alone", "size": "S"}]
        wpt = {"css/css-flexbox/x.html": (1000, 2000)}
        ranked, unclaimed = rank.rank(signals, self.AREAS, reports, wpt, W)
        by = {i.id: i for i in ranked}
        flex = by["flex"]
        self.assertEqual(flex.sites, {"a.com": 1.0, "b.com": 0.5})
        self.assertEqual((flex.wpt_failing, flex.wpt_total), (1000, 2000))
        # (10 * 1.5 + 15 * 1 + 10 * 1000/2000) / 2
        self.assertEqual(flex.score, 17.5)
        self.assertEqual(by["big"].score, 7.5)            # 10 * 3 / 4
        self.assertEqual(by["css: prop:field-sizing"].score, 5.0)
        self.assertEqual(by["report:r2"].score, 3.0)
        self.assertEqual([i.id for i in ranked][0], "flex")
        self.assertEqual([s.key for s in unclaimed], ["exception: boom"])

    def test_ignored_signals_are_dropped(self):
        S = rank.Signal
        ranked, _ = rank.rank([S("api: Foo.prototype.private", "a.com", 2.0), S("api: Bar", "a.com", 2.0)],
                              [], [], None, W, ["private"])
        self.assertEqual([i.id for i in ranked], ["api: Bar"])

    def test_first_matching_area_wins(self):
        areas = [{"id": "one", "title": "1", "match": ["flex"]}, {"id": "two", "title": "2", "match": ["flex"]}]
        ranked, _ = rank.rank([rank.Signal("css: flex", "a.com", 1.0)], areas, [], None, W)
        self.assertEqual([i.id for i in ranked], ["one"])


class AreasFileTest(unittest.TestCase):
    def test_areas_file_is_valid(self):
        cfg = json.loads((HERE / "areas.json").read_text(encoding="utf-8"))
        ids = [a["id"] for a in cfg["areas"]]
        self.assertEqual(len(ids), len(set(ids)))
        for a in cfg["areas"]:
            self.assertIn(a.get("size", "M"), rank.SIZES, a["id"])
            for p in a.get("match", []):
                re.compile(p)
                self.assertNotIn("\\\\", p, f"{a['id']}: a doubled backslash matches a backslash: {p}")
            for p in a.get("wpt", []):
                self.assertTrue(p.endswith("/"), f"{a['id']}: {p}")
        self.assertTrue(set(cfg.get("weights", {})) <= set(rank.DEFAULT_WEIGHTS))
        for p in cfg.get("ignore", []):
            re.compile(p)


class EndToEndTest(unittest.TestCase):
    def test_main_writes_both_files(self):
        with tempfile.TemporaryDirectory() as d:
            d = Path(d)
            (d / "sweep.json").write_text(json.dumps({"supports": {"groups": [
                {"key": "prop:field-sizing", "sites": ["a.com", "b.com"], "examples": []}]}}), encoding="utf-8")
            (d / "wpt.jsonl").write_text(wpt_line("css/css-ui/f.html", [1, 0]) + "\n", encoding="utf-8")
            (d / "areas.json").write_text(json.dumps({"areas": [
                {"id": "ui", "title": "CSS UI", "size": "S", "wpt": ["css/css-ui/"], "match": ["field-sizing"]}]}),
                encoding="utf-8")
            (d / "reports.json").write_text(json.dumps({"reports": [{"id": "9", "user": True, "area": "ui"}]}),
                                            encoding="utf-8")
            rc = rank.main(["--sweep", str(d / "sweep.json"), "--wpt", str(d / "wpt.jsonl"), "--areas",
                            str(d / "areas.json"), "--reports", str(d / "reports.json"), "--out", str(d / "o")])
            self.assertEqual(rc, 0)
            doc = json.loads((d / "o" / "payoff.json").read_text(encoding="utf-8"))
            self.assertEqual(doc["items"][0]["id"], "ui")
            self.assertEqual(doc["items"][0]["user_reports"], ["9"])
            self.assertEqual(doc["items"][0]["wpt_failing"], 1)
            md = (d / "o" / "payoff.md").read_text(encoding="utf-8")
            self.assertIn("| 1 | CSS UI |", md)


if __name__ == "__main__":
    unittest.main()
