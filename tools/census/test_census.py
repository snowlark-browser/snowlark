# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Unit tests for the census tool: `python -m unittest discover tools/census` (no browser needed)."""

from __future__ import annotations

import json
import struct
import sys
import tempfile
import unittest
import zlib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import cdp  # noqa: E402
import corpus  # noqa: E402
import report  # noqa: E402
import runtime  # noqa: E402
import surface  # noqa: E402


class WebSocketFrames(unittest.TestCase):
    def test_short_frame_is_masked(self):
        frame = cdp.encode_frame(0x1, b"hi", b"\x01\x02\x03\x04")
        self.assertEqual(frame[:2], bytes([0x81, 0x82]))
        self.assertEqual(frame[2:6], b"\x01\x02\x03\x04")
        self.assertEqual(frame[6:], bytes([ord("h") ^ 1, ord("i") ^ 2]))

    def test_length_encodings(self):
        self.assertEqual(cdp.encode_frame(1, b"x" * 200, b"\0" * 4)[1:4], bytes([0xFE, 0, 200]))
        self.assertEqual(cdp.encode_frame(1, b"x" * 70000, b"\0" * 4)[1:10],
                         bytes([0xFF]) + struct.pack(">Q", 70000))


def surface_of(members, values=None):
    return {"members": members, "values": values or {}}


class Diff(unittest.TestCase):
    def test_missing_interfaces_collapse_their_members(self):
        chrome = surface_of({
            "ReadableStream": "interface",
            "ReadableStream.prototype.getReader": "method",
            "ReadableStream.from": "method",
            "Navigator": "interface",
            "Navigator.prototype.getBattery": "method",
            "Navigator.prototype.webkitGetUserMedia": "method",
            "window.screen": "accessor",
        })
        snowlark = surface_of({
            "Navigator": "interface",
            "window.screen": "getter",
            "Navigator.prototype.oscpu": "getter",
        })
        d = surface.diff_members(chrome, snowlark, ["*webkit*"])
        self.assertEqual(d["missing"], ["Navigator.prototype.getBattery", "ReadableStream"])
        self.assertEqual(d["missing_members_of"], {"ReadableStream": 2})
        self.assertEqual(d["extra"], ["Navigator.prototype.oscpu"])
        self.assertEqual(d["kind"], [("window.screen", "accessor", "getter")])

    def test_stubs(self):
        chrome = surface_of({"window.screen": "getter"}, {"window.screen": "object"})
        snowlark = surface_of({"window.screen": "getter"}, {"window.screen": "undefined"})
        self.assertEqual(surface.diff_members(chrome, snowlark, [])["stubs"],
                         [("window.screen", "object", "undefined")])

    def test_ignore_patterns(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "ignore.txt"
            p.write_text("# comment\n*webkit*  # prefixed\n\nwindow.chrome\n", encoding="utf-8")
            patterns = surface.load_ignore(p)
        self.assertEqual(patterns, ["*webkit*", "window.chrome"])
        self.assertTrue(surface.ignored("HTMLVideoElement.prototype.webkitEnterFullscreen", patterns))
        self.assertFalse(surface.ignored("window.screen", patterns))

    def test_ecmascript_classification(self):
        self.assertTrue(surface.is_ecmascript("Iterator.prototype.toArray"))
        self.assertTrue(surface.is_ecmascript("window.Math"))
        self.assertFalse(surface.is_ecmascript("ReadableStream"))


class Usage(unittest.TestCase):
    def setUp(self):
        self.t = corpus.SiteTokens()
        self.t.add_script("var s=new ReadableStream({});x.getReader();if('getBattery'in navigator){}"
                          "Promise.try(f);requestIdleCallback(g);")
        self.t.add_html("<p>.notInScript(</p><script>a.inInline()</script>")

    def test_references(self):
        r = self.t.references
        self.assertTrue(r("ReadableStream", "interface"))
        self.assertFalse(r("WritableStream", "interface"))
        self.assertTrue(r("ReadableStream.prototype.getReader", "method"))
        self.assertTrue(r("Navigator.prototype.getBattery", "method"))  # quoted
        self.assertTrue(r("Promise.try", "method"))
        self.assertFalse(r("Array.try", "method"))
        self.assertTrue(r("window.requestIdleCallback", "method"))
        self.assertTrue(r("Element.prototype.inInline", "method"))
        self.assertFalse(r("Element.prototype.notInScript", "method"))
        self.assertFalse(r("Symbol.prototype.@@toPrimitive", "method"))

    def test_receivers_and_free_identifiers(self):
        t = corpus.SiteTokens()
        t.add_script("a.storage.x;var o={external:1};b.Animation;if('connection' in navigator){}"
                     "navigator.userAgentData;performance.memory")
        r = t.references
        self.assertFalse(r("Navigator.prototype.storage", "getter"))  # a.storage only
        self.assertTrue(r("Navigator.prototype.connection", "getter"))
        self.assertTrue(r("Navigator.prototype.userAgentData", "getter"))
        self.assertTrue(r("Performance.prototype.memory", "getter"))
        self.assertFalse(r("window.external", "getter"))  # an object key
        self.assertFalse(r("Animation", "interface"))  # a property
        self.assertTrue(r("navigator.userAgentData", "data"))
        self.assertEqual(corpus.match_kind("Navigator.prototype.storage"), "exact")
        self.assertEqual(corpus.match_kind("Range.prototype.expand"), "name")
        self.assertEqual(corpus.match_kind("Promise.try"), "exact")

    def test_words_in_strings_and_comments_are_not_globals(self):
        t = corpus.SiteTokens()
        t.add_script(r"""var m="call when ready", n='it\'s Profiler'; // viewport""" + "\n"
                     '/* trustedTypes */ x=typeof external; new Animation(); y=Lock; z=requestIdleCallback')
        r = t.references
        self.assertFalse(r("window.when", "method"))
        self.assertFalse(r("window.viewport", "getter"))
        self.assertFalse(r("window.trustedTypes", "getter"))
        self.assertFalse(r("Profiler", "interface"))
        self.assertTrue(r("window.external", "getter"))  # typeof
        self.assertTrue(r("Animation", "interface"))  # new
        self.assertFalse(r("Lock", "interface"))  # a one-word name used bare
        self.assertTrue(r("window.requestIdleCallback", "method"))

    def test_usage_lists_sites(self):
        other = corpus.SiteTokens()
        other.add_script("new WritableStream()")
        use = corpus.usage({"ReadableStream": "interface", "WritableStream": "interface",
                            "Foo": "interface"}, {"a": self.t, "b": other})
        self.assertEqual(use, {"ReadableStream": ["a"], "WritableStream": ["b"]})

    def test_slug(self):
        self.assertEqual(corpus.slug("https://www.facebook.com"), "facebook.com")
        self.assertEqual(corpus.slug("https://en.wikipedia.org/wiki/Main"), "en.wikipedia.org_wiki_Main")


LOG = """[console.warn] PerformanceObserver: the entry type 'resource' is not supported; ignored
[console.error] Uncaught ReferenceError: requestIdleCallback is not defined (https://a.example/app.js:1:5)
[console.error] Uncaught TypeError: navigator.getBattery is not a function (https://a.example/b.js:2:9)
[console.error] Uncaught (in promise) TypeError: Cannot read properties of undefined (reading 'width')
[console.error] boom (https://a.example/c.js:3:1)
[console.error] Uncaught TypeError: Failed to resolve module specifier (undefined:0:0)
[render] not loaded after 60012 ms (timeout)
{"title":"A","url":"https://a.example/","readyState":"interactive","elements":10,"text":0}
[render] https://a.example/ -> out.png (1280x800 px, document 800 px tall) in 60500 ms
"""


class Logs(unittest.TestCase):
    def test_parse_log(self):
        r = runtime.parse_log(LOG)
        self.assertEqual((r["loaded"], r["timeout"], r["settle_ms"], r["total_ms"]),
                         (False, True, 60012, 60500))
        self.assertEqual(len(r["exceptions"]), 4)
        self.assertEqual(r["exceptions"][3]["source"], "undefined:0:0")
        self.assertEqual(r["exceptions"][0]["source"], "https://a.example/app.js:1:5")
        self.assertEqual(r["exceptions"][0]["text"],
                         "Uncaught ReferenceError: requestIdleCallback is not defined")
        self.assertEqual(r["errors"], [{"text": "boom", "source": "https://a.example/c.js:3:1"}])
        self.assertEqual(r["reference_errors"], ["requestIdleCallback"])
        self.assertEqual(r["type_error_names"], ["getBattery", "width"])
        self.assertEqual(r["warnings"], 1)
        self.assertEqual(r["eval"]["elements"], 10)


def png_bytes(width, height, rows, filters, ctype=6):
    bpp = {2: 3, 6: 4}[ctype]

    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))

    raw = b""
    prev = bytes(width * bpp)
    for row, f in zip(rows, filters):
        if f == 0:
            enc = row
        elif f == 1:
            enc = bytes((row[i] - (row[i - bpp] if i >= bpp else 0)) & 255 for i in range(len(row)))
        elif f == 2:
            enc = bytes((a - b) & 255 for a, b in zip(row, prev))
        elif f == 3:
            enc = bytes((row[i] - (((row[i - bpp] if i >= bpp else 0) + prev[i]) >> 1)) & 255
                        for i in range(len(row)))
        else:
            out = []
            for i in range(len(row)):
                a = row[i - bpp] if i >= bpp else 0
                b = prev[i]
                c = prev[i - bpp] if i >= bpp else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pr = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                out.append((row[i] - pr) & 255)
            enc = bytes(out)
        raw += bytes([f]) + enc
        prev = row
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, ctype, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))


class Png(unittest.TestCase):
    def test_decode_every_filter(self):
        rows = [bytes((x * 7 + y * 13 + c) & 255 for x in range(5) for c in range(4)) for y in range(5)]
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "a.png"
            p.write_bytes(png_bytes(5, 5, rows, [0, 1, 2, 3, 4]))
            w, h, bpp, out = runtime.decode_png(p)
        self.assertEqual((w, h, bpp), (5, 5, 4))
        self.assertEqual(out, rows)

    def test_blank_and_inked(self):
        white = bytes([255] * 40 * 4)
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "blank.png"
            p.write_bytes(png_bytes(40, 40, [white] * 40, [2] * 40))
            self.assertTrue(runtime.analyze_png(p, step=1)["blank"])
            inked = bytes([0, 0, 0, 255] * 20) + bytes([255] * 20 * 4)
            p.write_bytes(png_bytes(40, 40, [inked] * 40, [4] * 40))
            a = runtime.analyze_png(p, step=1)
            self.assertFalse(a["blank"])
            self.assertAlmostEqual(a["ink"], 0.5)
            p.write_bytes(b"not a png")
            self.assertIn("error", runtime.analyze_png(p))


class Report(unittest.TestCase):
    def test_capture_verdict(self):
        v = report.capture_verdict
        self.assertEqual(v({}, {}), "?")
        self.assertEqual(v({"ink": 0.0001, "blank": True}, {}), "**BLANK**")
        self.assertEqual(v({"ink": 0.002, "blank": False}, {"ink": 0.7}), "**BLANK**")
        self.assertEqual(v({"ink": 0.15, "blank": False}, {"ink": 0.48}), "**sparse**")
        self.assertEqual(v({"ink": 0.45, "blank": False}, {"ink": 0.48}), "no")
        self.assertEqual(v({"ink": 0.002, "blank": False}, {"ink": 0.003}), "no")

    def test_runtime_reads_attribute_missing_interfaces(self):
        chrome = {"members": {"PerformanceTiming": "interface",
                              "PerformanceTiming.prototype.navigationStart": "getter",
                              "PerformanceNavigationTiming": "interface",
                              "PerformanceResourceTiming": "interface",
                              "PerformanceResourceTiming.prototype.responseStart": "getter",
                              "PerformanceEntry": "interface",
                              "PerformanceEntry.prototype.name": "getter"},
                  "parents": {"PerformanceNavigationTiming": "PerformanceResourceTiming",
                              "PerformanceResourceTiming": "PerformanceEntry"}, "values": {}}
        snowlark = {"members": {"PerformanceEntry": "interface",
                              "PerformanceEntry.prototype.name": "getter"}, "values": {}}
        surfaces = {"chrome": {"window": chrome, "worker": None},
                    "snowlark": {"window": snowlark, "worker": None}}
        diff = surface.diff_surfaces(surfaces["chrome"], surfaces["snowlark"], [])
        yt = ("[console.error] Uncaught TypeError: Cannot read properties of undefined "
              "(reading 'responseStart')")
        name = ("[console.error] Uncaught TypeError: Cannot read properties of undefined "
                "(reading 'name')")
        rt = {"youtube.com": runtime.parse_log(yt), "x.com": runtime.parse_log(name),
              "twitch.tv": runtime.parse_log(
            "[console.error] Uncaught TypeError: Cannot read properties of undefined "
            "(reading 'navigationStart') (https://t.example/a.js:1:2)")}
        rows = {r["key"]: r for r in report.missing_rows(surfaces, diff, {}, set(), rt)}
        self.assertEqual(rows["PerformanceTiming"]["runtime"], ["twitch.tv"])
        # Inherited from a missing ancestor, not from PerformanceEntry, which Snowlark has.
        self.assertEqual(rows["PerformanceNavigationTiming"]["runtime"], ["youtube.com"])

    def test_render(self):
        chrome = {"members": {"ReadableStream": "interface", "ReadableStream.prototype.getReader":
                              "method", "Navigator.prototype.getBattery": "method",
                              "Iterator.prototype.toArray": "method", "window.screen": "getter",
                              "Element.prototype.remove": "method"}, "values": {}}
        snowlark = {"members": {"window.screen": "getter", "Node.prototype.remove": "method",
                              "Iterator": "interface", "Window.prototype.foo": "method"}, "values": {}}
        surfaces = {"chrome": {"window": chrome, "worker": None},
                    "snowlark": {"window": snowlark, "worker": None}}
        diff = surface.diff_surfaces(surfaces["chrome"], surfaces["snowlark"], [])
        use = {"window": {"Navigator.prototype.getBattery": ["a.com", "b.com"],
                          "ReadableStream": ["a.com"], "window.screen": ["a.com", "b.com"]}}
        rt = {"a.com": runtime.parse_log(LOG)}
        rt["a.com"]["image"] = {"ink": 0.0, "blank": True}
        text = report.render({"date": "2026-11-01"}, surfaces, diff, use, rt,
                             {"a.com": {}, "b.com": {}})
        self.assertIn("| 1 | `Navigator.prototype.getBattery` | method | 2/2 | a.com |", text)
        self.assertIn("`Element.prototype.remove`", text.split("### 1b")[1].split("### 1c")[0])
        self.assertIn("`Iterator.prototype.toArray`", text.split("### 1c")[1].split("### 1d")[0])
        self.assertIn("| `window.screen` | 2/2 | 1 of", text)
        self.assertIn("**BLANK**", text)
        self.assertIn("**no settle** (60012 ms, no load)", text)
        self.assertIn("`Window.prototype.foo`", text.split("## 3.")[1])
        json.dumps(diff)


if __name__ == "__main__":
    unittest.main()
