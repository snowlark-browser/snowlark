# Snowlark on Linux

Snowlark runs on Windows only. This document maps every part of it that is tied to Windows, names the Linux
counterpart and the Rust options for each, and lays out the order of the port in stages. The engine crates stay
free of platform code; each platform part sits behind one interface with a Windows and a Linux half.

## Where it stands

**These crates check for Linux** (`cargo check --target x86_64-unknown-linux-gnu`, tests included), from
Windows: `snowlark-dom`, `snowlark-idl`, `snowlark-audio`, `snowlark-idb`, `snowlark-style`, `snowlark-net`,
`snowlark-js` and `snowlark-bindings`. Keep them so:

```powershell
.\tools\linux\check.ps1
```

It needs the target once (`rustup target add x86_64-unknown-linux-gnu`). Build scripts compile C for the target
(`ring`, `zstd`, `mimalloc`) and Windows has no compiler for Linux; `cargo check` never links, so
`tools/linux/stub_cc.py` stands in for the compiler and archiver and writes empty outputs.

**Built and tested on Linux:** `snowlark-dom`, `snowlark-idl`, `snowlark-audio`, `snowlark-idb`, `snowlark-style`
and `snowlark-net` build natively and pass their tests there (11 test binaries, 588 tests, none failing).

### A Linux machine for building

Any x86-64 Linux with a recent distribution works; WSL 2 with Ubuntu on Windows is enough for the engine crates
and the headless modes (its window and GPU are not representative of a Linux desktop). Keep the checkout and its
`target` on the Linux file system: a checkout under `/mnt/<drive>` (the Windows drives) builds several times
slower.

```sh
apt-get install -y build-essential clang lld pkg-config curl ca-certificates git python3 cmake ninja-build \
    libfontconfig-dev libfreetype-dev
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal \
    --default-toolchain stable -c rustfmt -c clippy
git clone <the repository> snowlark && cd snowlark
cargo test --profile iter --no-fail-fast -p snowlark-dom -p snowlark-idl -p snowlark-audio -p snowlark-idb \
    -p snowlark-style -p snowlark-net
```

The fontconfig and FreeType headers are for the text engine of stage 3. Add `-j <n>` on a machine other work
shares. `dev.ps1` is Windows-only; on Linux cargo runs directly.

`snowlark-js`, `snowlark-bindings` and every crate above them need a V8 library to link, and none is published
for Linux at the V8 version the workspace pins (`vendor/v8`, with pointer compression): building it from source
for Linux, as the Windows library is built (`docs/v8-build.md`), is the first step of stage 3.

`snowlark-js` has three Windows-only pieces, each with a fallback elsewhere:

- The native sampling profiler (`native_profile.rs`, `SNOWLARK_PROFILE=1`) suspends the thread and unwinds with the
  Windows unwind tables. Elsewhere `NativeSampler::start` returns `None`; the V8 profile and the binding counts
  still work. The Linux design: the sampler sends a signal to the profiled thread, whose handler copies its
  registers and stack into a preallocated buffer (only async-signal-safe code there), and the copy is unwound with
  the DWARF call frame information afterwards; names come from the ELF symbols.
- Four V8 entry points the `v8` crate does not expose are declared by their MSVC-mangled names: the access check
  with interceptors (`access_check.rs`), the accessor cache (`cached_accessor.rs`), consuming a code cache on a
  worker (`code_cache.rs`) and streamed compilation (`streaming.rs`). Elsewhere the code falls back: code caches
  are consumed on the page thread, scripts compile without streaming, and **a cross-origin access to a `Window`
  is refused outright**, with none of HTML's cross-origin properties (closed, not open: safe, but pages that
  `postMessage` to another origin's window break). Linux needs the same declarations under their Itanium names
  (`_ZN2v8...`), checked against the same V8 version (`docs/v8-upgrade.md`).
- The C runtime's heap on mimalloc (`crt_heap.rs`) is only for the static MSVC runtime; on Linux the allocator is
  set in the binary as on Windows.

**What stops the rest: `snowlark-paint`.** `snowlark-layout` and `snowlark-render` depend on it for platform-free
things (geometry, the display list, layer animations, filter geometry, the text types) and for one platform
thing: the `TextEngine`, which is DirectWrite. Every module of `snowlark-paint` sits in one crate whose
dependencies include `windows` unconditionally, and many platform-free modules use `windows::core::Result` as their
error type (the Rust image decoders in `decoders.rs`, for example). Every crate above layout (`snowlark-render`,
`-page`, `-tab`, `-media`, `-shell`, `-blocker`, the binary) waits on stage 2 below.

`snowlark-gl` (ANGLE), `snowlark-ipc` (pipes, sections, process launch), `snowlark-media` (Media Foundation,
PlayReady, WASAPI) and the window half of `snowlark-paint` are platform halves by nature: each gets a Linux half.

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
| Text engine: font lists, shaping, metrics, outlines | `TextEngine` struct in `snowlark-paint` `text.rs` (its shaped-text types above it are platform-free) | the same methods on a Linux `TextEngine`; the types and the shape cache moved where layout reaches them without the rest of paint |
| Raster: a display list into pixels | `raster.rs` painter over a Direct2D device context | a painter over the chosen 2D library; `HeadlessRenderer` first (headless modes and reftests) |
| Tiles and the compositor | `gpu.rs` `TileRasterizer`, `compositor.rs` `Compositor` | the same over GL or Vulkan |
| Window | `snowlark-paint` `window.rs` `Window`, `Event`, `WindowProxy` | a Wayland half and an X11 half |
| Process boundary | `snowlark-ipc`'s `Link`, `Section`, `RemoteHandle`, `launch` (with `win/` as the only half) | a `posix/` half under the same names |
| Media player | `snowlark-media` `win/` (`Player`, MSE) | an FFmpeg (or GStreamer) half |
| ANGLE | `snowlark-gl` `ffi.rs` loads two libraries by name | the `.so` names |
| OS services | `os_crypt.rs`, shell `profile.rs`, `downloads_ui.rs` | a function each behind `cfg` |

## Stages

| Stage | What | Size | Ends with |
|---|---|---|---|
| 1 | The map, the Linux check of the platform-free crates, this plan | S | done: `tools/linux/check.ps1` green for eight crates; six of them built and tested on Linux |
| 2 | **Split `snowlark-paint`**: the platform-free modules (geometry, display list, layer animation, filters, scroll curves, colour profiles, the Rust image decoders, the text types and shape cache) into a crate of their own with their own error type; the Windows half keeps Direct2D, DirectWrite and the window. A Linux `TextEngine` stub with the same methods (no fonts) so layout checks | M | `snowlark-layout` and `snowlark-render` in `check.ps1`; nothing changes on Windows (WPT gate) |
| 3 | **Headless on Linux**: V8 for Linux; the Itanium declarations in `snowlark-js`; the Linux text engine (fontconfig, shaping, outlines); a CPU raster backend for `HeadlessRenderer`; WIC formats in Rust. `--eval`, `--render`, `--wpt` and reftests run on Linux | L | WPT on Linux within a known distance of the Windows counts, read per area |
| 4 | **Processes**: the `posix/` half of `snowlark-ipc` (sockets, descriptor passing, memfd sections, launch, a fork server), crash dumps | M | the renderer process model on Linux; IPC tests green there |
| 5 | **The window**: Wayland first (X11 after, or through XWayland), input and IME, clipboard, drag and drop, cursors, display scale, frame timing; GPU tiles and the compositor on GL or Vulkan | L | a usable window on a Linux desktop |
| 6 | **WebGL** on ANGLE for Linux | S | the WebGL conformance areas as on Windows |
| 7 | **Media**: playback and MSE on FFmpeg (or GStreamer), audio output; hardware decoding later | L | `<video>`, `<audio>`, MSE sites play |
| 8 | **Sandbox**: namespaces and seccomp-bpf for renderers, a font broker | L | a renderer that cannot open a file or a socket |
| 9 | **OS integration and distribution**: profile folders, the password store's key, PDF, opening downloads, packages and their signing | M | a package a user can install |

Stages 2 and 3 need no window and no decision on the window toolkit; stage 3 needs the painting backend and the
text stack decided, and a Linux machine. Stages 4 and 5 can go side by side; 6 follows 5. The sandbox (8) comes
before Linux builds are offered to users, as on Windows.

## Decisions still open

1. **The painting backend** (before stage 3): Skia, Vello or tiny-skia, and whether Windows moves to it too (one
   backend for both, or Direct2D kept on Windows). tiny-skia is enough for headless runs and costs nothing new;
   the window needs a GPU backend.
2. **The text stack** (before stage 3): fontconfig with Rust shaping and outlines (harfrust or rustybuzz, skrifa),
   or the C FreeType and HarfBuzz.
3. **The window toolkit** (before stage 5): Wayland and X11 directly, or winit.
4. **The media stack** (before stage 7): FFmpeg or GStreamer, and which codecs ship.
5. **A Linux desktop machine** (before stage 5): WSL with Ubuntu serves stages 2 to 4 (headless, WPT,
   processes); the window, the GPU and vsync need a real Linux box or a virtual machine with its own GPU.
6. **Distribution** (stage 9): deb and rpm repositories, Flatpak, AppImage; how updates reach users.
7. **DRM on Linux**: none unless a CDM is chosen.

Every new large dependency (Skia, FFmpeg, fontconfig, a toolkit) waits for its decision.
