# Reftests

Most of CSS layout and painting conformance in web-platform-tests is written as **reftests**: a test page and one or
more reference pages that must render the same (or, for `mismatch`, differently). `run.py` runs them next to the
testharness.js tests, with Snowlark's `--reftest` mode. WPT's own description:
<https://web-platform-tests.org/writing-tests/reftests.html>; the reference implementation is
`RefTestImplementation` in `tools/wptrunner/wptrunner/executors/base.py` of the WPT checkout (`SNOWLARK_WPT`).

## Running

```bash
python tools/wpt/run.py --snowlark target/iter/snowlark.exe css/css-inline css/css-sizing --jobs 8 --name inline1
```

runs the areas' testharness tests, then their reftests. `--reftests-only` skips the testharness tests,
`--no-reftests` the reftests. Results go where the testharness results go (`SNOWLARK_WPT_STATE`, see `README.md`):

- `results/<name>.jsonl`: one line per reftest after the testharness lines, in the same shape (`url`, `ms`,
  `result: {status, message, tests: [{name: "reftest", status, message}]}`) plus `"kind": "reftest"` and a `reftest`
  object: `status` (PASS, FAIL, TIMEOUT, ERROR), `wait` (`removed` / `never removed` / null for `reftest-wait`),
  `error` (the test page's first uncaught exception), `pair` (the last comparison: left URL, `==` or `!=`, right URL),
  `max_difference` and `pixels` (of that comparison), `artifacts` (the folder below). The testharness counts of the
  summary ignore these lines, so they are unchanged by reftests.
- `results/<name>-summary.md`: a **Reftests** section at the end: passed / total per area, and fail, timeout and
  error counts.
- `results/<name>-reftests/<test path>/`: for every reftest that did not pass, `test.png` and `ref.png` (the two sides
  of the last comparison: for a chain, the left side can be a reference), `diff.png` (differing pixels red over a
  faded copy of the left side) and `info.txt` (the URLs, the verdict, the pixel counts and the pages' uncaught
  exceptions). Open the URLs in a browser to compare.

## How it works

1. **Discovery** (`reftests_in`, `ref_node`): the manifest's rules (`tools/manifest/sourcefile.py` of the WPT
   checkout), with regular expressions instead of html5lib (an area takes under a second instead of a minute; it finds
   the same tests as the manifest for every area validated). A reftest is a `.html`, `.htm`, `.xht`, `.xhtml`, `.xml`
   or `.svg` file with a `<link rel=match|mismatch href=...>` that is not a testharness test, outside `resources/`,
   `support/`, `tools/`, `crashtests/`, `print/`, and not named `-manual`, `-visual`, `-crash` or `-print`. Files
   under `reference/` with links of their own are tests too, as in the manifest. `.https.` and `.h2.` files are
   skipped like the testharness ones. `<meta name=variant>` variants become separate tests, their query and fragment
   carried to the references.
2. **The reference graph**: every file reachable through match/mismatch links from a test is parsed and screenshotted
   once per run (a reference shared by a hundred tests is taken once). References that are not in the sparse checkout
   make their tests ERROR (`missing: ... (not in the sparse checkout?)`); add the directory with
   `git -C <wpt> sparse-checkout add <dir>`. `about:blank` references are drawn by the runner (a white page).
3. **Screenshots** (`snowlark --reftest <list> <outdir> [<results.jsonl>] [--font <file>]`, in
   `crates/snowlark/src/cli/reftest.rs`): one process per job renders its share of the pages one after another, each
   in a fresh `Page` over wptserve (`http://web-platform.test:8000`, host rules as for the harness tests). The
   screenshot is taken as wptrunner's `test-wait.js` takes it: after `load`, `document.fonts.ready` and two animation
   frames; while the root element has class `reftest-wait`, the runner fires `TestRendered` at it and waits for the
   class to go (two more frames after each change of the root's attributes). Two departures, both for this engine's
   sake: the wait also covers the document's subresources still in flight after `load` (the `load` event does not
   wait for CSS background images, so screenshots raced them), and `fonts.ready` is only awaited while
   `document.fonts.status` is `loading` (a promise reaction queued outside a task does not run while the event loop is
   otherwise idle, which ended pages early). The viewport is 800x600 CSS px at device pixel ratio 1 (or a
   `<meta name=viewport-size content=WxH>`), drawn from the page's display list with its frames, at its scroll
   position, with overlay scroll bars (as `--wpt`; `SNOWLARK_WPT_SCROLLBAR=17` for the classic one). Text is
   antialiased in grayscale: ClearType's color fringes would make Ahem glyphs differ from the boxes references draw in
   their place. `run.py` passes `--font <wpt>/fonts/Ahem.ttf`, which registers Ahem as an installed font (WPT requires
   it installed; tests that load it through `/fonts/ahem.css` get it as a web font either way). A crash or hang costs
   one page: the driver restarts the process after it (the page becomes ERROR `CRASH: ...`), and a watchdog stops a
   script that never yields.
4. **Timeouts**: 10 s per page, 60 s with `<meta name=timeout content=long>` on the test (WPT's reftest timeouts). A
   page whose event loop has nothing left to do while `reftest-wait` is still on (or before `load`) cannot change any
   more and ends at once as TIMEOUT, with `(the page went idle)`. A test is TIMEOUT when a page it needs timed out,
   ERROR when one could not be loaded or crashed.
5. **Comparison** (`evaluate_reftest`): wptrunner's depth-first walk. The test's references are alternatives (the
   first path that holds wins); a reference with references of its own must hold for them too (a chain). Images with
   the same pixels are equal; otherwise the difference is measured as wptrunner measures it (below). `==` holds when
   the images are equal (within the fuzziness), `!=` when they are not.

## Fuzzy matching

`<meta name=fuzzy content="[ref-url:]maxDifference=A-B;totalPixels=C-D">` (the names can be left out: `A-B;C-D`, and
a single number means that exact value). With a URL before the colon it applies to the comparison with that reference
only; without, to every comparison of the file. Like wptrunner, the fuzziness of a comparison is that of its left-hand
file (the test, or the reference whose own reference is being compared).

The difference of two screenshots is computed exactly as wptrunner's `get_differences` does with Pillow: the
per-channel absolute difference image, then over the pixels whose difference has a nonzero luma (Pillow's RGB to L
conversion, `(R*19595 + G*38470 + B*7471 + 32768) >> 16`), the pixel count and the largest channel difference. (A pixel
that differs by a few levels in blue alone has zero luma, so it does not count: that is wptrunner's behaviour, kept on
purpose.) Then, with no fuzziness, the images are equal when both numbers are 0; with fuzziness they are equal when the
pixel count is 0 and C is 0, or the maximum difference is 0 and A is 0, or A <= max difference <= B and C <= pixel
count <= D.

## Known limitations

- Not supported: `.ini` expectations and fuzziness overrides (not needed: every result is reported),
  `<meta name=device-pixel-ratio>` (screenshots are at 1), print reftests (`-print`, `print/`, `reftest-print`),
  `.https.` tests, and `<link>`s are found by tag name, so a `<link rel=match>` outside the XHTML namespace in an SVG
  file counts (the manifest would ignore it; no such file in the areas checked).
- wptrunner screenshots lazily and stops at the first passing path; this runner screenshots every reachable reference
  up front (only matters for graphs with alternatives, which are rare).
- The screenshot is the viewport only, as WPT's; what is below the fold does not count.
- Scroll bars are overlay scroll bars while other browsers' tests run with classic ones: a test whose reference draws a
  scroll bar's room by hand can differ.
- Text antialiasing is grayscale, at DirectWrite's rendering mode: a test that compares text with text passes when
  both are drawn the same way, but one comparing text with an image of text made by another browser does not (that is
  also true of the other browsers).
- Run to run, a handful of results flip (about 1%: `css/css-backgrounds/background-size/vector/*` SVG backgrounds and
  a few timeouts); compare two runs before calling a single flip a regression.

## Cost and first numbers

With `--jobs 8`: `css/css-backgrounds` (132 testharness files, 729 reftests, 1,147 screenshots) takes about a minute;
nine layout areas (5,999 reftests, 9,389 screenshots) take 8 minutes with `--reftests-only`. The first run
(2026-11-01) passed 1,646 of 5,999 (27.4%). About a quarter of the `==` failures differed only in lines at most two
pixels wide (box edges at fractional positions drawn antialiased, where browsers snap them to pixels; one-pixel
offsets), so pixel snapping in painting was the largest single lever.
