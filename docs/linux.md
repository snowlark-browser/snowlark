# Snowlark on Linux

Snowlark runs on Windows; on Linux it builds with its headless modes, and the window and the rest follow in the
stages below. This document maps every part of it that is tied to Windows, names the Linux
counterpart and the Rust options for each, and lays out the order of the port in stages. The engine crates stay
free of platform code; each platform part sits behind one interface with a Windows and a Linux half.

## Where it stands

**These crates check for Linux** (`cargo check --target x86_64-unknown-linux-gnu`, tests included), from
Windows: `snowlark-dom`, `snowlark-idl`, `snowlark-audio`, `snowlark-idb`, `snowlark-style`, `snowlark-net`,
`snowlark-js`, `snowlark-bindings`, `snowlark-graphics`, `snowlark-paint`, `snowlark-layout` and `snowlark-render`.
Keep them so:

```powershell
.\tools\linux\check.ps1
```

It needs the target once (`rustup target add x86_64-unknown-linux-gnu`). Build scripts compile C for the target
(`ring`, `zstd`, `mimalloc`) and Windows has no compiler for Linux; `cargo check` never links, so
`tools/linux/stub_cc.py` stands in for the compiler and archiver and writes empty outputs.

**Built and tested on Linux:** `snowlark-dom`, `snowlark-idl`, `snowlark-audio`, `snowlark-idb`, `snowlark-style`
and `snowlark-net` build natively and pass their tests there (11 test binaries, 588 tests, none failing).
`snowlark-graphics` and `snowlark-paint` build and pass their tests too (115 tests), except `snowlark-graphics`'s
six AVIF tests, which need the dav1d library (below). `snowlark-js` and `snowlark-bindings` build against the
Linux V8 library and pass their tests (69, among them the access check, streaming, code cache, object clone and
inline field tests that drive the mangled-name declarations through V8). `snowlark-paint` has the Linux text
engine (below) with its tests, and the Skia painter with its own. `snowlark-layout` and `snowlark-render` build
and pass their tests (`snowlark-render`'s GPU canvas tests are ignored there). `snowlark-page`, `snowlark-media`
and `snowlark-ipc` build and pass their tests; the page tests that need WebGL (stage 6), a media backend (stage
7), the GPU canvas (stage 5) or the PDF viewer (stage 9) are ignored there, each with its reason.

**The binary** builds on Linux with its headless modes: `--eval`, `--render`, `--wpt`, `--reftest`, `--fetch`,
`--computed`, `--layout-demo`, `--media-types` and `--update-filters`. The window, the renderer and GPU processes
and the modes that need a GPU device or Windows' CPU counters (`--video`, `--render-demo`, `--bench`,
`--speedometer`) are compiled out until their stages; `snowlark` without a `--` mode says so. `snowlark-tab`
builds only its platform-free parts there (the network thread's start, WebDriver actions), and `snowlark-shell`
is not built. What is missing behaves as on a system without it: a media element fails with
`MEDIA_ERR_SRC_NOT_SUPPORTED`, `MediaSource` cannot attach, `getContext("webgl")` returns null, canvases draw on
the CPU, and HTML's "PDF viewer supported" is false (so `navigator.pdfViewerEnabled` is false and
`navigator.plugins` empty).

**Processes:** `snowlark-ipc`'s `posix/` half gives the process boundary the same names and rules as on
Windows, and its tests (a real second process, links between children, the loopback) run on both systems. The
channel to a child is a pair of Unix stream sockets (`socketpair`, never named), read by one epoll thread per
process. A descriptor crosses as a `Handle` frame whose first byte carries it as `SCM_RIGHTS`; the receiver
keeps it under the value the sender chose until a message names it (a child in its process's table, the
browser with that child's connection, so a child can only ever name its own). Sections are `memfd_create`
files sealed against shrinking and growing, and a mapping is refused for a file that is smaller than asked or
not sealed. A child starts from one long-lived launcher thread (the parent-death signal follows the thread that
forked) with its channel as descriptor 3 and every other descriptor marked close-on-exec, the parent-death
signal set before exec, its environment on an allow-list, and `JobLimits::commit` as its `RLIMIT_DATA`. Not
there yet: `JobLimits::no_children` (it needs a system call filter), crash dumps, and a fork server (each launch
forks the browser itself, which copies its page tables); `Process::set_background` lowers every thread's
priority, and raising it again needs a privilege the browser may lack.

**Identity:** the default User-Agent carries the platform token of the family's reduced strings for Linux,
`X11; Linux x86_64` (Windows keeps `Windows NT 10.0; Win64; x64`); `navigator.platform` (`Linux x86_64`),
`navigator.userAgentData.platform` and `Sec-CH-UA-Platform` (`Linux`) follow it. The platform version hint is
empty there. Per-site User-Agents are the same on both systems.

**Fonts:** the CSS generic families resolve as on other systems' defaults: `serif` to Times New Roman,
`sans-serif` to Arial, `cursive` to Comic Sans MS and `fantasy` to Impact, through fontconfig's rules (which
bind them to metric-compatible faces such as Liberation's where those fonts are absent), with fontconfig's own
generic as the next choice; `monospace` and `system-ui` are fontconfig's. The font database of SVG text and CPU
canvases (fontdb) knows the same families: its generic families are the text engine's choices, and Arial,
Helvetica, Times New Roman, Times, Courier New, Courier, Comic Sans MS and Impact, where not installed, name
the faces fontconfig binds them to.

### A Linux machine for building

Any x86-64 Linux with a recent distribution works; WSL 2 with Ubuntu on Windows is enough for the engine crates
and the headless modes (its window and GPU are not representative of a Linux desktop). Keep the checkout and its
`target` on the Linux file system: a checkout under `/mnt/<drive>` (the Windows drives) builds several times
slower.

```sh
apt-get install -y build-essential clang lld pkg-config curl ca-certificates git python3 cmake ninja-build \
    libfontconfig-dev libfreetype-dev libharfbuzz-dev \
    fonts-liberation fonts-dejavu-core fonts-noto-core fonts-noto-cjk fonts-noto-color-emoji
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal \
    --default-toolchain stable -c rustfmt -c clippy
git clone <the repository> snowlark && cd snowlark
cargo test --profile iter --no-fail-fast -p snowlark-dom -p snowlark-idl -p snowlark-audio -p snowlark-idb \
    -p snowlark-style -p snowlark-net -p snowlark-graphics -p snowlark-paint
```

fontconfig and HarfBuzz are the text engine's libraries (FreeType comes with them and is the rasterizer's);
the fonts are the ones the text engine's tests expect (Liberation for the metric-compatible Arial, Times New
Roman and Courier New, DejaVu, Noto for other scripts, CJK and color emoji). Add `-j <n>` on a machine other work
shares. `dev.ps1` is Windows-only; on Linux cargo runs directly.

`snowlark-js`, `snowlark-bindings` and every crate above them need a V8 library to link, and none is published
for Linux at the V8 version the workspace pins (`vendor/v8`, with pointer compression): `tools/linux/build-v8.sh`
builds it from the crate's source, as the Windows library is built, and `tools/linux/cargo.sh` runs cargo with it
(`docs/v8-build.md`, "Linux"):

```sh
tools/linux/cargo.sh test --profile iter -p snowlark-js -p snowlark-bindings
```

The V8 entry points the `v8` crate does not expose (the access check with interceptors, the accessor cache,
consuming a code cache on a worker, streamed compilation, object clones, global handles) are declared by their
MSVC names on Windows and their Itanium names on Linux, checked against each library (`docs/v8-upgrade.md`), so
they work the same on both.

`snowlark-js` has two Windows-only pieces, each with a fallback elsewhere:

- The native sampling profiler (`native_profile.rs`, `SNOWLARK_PROFILE=1`) suspends the thread and unwinds with the
  Windows unwind tables. Elsewhere `NativeSampler::start` returns `None`; the V8 profile and the binding counts
  still work. The Linux design: the sampler sends a signal to the profiled thread, whose handler copies its
  registers and stack into a preallocated buffer (only async-signal-safe code there), and the copy is unwound with
  the DWARF call frame information afterwards; names come from the ELF symbols.
- The C runtime's heap on mimalloc (`crt_heap.rs`) is only for the static MSVC runtime; on Linux the allocator is
  set in the binary as on Windows.

**Painting: `snowlark-graphics` and `snowlark-paint`.** What no platform API is behind is in `snowlark-graphics`:
geometry, the display list, filters, layer animations, scroll curves, a canvas's operations as data, the text types
every text engine fills in (font queries, shaping options, shaped runs and clusters) and the cache of shaped runs,
the image decoders written in Rust (PNG, JPEG, GIF, WebP, AVIF) with colour profiles and animation, the image
store, and `Ticks` (the performance counter on Windows, `CLOCK_MONOTONIC` on Linux). Its errors are its own
(`snowlark_graphics::Error`). `snowlark-paint` re-exports all of it under the same paths and keeps the platform's
half; a module with a Linux half has it in `src/linux/<module>.rs` under the same name, with the same items:

- `text`: `TextEngine` with the Windows engine's methods, on fontconfig and HarfBuzz (both linked dynamically,
  declared in `linux/ffi.rs`). A family is matched through fontconfig's rules and counts as present only when
  the face is that family or one fontconfig binds strongly to it (Liberation Sans for Arial), so a CSS family
  list falls through missing families as on Windows; a character none of the list's faces has falls back to the
  first font of fontconfig's sorted list for the text's language that has it (cached per language). HarfBuzz
  shapes each run of one font, script (UAX #24, `unicode-script`), bidi level (UAX #9, `unicode-bidi`) and
  orientation, gives the metrics (OS/2 and hhea, as HarfBuzz reads them) and the outlines for
  `text-decoration-skip-ink`; line-break opportunities are UAX #14's (`icu_segmenter`). Web fonts pass OTS
  first, as on Windows. `font_data` gives the raster side a face's bytes, index, variation values and
  synthesized bold or oblique. `text_cache`'s per-word cache (whose segments follow DirectWrite's script
  analysis) is not used: whole runs are shaped and cached.
- `raster`: a painter over a Skia canvas (`skia-safe`), and `HeadlessRenderer` on Skia's CPU backend. Each group of
  the list is a Skia save or layer (an opacity group a layer with that alpha, a filter group a layer whose paint
  carries the filters as an image filter, a mask a layer the mask is composited onto with destination-in, a
  backdrop filter a layer with a backdrop filter and mirrored edges); filter functions and SVG filter primitives
  are Skia's image filters (`raster_filters.rs`, `raster_svg.rs`); box edges snap to device pixels as on Windows;
  text is drawn with typefaces made from the text engine's `font_data`. Skia links the system's FreeType (no
  copy of its own), the one fontconfig and HarfBuzz load. `canvas_thread`: the GPU canvas, which cannot be made,
  so `snowlark-render` draws canvases on its CPU backend (its GPU canvas tests are ignored on Linux); `gpu`: only
  the `Driver` choice.
- `image`: of the formats Windows decodes through WIC, BMP, ICO and WBMP go through Skia's codecs and TIFF is
  refused (`Error::Unsupported`); rendered pixmaps are written as PNG with the `png` crate.

`skia-safe`'s build downloads Skia's prebuilt library for the target and features once (from the project's
releases; a machine without network access builds it from source, which needs `python3`, `ninja` and `clang`).

The rest of `snowlark-paint` (the compositor, tiles, video, the window, cursors, the clipboard, the PDF viewer, the
password store's key) is Windows-only and compiled out elsewhere; its integration tests too. `video` has a
Linux half that hands out slots no frame reaches, `pdf` one that renders nothing (`AVAILABLE` false), and `gpu` a
`Gpu` that cannot be made.

`snowlark-gl` (ANGLE), `snowlark-ipc` (pipes, sections, process launch), `snowlark-media` (Media Foundation,
PlayReady, WASAPI) and the window half of `snowlark-paint` are platform halves by nature: each gets a Linux half.

`snowlark-graphics`'s AVIF decoder needs the dav1d library as on Windows (`SNOWLARK_DAV1D_DIR` naming a folder that
holds `libdav1d.a`); without it AVIF images are refused on Linux, where there is no system decoder to fall back on.

## The map

Sizes: **S** is days, **M** one to two weeks, **L** several weeks of work.

| Part | Today (crate, module) | Windows API | Linux counterpart in the established design | Rust options and what they cost | Size |
|---|---|---|---|---|---|
| Display list raster | `snowlark-paint` `raster.rs`, `effects.rs`, `svg_effects.rs`, `raster_tables.rs` | Direct2D (WIC bitmaps headless) | a 2D library with a CPU and a GPU backend (GL or Vulkan) | **Skia** (`skia-safe`: the most complete, GPU and CPU, C++ library of about 20 MB, prebuilt binaries per target); **Vello** (Rust, GPU compute, young; its CPU renderer is younger still); **tiny-skia** (Rust, CPU only, already linked through `resvg`; no GPU, slower on large areas); Cairo (C, CPU, old) | L |
| GPU tiles and canvas | `gpu.rs`, `canvas.rs`, `canvas_thread.rs` | Direct3D 11, Direct2D on DXGI surfaces | GL or Vulkan textures through the 2D library | follows the raster choice | M |
| Compositor | `compositor.rs`, `layer_animation.rs` (platform-free), `video.rs` | DirectComposition, D3D11 shared textures | the compositor draws layers with GL or Vulkan into the window's surface (EGL on Wayland or X11); video frames as dma-buf textures | `glow` or `ash` with EGL (`khronos-egl`); `wgpu` (one API over Vulkan and GL, some overhead and less control of presentation) | L |
| Text: fonts, shaping, metrics, glyph outlines | `text.rs`, `text_cache.rs`, `ink.rs`, `web_font.rs`, `font_files.rs` | DirectWrite | fontconfig for matching and fallback, FreeType for outlines and rasterization, HarfBuzz for shaping, ICU for breaks; web fonts sanitized before loading (OTS, as now) | **fontconfig** (C, on every desktop; `fontconfig` or `yeslogic-fontconfig-sys`) with **harfrust** or `rustybuzz` (HarfBuzz in Rust) and **skrifa** (outlines, metrics; `swash` for glyph rasterization); or the C FreeType and HarfBuzz; `fontdb` alone (already linked) finds fonts but has no fallback rules or aliases | L |
| Images: other formats | `image.rs` | WIC (BMP, ICO, TIFF and others) | the engine's own decoders | `image` crate's `bmp`, `ico`, `tiff` features (Rust); PNG, JPEG, GIF, WebP and AVIF are already decoded in Rust | S |
| Window, input, cursors | `window.rs`, `input.rs`, `cursors.rs`, `placement.rs`, `autoscroll_cursor.rs` | HWND, window messages, raw input | a platform window interface with Wayland and X11 halves; input from the compositor protocol (`wl_seat`, XInput2) | **direct**: `smithay-client-toolkit` (Wayland) and `x11rb` (X11): full control of frames, scale, IME, drag and drop, more code; **winit**: one crate for both, less control (IME and frame timing are coarse, no drag and drop on Wayland yet) | L |
| IME | `window.rs`, `ImeEvent` | IMM32 / TSF | `zwp_text_input_v3` on Wayland, XIM or IBus on X11 | with the toolkit above | M |
| Clipboard, drag and drop | `window.rs`, `drop_target.rs` | OLE clipboard, OLE drop target | `wl_data_device`, X11 selections and XDND | `smithay-clipboard`, `x11rb`; `arboard` for the clipboard only | M |
| Vsync, frame timing | `vsync.rs`, `ticks.rs`, `timer_resolution.rs` | DXGI `WaitForVBlank`, QPC, `timeBeginPeriod` | Wayland frame callbacks and `wp_presentation` feedback; X11 Present; `CLOCK_MONOTONIC` (no timer resolution to raise) | std `Instant`; the toolkit's frame callbacks | S |
| Display scale | `window.rs` | per-monitor DPI | `wp_fractional_scale_v1`, `Xft.dpi` | with the toolkit | S |
| Media playback | `snowlark-media` `win/player.rs`, `mse.rs`, `decode.rs`, `encode.rs`, `probe.rs`, `bytestream.rs` | Media Foundation | FFmpeg for demuxing and software decoding, VA-API for hardware decoding | **FFmpeg** (`ffmpeg-next` or own bindings; loaded from the system so its codecs are the distribution's); **GStreamer** (`gstreamer-rs`: whole pipelines, less control of timing and MSE) | L |
| Audio output | `win/wasapi.rs` | WASAPI | PulseAudio or PipeWire (ALSA beneath) | `cpal` (ALSA, PulseAudio through ALSA, PipeWire through JACK or ALSA); `libpulse-binding`; `pipewire-rs` | M |
| DRM | `win/playready.rs`, `playready_service.rs`, `protected_source.rs`, shell `cdm.rs` | PlayReady | none on Linux without a third-party CDM | none: no DRM on Linux until that is decided | — |
| WebGL | `snowlark-gl` `ffi.rs` | ANGLE on Direct3D 11 (`libEGL.dll`, `libGLESv2.dll`) | ANGLE on Vulkan or desktop GL, or the system's GL ES through EGL | the same ANGLE build for Linux (`.so`), loaded the same way | S |
| IPC: pipes, handles, sections | `snowlark-ipc` `win/pipe.rs`, `io.rs`, `handle.rs`, `section.rs` | named pipes, IOCP, `DuplicateHandle`, file mappings | Unix domain sockets (`socketpair`) with file descriptors passed by `SCM_RIGHTS`, an epoll IO thread, `memfd_create` shared memory sealed against growth | `rustix` or `nix`; `mio` for the IO loop | M |
| Process launch, lifetime | `win/launch.rs`, `processes.rs`, `cpu.rs` | `CreateProcess`, job objects | a fork server started at launch (forks renderers from one prepared image, so an updated binary on disk never mixes versions), `prctl(PR_SET_PDEATHSIG)`, cgroups | `rustix` | M |
| Sandbox | (M6 of `docs/process-isolation.md`: Windows not started either) | restricted token, job, mitigations | user, PID and network namespaces plus a seccomp-bpf syscall filter on every renderer; files and fonts reach the renderer only as descriptors from the browser | `seccompiler` (Firecracker's BPF compiler), `rustix` for namespaces; the font path needs a broker | L |
| Crash dumps | `win/crash_dump.rs` | `MiniDumpWriteDump` | minidumps written by a handler process through `ptrace` | `minidump-writer` and `crash-handler` (Rust, from the Sentry/Mozilla crash tooling) | M |
| Profile folder | `snowlark-shell` `profile.rs` | `%LOCALAPPDATA%` | `$XDG_CONFIG_HOME/snowlark` for settings, `$XDG_DATA_HOME`, `$XDG_CACHE_HOME` for caches | `dirs` or a few lines of std | S |
| Password store key | `snowlark-paint` `os_crypt.rs` | DPAPI | the Secret Service over D-Bus (GNOME Keyring, KWallet); without one, a key in the profile, said so in the UI | `secret-service` or `oo7` (Rust, D-Bus) | S |
| Files written in place | shell `restore.rs`, `profile.rs` (`OpenOptionsExt`) | share modes | `rename` over the old file; `flock` | std | S |
| Open a download | shell `downloads_ui.rs` (`CommandExt`) | `ShellExecute`, Explorer | `xdg-open`, the FileManager1 D-Bus interface to show it in its folder | std `Command`; `zbus` | S |
| PDF viewer | `snowlark-paint` `pdf.rs` | `Windows.Data.Pdf` | PDFium | `pdfium-render` (PDFium as a shared library); `hayro` (Rust, young) | M |
| Updater and signing | shell `update/`, `snowlark-ipc` `win/signing.rs`, `tools/release/` | Authenticode, a zip | distribution packages (deb, rpm) from signed repositories, or Flatpak; the browser does not replace itself | packaging scripts; no in-browser updater | M |
| Native profiler | `snowlark-js` `native_profile.rs` | `SuspendThread`, `RtlVirtualUnwind`, dbghelp | a signal handler copies the stack; DWARF unwinding and ELF symbols afterwards | `framehop` (unwinder), `addr2line`, `object` | M |
| V8 | `dev.ps1`, `tools/v8-pgo/pin.txt` | a pinned `rusty_v8.lib` built with pointer compression | the same V8 version built for Linux with the same arguments | the `v8` crate's source build (`V8_FROM_SOURCE`), published and pinned beside the Windows library; no prebuilt exists for this version | M |
| The binary | `snowlark` `main.rs` | Windows subsystem plus `snowlark-cli.exe` | one ELF; no console subsystem to split | — | S |

## Interfaces each part sits behind

The engine sees a platform part through one Rust interface with a Windows and a Linux implementation chosen at
compile time (a module per platform under one name, not a `dyn` trait on hot paths: text and raster calls are hot).

| Interface | Today | Needed |
|---|---|---|
| Text engine: font lists, shaping, metrics, outlines | `TextEngine` in `snowlark-paint` `text.rs`; the text types and the run cache in `snowlark-graphics` | done: `linux/text.rs` on fontconfig and HarfBuzz |
| Raster: a display list into pixels | `raster.rs` painter over a Direct2D device context | `linux/raster.rs` has `HeadlessRenderer`'s methods and no backend yet: a painter over Skia, `HeadlessRenderer` first (headless modes and reftests) |
| Tiles and the compositor | `gpu.rs` `TileRasterizer`, `compositor.rs` `Compositor` | the same over Skia's GPU backend (GL or Vulkan) |
| Window | `snowlark-paint` `window.rs` `Window`, `Event`, `WindowProxy` | a Wayland half (X11 desktops through XWayland) |
| Process boundary | `snowlark-ipc`'s `Link`, `Section`, `RemoteHandle`, `launch` | done: the `posix/` half under the same names |
| Media player | `snowlark-media` `win/` (`Player`, MSE) | an FFmpeg half |
| ANGLE | `snowlark-gl` `ffi.rs` loads two libraries by name | the `.so` names |
| OS services | `os_crypt.rs`, shell `profile.rs`, `downloads_ui.rs` | a function each behind `cfg` |

## Stages

| Stage | What | Size | Ends with |
|---|---|---|---|
| 1 | The map, the Linux check of the platform-free crates, this plan | S | done: `tools/linux/check.ps1` green for eight crates; six of them built and tested on Linux |
| 2 | **Split `snowlark-paint`**: the platform-free modules (geometry, display list, layer animation, filters, scroll curves, colour profiles, the Rust image decoders, the text types and shape cache, a canvas's operations) into `snowlark-graphics` with its own error type; the Windows half keeps Direct2D, DirectWrite and the window. Linux halves of the text engine, the headless renderer and the GPU canvas with the same methods and nothing behind them | M | done: `snowlark-graphics`, `-paint`, `-layout` and `-render` in `check.ps1`; nothing changes on Windows (WPT gate) |
| 3 | **Headless on Linux**: V8 for Linux; the Itanium declarations in `snowlark-js`; the Linux text engine (fontconfig, FreeType, HarfBuzz); Skia behind `HeadlessRenderer`; WIC formats in Rust. `--eval`, `--render`, `--wpt` and reftests run on Linux | L | WPT on Linux within a known distance of the Windows counts, read per area. Done so far: V8 for Linux and the Itanium declarations (`snowlark-js` and `snowlark-bindings` tested on Linux), the text engine, Skia behind `HeadlessRenderer` (`snowlark-render`'s tests pass on Linux but its GPU canvas ones) |
| 4 | **Processes**: the `posix/` half of `snowlark-ipc` (sockets, descriptor passing, memfd sections, launch, a fork server), crash dumps | M | the renderer process model on Linux; IPC tests green there. Done so far: the `posix/` half of `snowlark-ipc` (no fork server or crash dumps yet), its tests green on Linux |
| 5 | **The window**: Wayland (X11 desktops through XWayland), input and IME, clipboard, drag and drop, cursors, display scale, frame timing; GPU tiles and the compositor on Skia's GPU backend | L | a usable window on a Linux desktop |
| 6 | **WebGL** on ANGLE for Linux | S | the WebGL conformance areas as on Windows |
| 7 | **Media**: playback and MSE on FFmpeg, audio output; hardware decoding later | L | `<video>`, `<audio>`, MSE sites play |
| 8 | **Sandbox**: namespaces and seccomp-bpf for renderers, a font broker | L | a renderer that cannot open a file or a socket |
| 9 | **OS integration and distribution**: profile folders, the password store's key, PDF, opening downloads, packages and their signing | M | a package a user can install |

Stages 2 and 3 need no window; stage 3 builds on the painting backend and the text stack decided below. Stages 4
and 5 can go side by side; 6 follows 5. The sandbox (8) comes
before Linux builds are offered to users, as on Windows.

## Decisions

1. **Painting: Skia on Linux** (through the `skia-safe` bindings). The web's painting model (filters, blend modes,
   paths, GPU raster) is what Skia was built for; Vello is not complete enough for a browser yet, and tiny-skia
   draws on the CPU only. **Windows stays on Direct2D** behind the same interface; moving Windows to Skia is a
   later decision, made with measurements.
2. **Text: fontconfig, FreeType and HarfBuzz** (the C libraries). Font matching, fallback and shaping must match
   what pages expect across scripts and OpenType features, and these are the proven libraries for it; Skia
   already uses FreeType. The text engine itself needs only fontconfig and HarfBuzz (HarfBuzz reads metrics and
   outlines from the font's own tables); FreeType draws the glyphs, under Skia.
3. **Window: Wayland directly** (`smithay-client-toolkit` or an equivalent), not winit: a browser needs IME, the
   clipboard, drag and drop, popups and fractional scaling under its own control. X11 desktops run it through
   XWayland at first; a native X11 half only if measurements show it is needed.
4. **Media: FFmpeg** from the system's libraries (linked dynamically), with VA-API for hardware decoding later.
   Snowlark ships no codecs of its own on Linux.
5. **Test machine: WSL with Ubuntu** for stages 2 to 4 and the first window (WSLg is a Wayland compositor).
   Performance and real-desktop checks need a Linux machine or a virtual machine with its own GPU.
6. **Packaging: a `.tar.gz` preview first** (unpack and run, with the same signed update check as the Windows
   zip), Flatpak after it, distribution packages (deb, rpm) later.
7. **DRM on Linux: none** for now.

Each new large dependency these bring (Skia, FreeType, HarfBuzz, fontconfig, FFmpeg) is added in the stage that
needs it.
