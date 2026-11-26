# Contributing to Snowlark

Read [ARCHITECTURE.md](ARCHITECTURE.md) first: the crates, the dependency direction and the main decisions.
This file is how to build, test and measure, and the rules the code follows. Taking part means following the
[code of conduct](CODE_OF_CONDUCT.md); security problems are reported privately as [SECURITY.md](SECURITY.md) says.

## Prerequisites

- Windows 10 or 11, x64.
- Visual Studio 2022 (or the Build Tools) with the "Desktop development with C++" workload: the MSVC x64
  toolset and a Windows SDK.
- Rust stable for `x86_64-pc-windows-msvc`, installed with [rustup](https://rustup.rs).
- Git, and Python 3.10 or newer for the tools under `tools/`.
- For web-platform-tests: a checkout of [web-platform-tests/wpt](https://github.com/web-platform-tests/wpt).
- For comparisons with another engine (`tools/layout-diff`, `tools/visdiff`, `tools/census`, `tools/vs-chrome`, the micro
  benchmarks' `--chrome`): Google Chrome in its default place, or `--chrome <exe>`.

## Building

Always go through `dev.ps1`, from PowerShell. It runs cargo inside the Visual Studio x64 developer environment
(found with `vswhere`; rustc on its own picks the newest Visual Studio, which fails when that one has no C++
libraries) and chooses the V8 library to link ([docs/v8-build.md](docs/v8-build.md)).

```powershell
.\dev.ps1 build --profile iter
```

- `--profile iter` is the profile for day-to-day work, tests and WPT runs: the release optimisation level, but
  incremental, with 16 codegen units and no LTO. Binaries are in `target\iter\`.
- `--release` (thin LTO, `target\release\`) is for performance work only: Speedometer, benchmarks, A/B timing.
  An `iter` build is about 13% slower than a release build on the median micro benchmark, so never compare
  timings across the two profiles.
- A plain `.\dev.ps1 build` is a debug build; `.\dev.ps1 test` uses it.

Two native libraries are built once from their official source and found by `dev.ps1` in the dev folder:
ANGLE, for WebGL (`tools\angle\build.ps1`, `docs/webgl.md`), and dav1d, the AV1 decoder of AVIF images
(`.\tools\dav1d\build.ps1`: a static library of the release `tools\dav1d\pin.txt` names, about two minutes; it
fetches NASM, meson and ninja for itself). Without ANGLE the build has no WebGL; without dav1d it decodes AVIF
through Windows' codec instead, and the AVIF tests in `snowlark-graphics` fail. (For Linux, the folder holds
`libdav1d.a` in place of `dav1d.lib`.)

`snowlark.exe` is a Windows-subsystem program: the window opens without a console, and PowerShell neither waits
for it nor captures its output. Scripts use `snowlark-cli.exe`, the same program built for the console
subsystem.

### Settings

All optional. Set them in the environment, or as `KEY=VALUE` lines in a file `dev.env` in the repository root
(git-ignored; a linked git worktree without one uses the main checkout's). `dev.ps1` and the tools
(`tools/devenv.py`, `tools/devenv.ps1`) read both; the environment wins.

| Setting | Meaning | Default |
|---|---|---|
| `SNOWLARK_DEV_DIR` | A folder for the crate cache (`CARGO_HOME` becomes `<dev dir>\.cargo-home`), the build output of checkouts on another drive (`<dev dir>\.target\<checkout>`, with `target` a junction to it), a locally built V8 library, slot locks, and the tools' scratch files and results. Put it on an SSD. | unset: cargo's defaults (`~\.cargo`, `.\target`); the tools use `.dev\` in the repository |
| `SNOWLARK_VCVARS` | The `vcvars64.bat` to use. | the newest Visual Studio whose MSVC toolset has libraries |
| `SNOWLARK_MIN_FREE_GB` | `dev.ps1` waits, and says so, while the build drive has less free space than this: a build that runs out of disk leaves a broken target directory that looks like a compile failure. | 25 |
| `SNOWLARK_SLOTS` | `1` queues builds, test runs and WPT runs on machine-wide slots (3 builds, 2 test runs, 2 WPT runs; 8 cargo jobs each), for a machine where several checkouts build at once. `SNOWLARK_NO_SLOTS=1` turns it off for one command. | off |
| `SNOWLARK_V8_DIR` | The folder with `rusty_v8.lib` and `src_binding.rs` to link; the pinned library is downloaded into it when they are missing. | `<dev dir>\v8\<name in tools\v8-pgo\pin.txt>`, or `target\v8\<name>` without a dev folder |
| `SNOWLARK_DAV1D_DIR` | The folder with the static `dav1d.lib` to link (`tools\dav1d\build.ps1` makes it). | `<dev dir>\dav1d\<name in tools\dav1d\pin.txt>` |
| `SNOWLARK_V8_PREBUILT` | `1` leaves V8 to the `v8` crate (its own published library, where one exists for the version and features). | off |
| `SNOWLARK_WPT` | The web-platform-tests checkout. | a folder `wpt` next to the repository |
| `SNOWLARK_WPT_STATE` | Where WPT results and baselines are kept. | `<dev dir>\wpt` |
| `SNOWLARK_WPT_WORK` | Scratch space of a WPT run (profiles, screenshots). | `<dev dir>\wpt-work` |
| `SNOWLARK_SPEEDOMETER` | A local copy of Speedometer 3.1. | `<dev dir>\speedometer-3.1` |
| `SNOWLARK_TEST_WINDOW_POS` | Where the tools that drive a real window (`window_ab.py`, `tools/scroll-ab`, `tools/scrub`) open it, as `x,y`: a monitor you are not working on. | `100,100` |

## Tests

```powershell
.\tools\test-workspace.ps1
```

This runs every crate's tests with the `iter` profile and `--no-fail-fast`, writes the whole output to
`target\test-workspace.log` and prints a summary that marks each failure as known (listed in
`tools/test-workspace-known.txt`) or new; it exits 0 when every failure is known. `-FromLog <log>` summarises an
earlier run without running anything. A full run compiles and links every test binary, which is most of its time.
While iterating, run `.\tools\test-workspace.ps1 -Affected`: the crates your branch changes and every crate that
depends on them (`-ListOnly` prints which). Leave the full run for once before a pull request.

- Unit tests live in the crates' sources.
- Each crate's integration tests are **one** test binary, `it` (`crates/<crate>/tests/it.rs`, with
  `autotests = false` in its `Cargo.toml`): every file in `tests/` is a module of it, so a test is named
  `<file>::<test>`. Run one file with `--test it <file>::`, for example
  `.\dev.ps1 test -p snowlark-page --test it csp:: '--' --nocapture` (quote `--`: PowerShell drops a bare one),
  one test with its full name, and the ignored benchmarks with `--test it bench:: '--' --ignored --nocapture`.
- A new file in `tests/` needs one line, `mod <file>;`, in that crate's `tests/it.rs` (a file that declares its
  own `mod common;` also needs `#[path = "<file>.rs"]` above it). The test `every_test_file_is_listed` fails if
  one is missing.
- A test that must have a process of its own (it changes process-wide state, such as environment variables)
  gets its own `[[test]]` target in the crate's `Cargo.toml`.
- Libraries have no doctest harness (`doctest = false`), and the `snowlark` binaries no test harness.
- A test that opens a window sets `SNOWLARK_NO_ACTIVATE=1`, so it never takes the keyboard focus.

Before a commit, these must be clean:

```powershell
.\dev.ps1 clippy --workspace --all-targets
```
```powershell
.\dev.ps1 fmt --all
```
```powershell
python tools/license_headers.py
```

The crates free of platform code also compile for Linux, checked from Windows (`rustup target add
x86_64-unknown-linux-gnu` once; [docs/linux.md](docs/linux.md)). A change to one of them keeps this clean:

```powershell
.\tools\linux\check.ps1
```

On a Linux machine, cargo runs through `tools/linux/cargo.sh`, which links the V8 library
`tools/linux/build-v8.sh` builds ([docs/v8-build.md](docs/v8-build.md), "Linux").

## Running and diagnosing

`snowlark.exe [url]` opens the browser. Headless modes (use `snowlark-cli.exe`):

- `--eval "<js>"` runs a script in an empty page and prints the result.
- `--render <url> <png> [width]` loads a page and writes a screenshot.
- `--wpt <list> <outdir>` and `--reftest <list> <outdir>` run web-platform-tests; `tools/wpt/run.py` drives them.
- `--bench` and `--speedometer` run the micro benchmarks and Speedometer; the tools below drive them.

CLI modes live in `crates/snowlark/src/cli/<mode>.rs`, each pulled in by its own match arm in `main.rs`.

Environment variables, in any mode that runs a page unless noted:

| Variable | Effect |
|---|---|
| `SNOWLARK_PROFILE=1` | Prints a `[render]` line per rendering update (restyles, boxes built and laid out, runs shaped, forced style and layout by the binding member that forced them) and, when the page closes, V8's sampling CPU profile, a native sampling profile of the page thread, and calls and time per binding member (`[profile]` lines on stderr). `SNOWLARK_PROFILE_FOLDED=<file>` also writes the native stacks in the folded format of flame graph tools; `SNOWLARK_PROFILE_JSON=<file>` writes V8's profile as a DevTools `.cpuprofile`. Getters take the slow path while profiling: profile for causes, time without it. |
| `SNOWLARK_TRACE=1` | An `[exec]` line per script with its time. |
| `SNOWLARK_GC_STATS=1` | After a V8 collection, the heap, the removed frames and replaced documents not freed yet, and what keeps each alive (`[gc]` lines). With `SNOWLARK_HEAP_SNAPSHOT=<file>` a V8 heap snapshot is written once eight removed frames are alive; `python tools/heap-retainers/retainers.py <file> [--holders]` prints the retaining paths. |
| `SNOWLARK_V8_FLAGS` | Flags passed to V8. |
| `SNOWLARK_PROFILE_DIR` | The profile folder (default `%LOCALAPPDATA%\Snowlark`). |
| `SNOWLARK_NO_ACTIVATE=1` | The window never takes the foreground. |
| `SNOWLARK_SCALE=1.5` | Draws the window as on a monitor at that display scale (0.5 to 4). |
| `SNOWLARK_PROCESS_MODEL=single` | Keeps every tab on a thread of the browser's process instead of a renderer process each (also `--process-model=single`). |
| `SNOWLARK_IPC=loopback` | Puts every tab message through the wire format inside `send`, so the window and `snowlark-shell`'s tests run as if each crossed a process. |
| `SNOWLARK_PLAYREADY=0` | Stops offering the `com.microsoft.playready.recommendation.3000` key system to pages in the window (offered by default where Windows has it: Windows 10 2004 or later, hardware DRM). The headless modes never offer it. |
| `SNOWLARK_TAB_STATS=1`, `SNOWLARK_GPU_STATS=1`, `SNOWLARK_VSYNC_STATS=1` | In the window: a line per batch of input a tab handles, per raster and presented frame, and per second of vsyncs. |

## Web-platform-tests

Conformance work is done by spec area and measured with WPT. `tools/wpt/run.py` runs testharness tests and
reftests in Snowlark and summarises them per area; [tools/wpt/README.md](tools/wpt/README.md) has the details.

```powershell
python tools/wpt/run.py dom/nodes css/css-flexbox --snowlark target/iter/snowlark.exe --jobs auto
```

The merge check compares a run, subtest by subtest, with a stored baseline, re-runs apparent losses alone and
exits non-zero on a real one:

```powershell
python tools/wpt/run.py --gate --baseline <name> --snowlark target/iter/snowlark.exe --jobs auto
```

A baseline comes from a gate or full run of `main` (`--save-baseline <name>`). Known flaky subtests are listed
in `tools/wpt/flaky.json`. A change that lowers an area's count is a regression unless the pull request says
why.

More tools locate conformance problems:

- `tools/layout-diff/` renders a URL in headless Chrome and in Snowlark and reports the first element whose box
  or computed style differs: the locator for "this page looks wrong".
- `tools/visdiff/` loads each page of a corpus in both engines, lets it settle (animations ended, content
  revealed on scroll), and reports the content Chrome shows and Snowlark leaves invisible, empty, unloaded or
  out, ranked by area and grouped by cause: the locator for "this page is blank or half there".
- `tools/census/` diffs the web-exposed API surface against Chrome's, weighted by how many sites of a corpus use
  each missing member, and records uncaught exceptions and blank renders per site.
- `tools/sweep/` ranks whole features by how many sites of the corpus they break: CSS Chrome accepts and Snowlark
  refuses, layout-diff root causes by layout feature, and repaints and blank rows while scrolling.
- `tools/payoff/` joins the census, the sweep and WPT results into one list of spec areas and features, scored by
  the sites each breaks, reported issues and failing WPT subtests, per size.

## Performance

Performance is measured, not assumed: a change claimed as faster comes with before and after numbers, measured
back to back on the same machine with the same build profile. [docs/benchmarks.md](docs/benchmarks.md)
describes the micro benchmarks (`bench/micro`, `tools/micro-bench/run.py`), the Speedometer tools
(`tools/speedometer/`), the comparison tools (`tools/vs-chrome/`) and the profiling counters
([docs/perf-counters.md](docs/perf-counters.md)).

## Rules

- **Spec conformance comes first.** Browser features layered on the engine (content blocking, developer tools,
  the shell) must not change what pages observe or how the engine behaves in `--wpt`, `--render` and `--eval`:
  such features are off there unless explicitly enabled, keep their injected styles at user origin, and add no
  page-visible globals or nodes.
- **Specs are the source of truth.** Implement from the WHATWG and W3C text (DOM, HTML, CSSOM, Fetch, Web IDL),
  not from memory of what browsers do, and fix problems by spec area rather than for one site: a site report
  locates the area.
- **No handwritten bindings.** Everything JavaScript can see comes from `snowlark-idl` out of the IDL under
  `idl/`. If an interface or member is missing, add its implementation; never add an ad-hoc V8 property.
- **Dependency direction** is as `ARCHITECTURE.md` draws it. `snowlark-dom` must not depend on V8, style or
  layout. A lower crate that needs something from above gets a trait or a callback.
- **`unsafe`** only in `snowlark-js`, `snowlark-paint`, `snowlark-graphics/src/ticks.rs` (the Windows clock), `snowlark-media/src/win`, `snowlark-ipc`, `snowlark-gl` and generated
  code, each block with a `// SAFETY:` comment.
- **Process isolation** ([docs/process-isolation.md](docs/process-isolation.md)): no new shared object in
  `TabContext`, `DriverContext` or `PageOptions` without a message-shaped trait. No `std::fs` or OS calls in
  `snowlark-page`, `snowlark-render`, `snowlark-style` or `snowlark-layout`. A new display list `Item` updates
  encode, decode, `validate` and the fuzz corpus. Security decisions about other sites go in `snowlark-net` or
  the browser half, never in the page crates.
- **Ids, not references, across the tree.** `NodeId` carries a generation; never store raw indices.
- **Hot paths allocate nothing per call.** Attribute lookups, tree walks and getters return borrowed data or
  small copies. Strings crossing into V8 are created from UTF-8 slices once; tag and attribute names are
  interned atoms.
- **Thread safety is by type.** Page state is `!Send` and lives on its page thread; Stylo's traversal sees the
  DOM through `Sync` views. No `static mut`, no global mutable state.
- **Tests with every change**: unit tests in the crate, and a WPT area run for anything JavaScript can see.
  Show that a new test fails without its fix.
- **Vendored crates** (`vendor/`) change only with an entry in that crate's `SNOWLARK-PATCHES.md`. Changing the
  `v8` crate's version follows [docs/v8-upgrade.md](docs/v8-upgrade.md).
- **Licence.** New source files carry the MPL-2.0 notice (`tools/license_headers.py --fix` adds it). Don't add
  code under a licence that is incompatible with the MPL-2.0, and note third-party data in `THIRD-PARTY.md`.
- **Style.** LF line endings, `rustfmt` defaults, edition 2021, clippy clean.

## Commits and pull requests

- One verified step per commit. The subject line says what changed in spec terms ("MediaQueryList: a list lives
  while it has listeners"), not which site it fixes.
- Say in the pull request what you ran: the crates' tests, the WPT areas with their counts before and after,
  and the benchmark numbers for anything that can move performance.
- By contributing you agree that your contribution is licensed under the Mozilla Public License 2.0.
- Contributions written with AI assistants are welcome, as most of Snowlark's own code is. You are responsible for
  what you submit: say that an assistant was used, and run the same checks as for any other change.
