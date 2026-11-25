# visdiff: content Chrome shows and Snowlark does not, once the page settles

Finds pages that look broken: content that Chrome paints and Snowlark leaves invisible, empty, unloaded or out.
Examples are a hero stuck at `opacity: 0` because an animation froze, a section that a script reveals in one
engine only, or an image that never decodes. It loads each page in both engines, lets the page settle,
snapshots every element, and reports what is visible in Chrome but not in Snowlark, ranked by area and
grouped by cause.

It is a separate tool from `tools/layout-diff` because it asks a different question. layout-diff explains
*where boxes differ* at load time, walking size and position differences back to a root cause. visdiff asks
*what is not seen at all* after the page has finished animating and revealing itself. Those are the pages a
user calls blank or broken, and layout-diff cannot see them. At load, Chrome's content is often still at
opacity 0 too. visdiff reuses layout-diff's headless Chrome driver (`chrome.py`), standard library only.

## Running it

Requirements: Python 3.11 or later (standard library only), Chrome (`--chrome`, by default the usual
install path), and a `snowlark-cli.exe`. It is found as `SNOWLARK_CLI`, or as this checkout's `iter` build,
then its `release` build (also under `SNOWLARK_DEV_DIR`).

```
python tools/visdiff/run.py https://example.com --out out/example
python tools/visdiff/run.py --corpus tools/visdiff/corpus.txt --out out/corpus
python tools/visdiff/run.py --recompare --out out/corpus      # re-read the saved snapshots (after changing the tool)
python tools/visdiff/run.py --recompare --retry-errors --out out/corpus   # and capture the failed pages again
python tools/visdiff/test_visdiff.py                         # unit tests: matching, classification, ranking, limits
```

| option | default | |
|---|---|---|
| `--widths` | `1280,390` | CSS widths; each page runs once per width (the viewport is 800 px tall, as in `--render`) |
| `--settle` | 10000 | ms after the load event to wait, at most, for the page to settle |
| `--timeout` | 30000 | ms to wait for the load event |
| `--jobs` | 6 | pages captured at the same time (each a Chrome and a Snowlark process) |
| `--max-nodes` | 20000 | elements snapshotted per page |
| `--max-memory-gb` | 6 | memory cap per Snowlark render; 0 for none |
| `--snowlark`, `--chrome` | | the executables |

A full corpus run (348 URLs, two widths, 696 pages) takes about 75 minutes with 8 jobs, about 6 s a page.

**Limits.** Each Snowlark render runs in a Windows job object. The job caps the render's memory: past the
cap its allocations fail, so a page whose memory runs away stops there instead of exhausting the machine. The
job also ends the render when the run ends, even when the run itself is stopped. A render is also stopped
after `--timeout` plus `--settle` plus 180 s. Both are findings, not tool failures: such a page is listed
under Snowlark's own failures in the summary, with its peak memory. Chrome's browser process is in a job
that ends it with the run, with no memory cap.

## What happens per page

Both engines run the same steps, at the same width, with Snowlark's user agent string and a light colour
scheme. Chrome's scrollbars are hidden, because Snowlark's `--render` has none.
1. Load the page and wait for the load event. Each page gets a fresh Chrome profile, and Snowlark starts with
   no cookies.
2. 3 s after the load event, run one script: click the first button or link in the page whose whole text
   is a refusal ("Reject all", "Decline", "Only necessary", and so on), so a consent prompt does not cover
   the page. Then scroll one screen down, then another, then back to the top, a second apart, each scroll instant (a page's smooth scrolling would make the
   positions depend on how busy the engine is). Content that
   is revealed on scroll or loaded lazily appears. Snowlark runs this script as `SNOWLARK_EVAL_LOADED`.
3. Wait until the page settles, at most `--settle` ms after the load event:
   - Chrome waits until no animation or transition that will end is still running.
   - Snowlark's `--render` waits until nothing is pending (no task, timer, frame, fetch or running
     animation).
4. Snapshot the page with `walker.js` and take a screenshot of the first three screens (2400 px).
5. Run the pixel check (`pixels.js`) on the two screenshots.

## What counts

The walker records every element of the flat tree (open shadow roots and slots included). For each one it
records:
- its box in document coordinates, and the clip it is painted in (the overflow ancestors' boxes);
- its own opacity, and the product with its ancestors' opacities;
- `display`, `visibility`, `animation-name` and transitions;
- whether its style attribute sets display, visibility or opacity;
- its content: own text, an image (loaded or not), a canvas, a video, an SVG, a form control, a
  background image, or a background colour.

A **content element shown in Chrome** meets all of these:
- it has content (a background colour alone counts only above 20000 px²);
- its effective opacity is above 0.05 and its `visibility` is `visible`;
- at least 64 px² of its box lies inside its clip and inside the first three screens.

Frames are left out: ads, consent dialogs and embeds differ between runs. The pixel check masks them too.
So are consent and cookie banners in the page (an element, or an ancestor, whose id or class names one):
whether a page shows one depends on where it thinks the visitor is and on whether the decline click found
its button. An element is also left out when Snowlark shows another element with the same tag and classes
in the same box: a rotating carousel or slideshow that is on another slide.

Each such element is matched to Snowlark's snapshot by:
- its path (`tag[nth-of-type]` from `<html>`) when the tag, own text and image source agree;
- else its id;
- else its own text;
- else its image source. With several candidates, the one with the longest common path wins.

When Snowlark does not show the element, the tool finds where the difference starts and files it under a
cause:

| group | meaning |
|---|---|
| `animation` | The element, or the ancestor where opacity 0 or `visibility: hidden` starts, has a CSS animation, or Chrome applies an animation to it that Snowlark does not. |
| `transition` | A transition on that property, which did not run to its end. |
| `class` | Opacity or visibility from a rule, and the element's classes differ: a script added or removed one in one engine only (a reveal library, a "loaded" class). |
| `inline` | An inline style set by script. |
| `style` | The stylesheet, with nothing else differing. |
| `display-none` | Under `display: none` in Snowlark. The classes or an inline style are named when they differ. |
| `zero-size` | Its box is empty in Snowlark. |
| `clipped` | An overflow ancestor clips all of it away (the ancestor is named, with its size). |
| `off-page` | Its box lies left of or above the page, or right of its scrollable width. |
| `image` | An image loaded in Chrome and not in Snowlark (with its format). |
| `missing` | No counterpart: a script did not create it, or created something else. Filed under the nearest ancestor that both have. |

Elements are grouped by the element where the difference starts: one finding per cause element. Its area is
the hidden content's area on a grid of 16 px cells, so nested content counts once. Its **share** is that
area over the region (the width times three screens).

A page's **score** is the sum of its findings' shares. Missing elements count half, because scripts, A/B
tests and ads swing those.

The **pixel check** covers what the walker cannot see: canvas and WebGL content, images drawn wrong,
paint. It splits both screenshots into 8 px blocks. A block is content when a pixel in it differs from
that image's most common colour. It reports the share of blocks that are content in Chrome and background
in Snowlark, with frames masked. It is printed beside the score, and pages where it is large while the
score is near zero are listed separately, for a look at the screenshots.

## What comes out

Per page (`<out>/<site>/<width>/`):
- `report.html`: the findings, both screenshots with the five largest findings outlined, and the pixel
  map;
- `result.json`: the same as data;
- `chrome.json`, `snowlark.json`: the snapshots;
- `chrome.png`, `snowlark.png`: the screenshots;
- `snowlark.log`: Snowlark's stderr.

Per run:
- `summary.md`: causes ranked by their share summed over pages, with each cause's largest findings;
  Snowlark's own failures (memory cap reached, time limit passed) and the highest peak memory; the
  pixel-only pages; every page by score, with its top finding and its notes;
- `summary.json`: the same as data.

A page is **doubtful**, and left out of the cause ranking, when:
- the capture failed;
- the final URLs differ;
- the page text suggests a bot check or a consent wall.

Its row says so.

## Limitations

- **Scripts do different things per engine.** Feature detection, A/B tests, ads, rotating carousels and
  geolocated content all change what the page shows. Data attributes whose values are ids or timestamps
  are compared by name only, because their values differ on every load. `missing` findings especially need a look at the
  screenshots before they are trusted.
- **Consent prompts in frames** (most consent platforms) are not declined. They are masked in the pixel
  check and ignored by the element check, but the page behind them can still differ.
- **Timing.** Infinite animations never settle, so such pages wait the whole `--settle`. A page that reveals
  content later than 3 s after load plus `--settle` is captured before the reveal.
- **Not covered:** closed shadow roots, frame contents, SVG internals, pseudo-elements (`::before`, `::after`),
  and anything below the first three screens.
