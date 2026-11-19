# layout-diff: where Snowlark's layout departs from Chrome's

Turns "this card is too tall" or "the checkbox is raised" into the element and the CSS properties
where Snowlark's layout first goes wrong. The tool loads one page in both engines, snapshots every element,
and walks the differences back to their root causes.

## Running it

Requirements: Python 3.13 (standard library only), Chrome at
`C:\Program Files\Google\Chrome\Application\chrome.exe` (`--chrome` to change), and a release
`snowlark-cli.exe` (`.\dev.ps1 build --release --package snowlark --bin snowlark-cli`). The tool finds the
binary in `target/release` of this checkout (or under `SNOWLARK_DEV_DIR`, where `dev.ps1` puts build output).
Pass `--snowlark` or set `SNOWLARK_CLI` to use another one.

```
python tools/layout-diff/run.py https://flingup.com --width 1593 --out out/flingup
python tools/layout-diff/run.py --corpus tools/layout-diff/corpus.txt --out out/corpus
python tools/layout-diff/tests/test_layout_diff.py        # tests (offline: local pages, in-process server)
```

| option | default | |
|---|---|---|
| `--width` | 1280 | CSS width of both viewports; the height is 800, as in `--render` |
| `--settle` | 2000 | ms to wait after the load event (Snowlark: `SNOWLARK_SETTLE_MS`) |
| `--timeout` | 30000 | ms to wait for the load event (Snowlark: `SNOWLARK_RENDER_TIMEOUT_MS`) |
| `--tol` | 1 | px; smaller differences are ignored |
| `--first` | 10 | root causes and raw differences listed per page |
| `--max-nodes` | 20000 | elements snapshotted per page |
| `--walk-timeout` | 300 | s allowed for Snowlark's snapshot script; on large pages it takes minutes (below) |
| `--serial` | off | run the engines one after the other, not at the same time |
| `--recompare` | off | no capture: compare the snapshots an earlier run left in `--out` again (after changing `--tol`, `--first` or the tool) |

Both engines get the same conditions:
- Width, viewport height and device scale 1.
- Snowlark's user agent string, read from `crates/snowlark-net/src/metadata.rs`. Chrome gets it through
  `--user-agent` and `Emulation.setUserAgentOverride`, with `Accept-Language: en-US`.
- `prefers-color-scheme: light`. Snowlark's `--render` reports light; Chrome would otherwise follow the
  Windows theme.
- No content blocking: `SNOWLARK_BLOCK` is removed from Snowlark's environment.
- A fresh temporary Chrome profile per page.

Each page goes through the same steps:
1. Wait for the load event, then the settle delay.
2. Evaluate `walker.js` in the page: Runtime.evaluate in Chrome, `SNOWLARK_EVAL` in Snowlark.
3. Take a screenshot: `Page.captureScreenshot` in Chrome, the `--render` PNG in Snowlark. Both are as tall
   as the document, at most 4000 px.

Chrome is closed with `Browser.close`. If it is still running 10 s later, the tool ends that Chrome's
process tree by its PID. Its profile is then deleted.

## What comes out

For one page (`--out DIR`):
- `report.html`: the report described below.
- `side-by-side.png`: the area around the first difference. Chrome is on the left and Snowlark on the
  right. Each root cause is outlined: 1 in red, then 2 to 5 in other colours.
- `chrome.json`, `snowlark.json`: the snapshots.
- `chrome.png`, `snowlark.png`: the full screenshots.
- `snowlark.log`: Snowlark's stderr.
- `result.json`: everything in the report, as data.

A corpus run writes one such directory per URL. It also writes these files:
- `summary.html`, `summary.tsv`, `summary.json`: one row per site with the URL, status, first
  difference and kind, the element, its delta, the CSS properties involved, run times and notes.
- A ranking over the first root causes of all sites, three ways: by kind, by kind and element
  (`size of img (block)`), and by differing computed property.

### The snapshot

`walker.js` walks the flat tree in document order. It enters open shadow roots, follows slots to their
assigned nodes, and skips `head`, `script` and `style`. It records the following for each element:
- A path: the `tag[nth-of-type]` chain from `<html>`, with `#shadow` marking a step into a shadow root.
- The id and class.
- `getBoundingClientRect()`.
- About 60 computed values: display, position, box model, sizes and min/max, font, line-height,
  vertical-align, aspect-ratio, object-fit, overflow, flex and grid basics, border-radius, transform.
- For elements with their own text: a hash of that text, plus the height and line count of its client
  rects.

The insides of `svg`, `iframe`, `video`, `canvas`, `select` and similar elements are not walked.

### Reading the report

The trees are aligned per parent on `tag#id.class` signatures, so one missing element does not shift
its siblings. Each matched element then gets a size delta (dw, dh) and a position delta relative to its
parent (rdx, rdy), so the contents of a moved box do not count as moved.

Differences cascade, so the tool tries to explain each one by another before calling it a cause:

| difference | explained by |
|---|---|
| width | The parent's width, for blocks (also in proportion, for percentages). The content, for shrink-to-fit boxes: inline-level boxes, floats, abspos boxes and the blocks inside them. |
| height | The in-flow content's extent, the own text, a missing or extra child, or a stretching parent (flex or grid stretch, table cells). |
| position | The previous in-flow sibling's growth or move, a missing or extra sibling, the parent's size (centring, end alignment) or the element's own size. |

An unexplained difference is a **root cause**. The first one in document order is **the first
difference**; inline-box roots are listed but never chosen as the first difference. Root causes have
these kinds:

| kind | meaning |
|---|---|
| `size` | The box's width or height differs and nothing around it accounts for that. |
| `position` | The box moved within its parent for no visible reason. |
| `baseline` | An atomic inline (form control, image, inline-block) on a baseline moved within its line. Or: its line box changed height around it while the control's size and its offset from the line top did not, so the text's baseline moved against it. |
| `text-wrap` | The own text needs a different number of lines at the same width: line breaking or glyph widths. |
| `text-height` | The own text is taller or shorter by less than a line: line-height `normal` or font metrics. |
| `text-width` | A content-sized box with own text is wider or narrower: glyph advances. |
| `accumulated` | Siblings each differ by less than `--tol`, and their sum crosses it here. The item names the element where the small difference starts (`source`), with its font properties. |
| `inline-box` | The rect of an inline box (or `<br>`) differs while its contents don't. This is a `getBoundingClientRect` or font content-area difference, usually invisible. |
| `missing`, `extra` | A visible element exists only in Chrome, or only in Snowlark. |

The same difference repeated on similar elements (`×30 similar elements`) is listed once.

Each item shows these sections:
- **Box**: both boxes, the delta (x, y, w, h) and the relative position delta.
- **Explanations tried**: what was ruled out for each aspect.
- **Properties involved**: computed values that differ. Resolved geometry (width, height, top and so
  on) is left out, because it only mirrors the boxes.
- **Deciding properties**: the properties that decide this kind of difference, where they have
  non-initial values, including the parent's display, flex and grid values.
- All computed values, with the differing ones highlighted.

A differing computed value is usually the answer. For example, on flingup `max-height: 540px -> none`
on the poster image comes from `--card-max: min(540px, 100cqw)`. Snowlark has no container query units,
so the variable is invalid at computed-value time. When nothing differs, the kind and the deciding
properties name the layout feature to check.

The report also has a second list, **raw differences**. It is the plain rule: document order, ancestor
first. An element under a differing ancestor is listed only when its own size differs by more than the
ancestor's. Use it when the explanations seem wrong.

## Limitations

- **Scripts can do different things per engine.** Feature detection, A/B tests, ads, rotating
  carousels, timestamps and lazy loading all depend on the engine and on timing. Chrome still sends
  `Sec-CH-UA` client hints that name Chrome, so servers can tell the engines apart despite the shared
  user agent. Notes in the report flag different final URLs, titles and media features. Compare the
  screenshots before trusting a `missing`/`extra` or a large first difference.
- **Timing is not identical.** Chrome waits for the load event plus the full settle delay. Snowlark's
  `--render` stops early once nothing is pending (no task, timer, frame or fetch) and waits at most
  the settle delay.
- **Fonts.** Both engines use DirectWrite and the system fonts, but fallback and metrics can differ.
  Text-width, text-height and accumulated sub-pixel differences, such as line-height `normal` (on
  news.ycombinator.com a 13.33px Verdana line is 16 px tall in Chrome and 16.2 px in Snowlark), are real but often not what you are hunting. `--tol` controls how much
  of this shows.
- **Consent pages and bot checks.** A cookie wall or captcha in one engine and not the other makes the
  diff meaningless. The notes flag likely ones from the page text. Content blocking is off in both, so
  ad-heavy pages differ in content as well as layout.
- **Gaps in Snowlark's APIs show up in the snapshot itself.** Properties that Snowlark never reports are
  listed and not compared. (`Range.getClientRects()`, the resolved `grid-template-*`, `transform` and
  `min-width` values work since 2026-11-01.)
- **Snowlark's walk time.** Until 2026-11-01 every `getBoundingClientRect` and
  `getPropertyValue` cost time linear in the document size, so the walk was quadratic (31-44 s on
  Wikipedia); it now takes 0.27 s there against Chrome's 0.2 s. `snowlark_walk_ms` in the report keeps
  track of it.
- **The explanations are heuristics.** They can blame the wrong side of an ambiguity, for example a
  child with `height: 100%` against a parent with a wrong `aspect-ratio`. They also don't model
  everything: wrapped flex lines, multi-column layout, `order` and grid placement.
- **Not covered:** closed shadow roots (their light children are walked instead), iframe contents, SVG
  internals, pseudo-elements, scrolled containers, and anything below 4000 px in the pictures (the
  snapshot itself is not limited).
