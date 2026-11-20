# Known issues

These are the known open problems in Snowlark, grouped by area. Some entries are partly fixed: the bullet describes
what is still wrong. Please report new problems on the issue tracker at
https://github.com/snowlark-browser/snowlark/issues. The reference number at the end of each bullet is stable; it is not an issue number on the tracker.

## Layout

- **Flexbox gaps.** A column flex item with an explicit height and `min-height: auto` does not shrink to its automatic
  minimum when the items overflow, and a single-line `flex-wrap: wrap-reverse` container puts its line at the wrong
  cross edge. `flex-wrap: balance` and `flex-line-count` are not implemented. Both layout bugs are in the vendored
  flex algorithm. (ref 42)
- **Grid gaps.** `display: grid-lanes` (masonry) and subgrid are not implemented, and subgrids in another writing mode do
  not take their parent's tracks. Column flex containers compute no baselines, so `last baseline` alignment falls back
  to `end` there. (ref 42, 85)
- **Writing modes.** Vertical modes work in block, inline, float, table, flex, grid and positioned layout, but several
  pieces are missing: ideographic baseline metrics (`text-box-edge`), `text-group-align`, `chws`/`halt` in vertical
  runs, emphasis marks, collapsed borders in right-to-left tables, `:dir()` in `matches()`, and native form-control
  looks under vertical `appearance: none`. (ref 37)
- **Positioned layout.** Anchor positioning is not implemented. Also missing: bidi reordering of absolutely positioned
  placeholders, single-axis scroll containers (`overflow-x` and `overflow-y` computed to different values),
  sticky positioning inside a split inline, and 3D transforms in client rects. (ref 48)
- **Multi-column layout.** Columns and balancing work. `column-span`, column rules, `getClientRects()` across columns,
  intrinsic column sizes, `break-*` properties, and orphans and widows are open. (ref 217)
- **Tables.** Missing: `baseline-table` handling, merged colspan tracks in auto tables, percentage handling in quirks
  mode and fixed-layout min/max cases, percentage heights of cell content against the final row height, and some
  collapsed-border painting order. (ref 68)
- **Scroll containers.** Elements use overlay scroll bars, so `scrollbar-gutter` reserves nothing. Also open:
  `text-overflow` while scrolled and in form controls, scroll markers and buttons, `scrollParent` and `getBoxQuads()`.
  (ref 50)
- **`content-visibility`.** `content-visibility: hidden` and `<details>` work, but `content-visibility: auto` is not
  implemented, skipped content is still laid out eagerly, and find-in-page and `hidden=until-found` do not reveal
  closed content. (ref 106)
- **Inline-level and block-in-inline details.** `::first-line` and `::first-letter` are not implemented, nor are `ruby`,
  `initial-letter` or `hanging-punctuation`. `alignment-baseline` and `dominant-baseline` values beyond what
  `vertical-align` offers are open. (ref 36)
- **Buttons.** `ruby-*` and `run-in` displays are not parsed. An absolutely positioned button is not sized as a replaced
  element, and `align-self: normal` still stretches a button that is a flex or grid item. (ref 216)
- **Smaller layout differences.** A flex container holding a `display: none` image counts that image's size, and a
  visually hidden absolutely positioned span in an inline-flex label reports an empty rect. (ref 31)
- **Incremental layout differs from a fresh build in rare cases.** When a text node is edited so that it holds only
  collapsible white space, the incremental update can keep a text box that a fresh build would not generate. Found with
  the layout fuzz tests, in `snowlark-layout`. (ref 266, 351)
- **Inline box background across lines.** A background image on an inline box that wraps across lines is positioned per
  line piece instead of once for the whole box (for example an external-link icon on a wrapped link). (ref 311)
- **`getBoundingClientRect()` of an inline.** The rect includes the transformed box of a descendant, where CSSOM View
  says only the inline's own fragments count. (ref 95)

## CSS and style

- **Filters.** `filter` functions and `backdrop-filter` work. Not done: `filter: url()` pointing to an external SVG
  document, `linearRGB` interpolation in SVG filters, `filter` on the root element, `will-change: filter` as a
  compositing reason, and running filter animations on the compositor. (ref 40)
- **Backgrounds and borders.** Open: `background-clip: text` and `border-area`, border images from SVGs without a natural
  size, box-shadow and background slicing across fragments, backgrounds on `::first-line`/`::first-letter`, and
  gradient interpolation color spaces for some cases. (ref 35)
- **CSS images.** Open: `object`/`embed` showing an SVG document under `object-fit`, `image-resolution`,
  `object-fit: contain scale-down`, `image()` orientation tags, and `conic-gradient` stops that mix angles and
  percentages inside `calc()`. (ref 69)
- **Fonts.** Missing: matching on `font-stretch` and `unicode-range` when selecting faces, `font-variation-settings`
  descriptors, `size-adjust`, metric override descriptors, `font-display` periods, `FontFaceSet` loading events, and
  lazy loading of faces nothing uses. Request modifiers on a `src` URL are not parsed. (ref 68)
- **Animations.** Replacing the effect of a CSS animation (`animation.effect = ...`) does nothing: the element keeps
  the CSS animation's values. Script animations on pseudo-elements do not reach the cascade. (ref 115, 33)
- **`getComputedStyle()` on pseudo-elements.** `getComputedStyle(el, '::before')` returns empty strings when the
  pseudo-element generates no box, where the specification asks for its computed values. (ref 96)
- **Highlight pseudo-elements.** The `::selection` highlight covers the font's ascent and descent rather than the line
  box. With no author style, selected text keeps its own color on a translucent blue. `text-shadow` and
  `text-decoration` on `::selection` are not applied, `getComputedStyle(el, '::selection')` returns empty strings, a
  selected replaced element gets no highlight, and the painted selection's colors are not resolved again when only
  styles change. (ref 502)
- **`clip-path`.** `margin-box` resolves percentage and `auto` margins against the box's own width. `url()` on a
  pseudo-element does not clip, a `clip-path` on a shape inside a `<clipPath>` is ignored, and `url()` to a
  `<clipPath>` in another document names nothing. (ref 63)
- **Backdrop filter over scrolling content.** Content that scrolls over a fixed layer does not blur that layer with its
  `backdrop-filter`, because the filter only sees its own raster piece. (ref 495)

## Painting and compositing

- **Rectangular SVG clip paths are not antialiased.** An SVG `clipPath` made of one axis-aligned rectangle is snapped to
  device pixels, so under a scaling `viewBox` an edge at a fractional position shows a full pixel. (ref 331)
- **System-font text is not byte-stable between runs.** The same frames can differ by a few bytes from one run to the
  next. The cause is not established. (ref 87)
- **Canvas 2D.** `ctx.filter` keeps its value but is not applied. Edges of images drawn at fractional coordinates are
  blended by coverage instead of filled. (ref 499)
- **Redundant paint.** On some animated pages the tab paints twice per frame, the second display list being identical
  to the first. The raster is skipped, but the display list work is wasted. (ref 505)
- **A page's own scroll and a wheel at the same moment.** A wheel turned while a script-driven scroll is on its way to
  the compositor can undo the scroll for a frame. Identified from the code, not observed. (ref 492)
- **Autoscroll cursor.** With middle-button autoscroll, circling the origin marker quickly makes the cursor icon draw
  wrongly. (ref 88)

## Text and fonts

- **Ideographic baselines.** OpenType `BASE` tables are not read, which affects `text-box-trim` with ideographic edges
  and vertical text metrics. (ref 37)

## DOM and HTML

- **`delegatesFocus` does nothing.** The flag is parsed and reflected, but clicks, `focus()` and sequential
  navigation ignore it. (ref 66)
- **`<link rel=expect>` does not block rendering.** `blocking=render` on `rel=expect` is not implemented. (ref 58)
- **No `unload` or `pagehide` events.** When a navigation replaces a document, `pagehide` and `unload` are not fired
  (only `beforeunload` is). (ref 108)
- **A removed stylesheet link can still fire `load`.** A `<link rel=stylesheet>` removed before its sheet arrives still
  fires `load`. (ref 122)
- **`about:srcdoc` base URL.** A srcdoc document resolves relative URLs against `about:srcdoc` instead of its
  container's base URL, so `window.open('/x.html')` inside a srcdoc frame throws. (ref 226)
- **Text controls.** A textarea does not scroll its own long text, by script or by the user. (ref 65)
- **Selection across shadow trees.** A selection cannot keep its two ends in different trees, so dragging across a
  shadow boundary and `getComposedRanges()` crossing ranges do not work as in other browsers. (ref 62)
- **`[SecureContext]` members on insecure pages.** `navigator.credentials`, `navigator.clipboard` and
  `PasswordCredential` exist on `http://` pages, although `isSecureContext` is false. (ref 82)
- **`window.open()` is a stub.** It returns null, because auxiliary browsing contexts do not exist, so `opener` is
  always null. (ref 126)

## Scripting and bindings

- **Cross-origin objects.** `[[SetPrototypeOf]]` and `[[PreventExtensions]]` on a cross-origin object throw a
  `SecurityError` instead of returning false. A receiver that is not the object itself is not detected for
  `CrossOriginGet`/`Set`. Same-origin `Location` is an ordinary object (it lacks the specified default properties),
  `javascript:` URLs do not run, `<iframe csp>` is not implemented, and the `Origin` interface is missing. On
  non-Windows-x64 targets V8's token check refuses all cross-origin access. (ref 77)
- **Cross-origin review findings.** Three findings from an independent review of the cross-origin boundary remain
  open. (ref 78)
- **Credential Management and Battery details.** Early rejections in Credential Management run in the queued task,
  `getBattery()` creates a fresh rejected promise per call in a cross-origin frame, `parse*OptionsFromJSON` rejects with
  the wrong error, and a truncated body over TLS has no test. (ref 83)
- **`use` trees are stale for script reads.** In the task that changes a `<use>` target, `elementFromPoint()` and
  `getBBox()` still see the old instance until the next rendering update. (ref 84)
- **Missing or partial APIs.** `LargestContentfulPaint`, `performance.eventCounts`, `ClipboardEvent`/`DataTransfer`, and
  Trusted Types on the attribute-node paths are not implemented. Frames share the top-level document's time origin. (ref 33)
- **Service workers.** `navigator.serviceWorker` and the Cache API are not implemented. Pages that check for service
  workers print a warning. (ref 127)
- **IndexedDB.** Names with lone surrogates are not supported, there is no storage partitioning by top-level site,
  no quota and no `close` event, a database's contents are read into memory on first use, and transactions,
  connections and cursors are not garbage collected. Deeply nested array keys (thousands of levels) can overflow a
  thread's stack, and the page does not yet refuse them with a `DataError`. (ref 49, 89)
- **Web Audio output device change.** An `AudioContext` stops with an `error` event when the default output device
  changes or is removed, instead of moving to the new device. (ref 125)

## SVG

- **SVG images.** `#svgView` and `#xywh` fragments on SVG images are not implemented. (ref 67)
- **Inline SVG painting.** Inline SVG is still drawn through a serialize-and-rasterize path. `foreignObject`,
  `vector-effect`, text beyond what the rasterizer supports (`inline-size`, `shape-inside`, `textLength` on lists),
  percentage dashes and `context-fill` corners are not supported. (ref 67)
- **SVG DOM details.** `:target` inside a resource document, SMIL animation of `d` where path arithmetic differs,
  focus inside hidden SVG containers, `getScreenCTM()` through CSS transforms of HTML ancestors,
  `attributeStyleMap` on SVG elements, and listeners added to an original that should reach its `<use>` instances are
  open. (ref 67)
- **SMIL crash in headless render.** `--render` of some SMIL animation pages panics with a stale node id roughly one
  run in three. Timing dependent. (ref 93)
- **Motion path.** `corner-shape` on the `<coord-box>` path and `rect()` radii in `offset-path` are not supported, and
  a containing block is known only for boxes laid out by a block, flex or grid parent. (ref 43)

## Media

- **DRM.** Encrypted Media Extensions offer Clear Key and, in the window on Windows 10 2004 or later with hardware
  DRM, PlayReady (`com.microsoft.playready.recommendation.3000`, or the name without `.3000` with robustness
  `"3000"`); Widevine is not offered, so services that need it cannot play. Netflix asks for Widevine only and shows
  error D7701-1003. Protected video plays through Media Source Extensions only, with temporary sessions only, and stays
  black with the GPU process (`--gpu-process`). Clear Key decrypts `cenc` and `cbcs` MP4 through Media Source
  Extensions only (not a `src` file or WebM). (ref 548)
- **xHE-AAC.** `canPlayType`, `MediaSource.isTypeSupported` and `decodingInfo` answer `mp4a.40.42` (xHE-AAC) as
  plain AAC, without asking whether the system can decode it. (ref 574)
- **`changeType()` to a different decoder.** Media Source Extensions `changeType()` cannot switch a set-up
  `SourceBuffer` to a stream that needs another decoder. Dolby AC-3 and E-AC-3 are not offered through MSE for that
  reason. (ref 150)
- **Intermittent downgrade at stream start.** On some YouTube 4K runs the first format picked is 720p and stays there.
  (ref 146)
- **YouTube attestation.** On some runs YouTube stops serving media after about a minute because its player attestation
  does not complete. The cause has not been found. (ref 72)
- **YouTube load time.** Watch pages load slower than they could, mostly in script before `DOMContentLoaded`.
  The first `isTypeSupported()` call still starts Media Foundation and one VP9 decoder, and VP9 profiles 1 and 3
  and 12-bit VP9 are reported as unsupported. (ref 98)
- **Slow main script on return visits.** From the third visit on, YouTube's main script runs slowly because cached code
  still takes a path that recollects source positions. (ref 220)

## Editing and input

- **Pointer and focus path.** Touch and pen input are not implemented. Autofocus, `inert` on non-HTML elements and in
  iframes, and `:focus-visible` after a click are partly missing, and `elementsFromPoint()` order is wrong for some
  SVG, table and margin cases. (ref 126)
- **Slow text selection on large pages.** Dragging a selection on a page with many shadow roots and custom elements
  (for example a long Reddit thread) lags behind the pointer. Not measured. (ref 92)

## Browser UI

- **Scroll bar details.** A classic scroll bar is drawn for the page, but frames keep overlay bars (none drawn) and
  `scrollbar-gutter` is not implemented. (ref 8)
- **Downloads.** Every download asks where to save it; there is no download indicator in the toolbar (the
  downloads page, Ctrl+J, shows progress). A download cannot be paused or resumed (Retry starts it again), files are
  not checked for danger before they are saved, and clearing browsing data does not clear the download list.
  (ref 542)
- **Incognito windows are not finished.** They keep nothing on disk, but open the settings, history and bookmark
  pages in themselves rather than in a regular window, and lose bookmarks added in them when they close. (ref 540)

## Tooling

- **Windows-subsystem build exits with failure when profiling.** `snowlark.exe` exits with a failure status when
  `SNOWLARK_PROFILE=1` is set and stderr is a pipe, after writing its output. `snowlark-cli.exe` works. (ref 54)
- **Debug-build test timeouts.** The IndexedDB test `finished_requests_are_collected` times out in a debug build, and the
  deep-tree test `a_deep_tree_overflows_one_renderer_only` returns early in debug builds because the debug renderer is
  too slow to crash. The Media Source test `an_append_ends_once_however_the_media_side_reports_it` fails in a debug
  build (its `update` and `updateend` events arrive after the check) and passes in optimised builds. (ref 56, 91, 560)
- **Reftest hangs.** Two reftests, `background-size-near-zero-png` and the flexbox `flex-1-N-N-shrink` pages, hang in
  the runner in ways its watchdog cannot stop. (ref 39)
