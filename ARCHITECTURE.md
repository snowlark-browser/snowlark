# Snowlark architecture

Snowlark is a web browser and its engine for Windows, written in Rust, with V8 for JavaScript. The DOM, the
bindings, layout, painting, networking and the browser window are its own; style, HTML parsing and flex and grid
layout build on crates from Servo and the Rust ecosystem. An earlier engine by the same authors, written in C#,
showed what the web needs; nothing of it is ported.

Direction: a free, open-source browser. Priorities, in order: **user security**, **DOM-heavy speed**
(Speedometer-class work), **style and layout throughput**. Spec coverage serves those three.

## Why Rust, and what we take from other engines

Every major engine keeps the DOM native and makes the JS boundary nearly free with bindings generated from Web
IDL: Blink (C++ DOM on Oilpan, V8 bindings, Fast API calls), WebKit (C++ DOM, JSC bindings, DOMJIT), Gecko
(C++ DOM, SpiderMonkey bindings, Stylo for style). A managed-language engine pays at that boundary: in the C#
engine, about 150 ns plus string copies per DOM call, and two garbage collectors. Rust has no such floor, gives
C++-class speed without C++'s memory-safety bug class, which matters because security is priority one, and lets
us use Firefox's real components as crates:

| Need | Crate | Origin |
|---|---|---|
| JavaScript | `v8` (rusty_v8) | Deno |
| HTML parsing | `html5ever` | Servo / Firefox |
| CSS parsing, selectors | `cssparser`, `selectors` | Servo / Firefox |
| Style system (cascade, computed values, invalidation, parallel restyle) | `stylo` | Firefox's Stylo |
| URL | `url` | Servo (WHATWG URL) |
| Encodings | `encoding_rs` | Firefox |
| Flex and grid layout | `taffy` | Dioxus / Bevy |
| SVG images (`<img src=x.svg>`, CSS `url()`) | `resvg` (`usvg` to parse, `tiny-skia` to rasterize) | linebender |
| Windows graphics, text, media | `windows` (Direct2D, DirectWrite, DXGI, DirectComposition, Media Foundation) | Microsoft |
| Web IDL parsing | `weedle2` | Mozilla uniffi |

`blitz` (DioxusLabs) is a small Rust renderer built on stylo + taffy + parley. Its `blitz-dom` crate is the
best worked example of implementing stylo's `TElement`/`TNode` traits over an arena DOM: read it when doing that
work. We do not depend on it: the DOM, bindings, layout and painting are ours.

## Crates

```
crates/
  snowlark-js        V8 embedding: platform, isolates, contexts, microtasks, module loading. The only crate that
                   uses the `v8` API directly besides the generated bindings.
  snowlark-dom       The DOM: arena-allocated nodes per document, html5ever TreeSink, tree mutation with
                   mutation records, attributes, text, shadow roots, custom element registry, events/targets.
                   No V8 types. No style or layout types.
  snowlark-idl       Web IDL → Rust bindings generator (build-time tool). Input: the spec IDL files (webref).
                   Output: `snowlark-bindings` source.
  snowlark-bindings  Generated: one V8 FunctionTemplate per interface with the spec prototype chain, attribute
                   accessors and operations with WebIDL argument conversion, Fast API callbacks for simple
                   getters. Calls into snowlark-dom/-page implementations through plain Rust functions.
  snowlark-style     stylo integration: `TElement`/`TNode` over snowlark-dom, stylesheets, the Stylist, restyle
                   traversal, computed values, CSSOM views over the parsed sheets.
  snowlark-layout    Box tree from the styled DOM; block, inline, table and positioned layout ours; flex and
                   grid through taffy's tree traits; fragment tree output; text shaping through DirectWrite
                   (ClearType, system fonts).
  snowlark-graphics  What no platform API is behind: geometry, the display list, filters, layer animations,
                   a canvas's operations as data, the text types every text engine fills in and the cache
                   of shaped runs, images decoded in Rust (PNG, JPEG, GIF, WebP, AVIF) with their colour
                   profiles applied (qcms), with an error type of its own. snowlark-paint re-exports it.
  snowlark-paint     The platform's half of drawing: display list → Direct2D on a per-tab raster thread,
                   tiles, compositor with DirectComposition; the DirectWrite TextEngine (shaping lives here,
                   not in layout, so layout → paint is the dependency and painting a fragment tree is
                   layout's job); WIC for BMP, ICO, TIFF and the other formats Windows has a codec for.
                   Every web font goes through OTS (`web_font`) before DirectWrite reads it, in the renderer
                   and again on the raster side. A module with a Linux half keeps it in `src/linux/`.
  snowlark-ots-sys   OTS, the OpenType Sanitizer, and the WOFF 2.0 decoder it uses, compiled from vendor/ots and
                   vendor/woff2 with a small C interface; declarations only, the calls are snowlark-paint's.
  snowlark-net       Fetch: HTTP/1.1 and 2 with TLS, cookies, cache, HSTS, mixed-content and CSP checks,
                   blob:/data:, the request policy.
  snowlark-gl        WebGL's service (docs/webgl.md): the plain-data protocol a page's contexts send, and its
                   executor on ANGLE (OpenGL ES over Direct3D 11), loaded at run time.
  snowlark-ipc       The process boundary (docs/process-isolation.md section 7): pipes on one IO thread,
                   sections, handle passing, framing, send and call, Untrusted<T>, process launch, a loopback.
  snowlark-sandbox   The page processes' sandbox (docs/process-isolation.md section 7): on Linux the user and
                   network namespaces a renderer starts in and the seccomp-bpf filter it locks itself into.
  snowlark-idb       IndexedDB's storage, free of V8: keys and key paths, databases with object stores and
                   indexes, the backend a browser's event loops share (connection queues, versionchange,
                   transaction scheduling) and the per-database append-only log files on a disk thread.
                   snowlark-page has the IDB* interfaces over it.
  snowlark-sw        Service Workers' browser half, free of V8: the Cache API's storage per storage key
                   (matching, Vary, all-or-nothing batches, an index and body files per key on disk),
                   behind a message-shaped trait. snowlark-page has Cache and CacheStorage over it.
  snowlark-audio     Web Audio's rendering, free of V8 and unsafe code: the render quantum, channel up- and
                   down-mixing, the AudioParam automation timeline, the graph of nodes by id driven by
                   control messages, and the OfflineAudioContext's rendering thread. snowlark-page has the
                   Web Audio interfaces over it.
  snowlark-render    One document from bytes to a display list: stylo's computed values as layout's Style
                   trait, the StyleSource (pseudo-elements, anonymous boxes, counters, replaced content),
                   subresource loading (sheets, @import, images with srcset, @font-face, sanitized by OTS)
                   through snowlark-net, SVG images rasterized per drawn size, restyle/layout/paint/PNG.
                   Driven by snowlark-page and `--render`.
  snowlark-page      A document's lifetime: navigation, parsing with scripts, the event loop (tasks,
                   microtasks, rAF at display rate, timers), frames and realms, same- and cross-origin rules,
                   session history.
  snowlark-media     Playing `<video>` and `<audio>`: a media resource fetched through snowlark-net with range
                   requests, one player per media element on Windows Media Foundation, Media Source
                   Extensions, video frames handed to the compositor.
  snowlark-tab       The renderer half of a tab: one thread owns the tab's document (snowlark-page) and a tile
                   rasterizer, takes `TabCommand`s, answers with `TabEvent`s and hands frames to the
                   compositor. It knows nothing of the browser's window.
  snowlark-blocker   Content blocking with uBlock Origin's filter syntax on Brave's `adblock` engine: network
                   and cosmetic filters, lists downloaded into the profile (THIRD-PARTY.md).
  snowlark-shell     The window: tabs, address bar, bookmarks, history, site info, painted by us over a bare
                   HWND. Each tab runs its page on its own thread (`tab.rs`); the window
                   holds the other end (`tab_host.rs`) with the session history.
  snowlark           The binary: the shell, plus headless modes `--render <url> <png>`, `--eval`, `--wpt`.
```

Dependency direction is top-down only (media, tab and blocker sit between shell and page): shell → page → {bindings, render, style, layout, paint, net, idb, audio} →
dom → (nothing), with render → {style, layout, paint, net}; idb and audio depend on nothing of ours.
`snowlark-js` is used by bindings and page. If a lower crate needs something from above, it gets a trait or a
callback, never a dependency.

## Key decisions

**DOM representation.** One `Dom` arena per event loop (per agent, in HTML's terms) holds every document that
loop can reach, with a node's document as a field of the node; a `NodeId` is an index plus a generation so a
stale id can never alias a reused slot. Per-document arenas were the first plan, but template contents (an
inert separate document), iframes, `adoptNode` and fragment parsing would all have needed cross-arena copies
that change a node's id, and MutationObserver scope is per agent anyway. `html5ever` is vendored under
`vendor/` with a short patch list (`vendor/html5ever/SNOWLARK-PATCHES.md`) for spec behaviour it lacks. Parent/first/last/prev/next links are ids. Elements keep attributes in a
small vector (two inline: most elements have at most two). Text is stored inline. This is what stylo and taffy's
traits want.

**Wrappers and GC.** A node's JS wrapper is a V8 object with the `NodeId` in an internal field. Wrappers are
created lazily on first access from JS. Lifetime follows WebKit's "opaque roots" idea: a wrapper is kept
strong while its node is connected to a document that is alive; when a node is detached, its subtree's wrappers
become weak and the subtree's root wrapper keeps them reachable, so expando properties on a detached node survive
as long as anything in that subtree is referenced from JS. Nodes with no wrapper and no tree parent are freed.

**Bindings are generated, never handwritten.** The generator reads the W3C/WHATWG IDL (the `@webref/idl`
package, vendored under `idl/`) so interfaces, attribute types, `[Reflect]`, enumerated attributes, overloads
and the prototype chain match the specs mechanically. Hot read-only attributes (`parentNode`, `firstChild`,
`nodeType`, `tagName`, `id`, `className`, `value`...) use V8's Fast API so the JIT calls the Rust getter
directly. The generator emits a Rust `impl` requirement per interface; a missing implementation is a compile
error, so the set of what Snowlark supports is visible in the type system.

**Style is stylo.** We don't write a cascade. We implement its DOM traits, feed it stylesheets, and read typed
computed values. Restyle is parallel on stylo's thread pool. Invalidation sets, `:has()`, container queries,
and animations' computed-value plumbing come with it.

**Layout owns its tree.** The box tree is rebuilt incrementally from the styled DOM (dirty subtrees only) and
produces an immutable fragment tree per layout, as LayoutNG does, so unchanged subtrees reuse their fragments.
Flex and grid are taffy; block, inline, table, positioning and text are ours.

**Inline SVG paints as display list operations, as Blink's does.** Blink's `SVGRootPainter`,
`SVGContainerPainter` and `SVGShapePainter` only record paint operations (paths with their paint flags,
text as glyph runs) on the main thread; cc's raster workers draw them. Snowlark's outermost `svg` box is
still a replaced box with an image (`inline_svg` serializes the subtree, which gives the natural size, the
change detection and the fallback), but at paint `svg_paint::record` walks the styled subtree and records
it in the content box's coordinates: `FillPath` and `StrokePath` items (path side vectors in the list),
glyph runs, transforms, opacity groups and rect clips. The recording is cached by the image (a new image
per change) and the size, and `DisplayList::replace_images` draws it in place of the image item, so the
page thread neither parses the markup with usvg nor rasterizes it with resvg; the raster thread draws the
paths with Direct2D. What `record` does not handle yet (paint servers, markers, masks, filters, `use`,
`image`, nested `svg`, text on a path or per-glyph positions...) returns a reason, and that `svg` is drawn
as its image as before.

**Writing modes are frames, not code paths.** Block, inline, table, flex, grid and positioned layout are
written once, for x along the inline axis and y along the block axis, as `horizontal-tb` is physically. A
box's style is read through a view (`logical::Flip`, one of the rectangle's eight symmetries): in a vertical
frame `margin(Top)` is the block-start margin and `width()` the inline size, and in `horizontal-tb` the view
is the identity. Where a box's writing mode differs from its parent's (it then establishes an independent
formatting context) `Cx::layout_across` lays it out in its own frame and maps its fragments into the
parent's (`flow.rs`); the root's frame maps to physical coordinates at the initial containing block, so
the fragment tree, painting, hit testing and the CSSOM stay physical. Taffy needs no vertical mode of its
own, since it sees the frame. Text keeps a `Flip` of its own: vertical runs are horizontal runs drawn under a
quarter turn, and `vertical-lr` lines are laid out in a line frame (line-over up) and mapped back.

**Threads.** Shell thread (input, composition), one page thread per tab (parsing, scripts, DOM, style driving,
layout), stylo's parallel pool for restyle, one raster thread per tab, a compositor thread. The shell never
blocks on a page thread: a tab's two ends (`TabHost`, `TabRenderer`) and the compositor talk in plain-data messages
(`TabCommand`, `TabEvent`, `CompMsg`, with serde), ready to cross a process boundary. V8: one isolate per tab; one context per frame realm; same-origin frames share the
isolate and can hand each other objects. Between origins, V8's access check on the `Window` and `Location` templates
decides (`snowlark-js/src/access_check.rs`, `snowlark-bindings/src/rt/cross_origin.rs`): a realm that is not same
origin-domain reaches only HTML's cross-origin properties, and a browsing context keeps one WindowProxy (one V8 global
proxy) across its navigations. They share the address space all the same. The move to processes is planned in
`docs/process-isolation.md`.

**Security defaults.** Mixed content blocked, HSTS persisted, file:// isolated, CSP enforced from the start,
`unsafe` only in snowlark-js, snowlark-paint, snowlark-graphics's Windows clock (`ticks`), snowlark-media's Windows half, snowlark-ipc, snowlark-sandbox, snowlark-gl (its calls into ANGLE) and the generated
bindings, each `unsafe` block with a comment saying what invariant makes it sound.

**Testing from day one.** `snowlark --wpt <list> <outdir>` runs web-platform-tests headless; `tools/wpt/run.py`
drives it over a WPT checkout, summarises the results per spec area and compares them with a stored baseline
(`tools/wpt/README.md`). Every crate has unit tests. `cargo test` must stay green on main.

## Milestones

| | Done when |
|---|---|
| M0 | `snowlark --eval` runs JS through V8; CI-equivalent script builds and tests all crates. |
| M1 | DOM + bindings: `snowlark --wpt` runs `dom/nodes`, `dom/traversal`, `dom/events` and `html/dom`; the generator produces Node/Element/Document/Event families from IDL; `--eval` can parse an HTML string and query it. |
| M2 | Style: stylo integrated, `getComputedStyle` works, `css/selectors`, `css/cssom`, `css/css-cascade` run. |
| M3 | Layout + paint: `--render` draws block/inline/flex/grid/table pages to PNG; `css/css-flexbox`, `css/css-grid`, `css/css-position` run; reftests supported. |
| M4 | Page: fetch, navigation, event loop, timers, frames; `--render` draws real sites; Speedometer completes. |
| M5 | Shell: tabs, window chrome, GPU compositing, video; usable as a daily browser. |

M0 to M5 are done. Process isolation (renderer, GPU and network processes, a sandbox) is under way:
`docs/process-isolation.md`. How performance is measured, and current numbers: `docs/benchmarks.md`.

A password manager (saving and filling logins, an encrypted store, `snowlark://passwords`) is built in four
stages: `docs/password-manager.md`.

WebGL runs on ANGLE (OpenGL ES over Direct3D 11) behind a command protocol that can cross a process boundary, in
four stages from a WebGL 1 context to conformance and WebGL 2: `docs/webgl.md`.

Applications are to embed the engine through a crate below the window, `snowlark-embed` (an engine handle, web views,
surfaces the application supplies, delegate traits), with the browser window as its first user: `docs/embedding.md`
has the API, what moves out of `snowlark-shell`, the process options and the steps.

## Linux, then macOS

Snowlark runs only on Windows today. The platform parts (painting and text, GPU compositing, media decoding, the
window and input, IPC and the process sandbox, the OS calls in `snowlark-shell`) each get a Linux and later a macOS
half behind one interface, chosen at compile time, so that the engine crates stay platform-free.
`docs/linux.md` maps every Windows-only part with its Linux counterpart and the Rust options, and lays out the port
in stages: the platform-free crates check for Linux (`tools/linux/check.ps1`); `snowlark-paint` is split into a
platform-free crate (`snowlark-graphics`: geometry, the display list, the text types, the Rust image decoders) and
its platform half, so that layout and render check too; next, the headless modes on Linux, processes, the window, WebGL, media, the sandbox and packaging. The
painting backend, the text stack, the window toolkit, the media stack and the Linux test machine are open
decisions listed there.
