# Third-party components and data

Snowlark's own code is under the Mozilla Public License 2.0 (`LICENSE`). This file lists what in the repository
comes from elsewhere. The crates Snowlark links from crates.io are not listed here: each release's
`THIRD-PARTY-NOTICES.txt` (made by `tools/release/notices.py`) names every one with its licence text, and the
browser's credits page shows the same.

## Vendored crates (`vendor/`)

Patched copies, taken through `[patch.crates-io]` in `Cargo.toml`. Each keeps its own licence files and headers;
`SNOWLARK-PATCHES.md` in each folder lists the changes.

| Folder | Upstream | Licence |
|---|---|---|
| `vendor/html5ever` | [servo/html5ever](https://github.com/servo/html5ever) 0.40.1 | MIT or Apache-2.0 |
| `vendor/stylo` | [servo/stylo](https://github.com/servo/stylo) 0.22.0 | MPL-2.0 |
| `vendor/taffy` | [DioxusLabs/taffy](https://github.com/DioxusLabs/taffy) 0.15.1 | MIT |
| `vendor/url` | [servo/rust-url](https://github.com/servo/rust-url) 2.5.8 | MIT or Apache-2.0 |
| `vendor/v8` | [denoland/rusty_v8](https://github.com/denoland/rusty_v8), its roll to V8 15.8 | MIT |

The V8 library itself is not in the repository (`docs/v8-build.md`); V8 is under a BSD-style licence, and the
notices file lists the libraries it links.

## ANGLE (WebGL)

WebGL runs on [ANGLE](https://chromium.googlesource.com/angle/angle), the OpenGL ES implementation over Direct3D 11,
under the BSD 3-Clause licence. It is not in the repository: `tools/angle/build.ps1` builds the commit pinned in
`tools/angle/pin.txt` from its official source, and the browser loads the two libraries it makes, `libEGL.dll` and
`libGLESv2.dll`, from its own folder at run time (`docs/webgl.md`). The code ANGLE links from its own
`third_party` folder keeps its licences there.

## Specifications' Web IDL (`idl/`)

The `.idl` files are the IDL fragments of the W3C and WHATWG specifications as extracted by
[w3c/webref](https://github.com/w3c/webref) (the `@webref/idl` package). They belong to their specifications,
under those specifications' licences (W3C Software and Document License, CC BY 4.0 for WHATWG standards). The
bindings in `crates/snowlark-bindings/src/generated/` are generated from them by `snowlark-idl`.

## Test and benchmark data

- `crates/snowlark-layout/tests/fixtures/Ahem.ttf`: the Ahem test font from web-platform-tests, which its
  authors have placed in the public domain.
- `crates/snowlark-render/tests/fixtures/noto-sans-latin-400.woff2`: a subset of Noto Sans, SIL Open Font
  License 1.1 (see the `README.md` next to it, which also describes the other fixtures there).
- `crates/snowlark-media/tests/fixtures/pattern.mp4`, `pattern-frag.mp4`: a generated test pattern, encoded by
  Snowlark's own test code. MPL-2.0 like the rest.
- `bench/micro/resources/newssite/*.css` and the markup in `bench/micro/style/newssite-*.html`: the rendered
  page and style sheets of the NewsSite workload of [Speedometer 3](https://github.com/WebKit/Speedometer)
  (BSD 2-Clause), used as a realistic input for style benchmarks.
- `crates/snowlark-dom/tests/html5lib.rs` and the WPT tools read tests from a web-platform-tests checkout
  (3-Clause BSD), which is not part of this repository.

## Content blocking (`crates/snowlark-blocker`)

**Engine.** Matching uses Brave's [adblock-rust](https://github.com/brave/adblock-rust) (the
`adblock` crate), licensed under the Mozilla Public License 2.0. It is a crates.io dependency, built
unmodified; its source is available from crates.io and its repository.

**Filter lists.** Snowlark does not ship filter lists. On first run, and then as each list's
`! Expires:` says, the browser downloads the lists the user has enabled from their publishers'
canonical addresses into the profile folder (`<profile>\blocker\lists`). The defaults are the ones
uBlock Origin enables:

| List | Publisher | Licence |
|---|---|---|
| uBlock filters (Ads, Badware risks, Privacy, Unbreak, Quick fixes; Annoyances and Cookie notices off by default) | uBlock Origin (uAssets) | GPL-3.0 |
| EasyList, EasyPrivacy (EasyList Other annoyances and Cookie notices off by default) | The EasyList authors | Dual GPL-3.0 / CC BY-SA 3.0 |
| Online Malicious URL Blocklist | malware-filter (curbengh), from abuse.ch URLhaus data | CC0 (list), URLhaus data CC0 |
| Peter Lowe's Ad and tracking server list | Peter Lowe (pgl.yoyo.org) | Free to use under its site's terms (no redistribution: downloaded, never bundled) |

**Resources.** uBlock Origin's scriptlets (`src/js/resources/*.js`) and redirect resources
(`src/web_accessible_resources`, mapped by `src/js/redirect-resources.js`) are GPL-3.0. They are
likewise downloaded at run time from uBlock Origin's repository into the profile
(`<profile>\blocker\resources.json`) and never bundled.

**Built in.** What the binary does contain is Snowlark's own (MPL-2.0): a minimal list of well-known
advertising and tracking servers (`crates/snowlark-blocker/src/fallback.txt`), used only until the
first download completes, and trivial neutered resources (empty script, style, text, HTML and
JSON documents, empty VAST/VMAP documents, transparent 1x1/2x2/3x2/32x32 images) under uBlock
Origin's resource names so that `redirect=` filters find them before uBlock Origin's own arrive.
