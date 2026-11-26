# The V8 library Snowlark links

Snowlark embeds V8 through the `v8` crate (rusty_v8). The crate's Rust side is vendored in `vendor/v8`
(`vendor/v8/SNOWLARK-PATCHES.md` says which commit and why); the V8 library itself, a static library of about
150 MB, is not in the repository. This page says where it comes from.

## Why not the crate's prebuilt library

The `v8` crate normally downloads a prebuilt V8 from its releases. Snowlark cannot use those for two reasons:

- The vendored crate is the crate's own roll to V8 15.8 (crate version 158.0.0), which has no release yet, so no
  library of that version is published.
- The workspace enables pointer compression (`v8_enable_pointer_compression` in `Cargo.toml`), and the engine's
  inline field and Smi code assumes it (`docs/v8-upgrade.md`).

So the library is our own build of exactly that V8: the crate's source at the vendored commit, the crate's build
arguments plus pointer compression, compiled with a profile of Snowlark's workload (PGO) and ThinLTO, with its
embedded builtins laid out from a profile of the same workload. `docs/v8-speed.md` has the measurements.

## What `dev.ps1` does

`tools/v8-pgo/pin.txt` names the library: the crate version it is for, a folder name, and SHA-256 hashes. For
every cargo command but `fmt`, while `Cargo.lock`'s `v8` is that version, `dev.ps1` picks, in order:

1. **The library's folder**: `SNOWLARK_V8_DIR`, else `<SNOWLARK_DEV_DIR>\v8\<name>`, else `target\v8\<name>` in
   the checkout. When it holds `rusty_v8.lib` and `src_binding.rs`, `dev.ps1` sets `RUSTY_V8_ARCHIVE`,
   `RUSTY_V8_ARCHIVE_SHA256` (`lib-sha256`, which the `v8` crate's build script checks) and
   `RUSTY_V8_SRC_BINDING_PATH`.
2. **The published library**: when the folder does not hold them, `dev.ps1` first downloads the pin's `url` (a
   gzipped copy attached to a release of this repository, about 32 MB), checks it against `url-sha256`, unpacks
   it there, and downloads `binding-url` beside it. This is what a fresh clone does on its first build; it needs
   no setting. With `SNOWLARK_DEV_DIR` set, every checkout shares the one copy.

A `RUSTY_V8_ARCHIVE` or `V8_FROM_SOURCE` already in the environment is left alone, and `SNOWLARK_V8_PREBUILT=1`
leaves the choice to the `v8` crate entirely (useful only with a crate version and feature set the crate
publishes a library for).

## Building it yourself

Building V8 takes the crate's full checkout with its submodules (it brings its own clang) and, for a plain
build, about 22 minutes with 8 jobs on a fast desktop; the profile-guided library is several builds and
training runs. The scripts are in `tools/v8-pgo/`; their work folder is `<SNOWLARK_DEV_DIR>\v8-pgo`
unless `-Work` says otherwise.

1. `prepare.ps1 -Tag v8-15.8 -Src <dir>` clones the crate's repository with its submodules (check out the commit
   named in `vendor/v8/SNOWLARK-PATCHES.md` if the branch has moved).
2. `build-v8.ps1 -Src <dir> -Name control` builds the library as the crate would: it runs
   `dev.ps1 build -p v8` with `V8_FROM_SOURCE=1` and the workspace's `v8` dependency taken from the checkout, so
   the workspace's features (pointer compression) apply. The result is `<work>\libs\control\` (`rusty_v8.lib`,
   `src_binding.rs`). This plain build is enough to link and run Snowlark: set `SNOWLARK_V8_DIR` to that folder.
3. For the profile-guided library: build an instrumented one (`-Name instrumented -GnArgs 'chrome_pgo_phase=1'`),
   run the workload with `train.ps1` (and `train-builtins.ps1` for the builtins' profile), then build with
   `-GnArgs 'chrome_pgo_phase=2 pgo_data_path="<work>/snowlark.profdata" use_thin_lto=true'` and make the
   ThinLTO objects native with `thinlto-native.py`. `docs/v8-speed.md`, section 6, walks through it.
4. Put `rusty_v8.lib` and the generated `src_binding.rs` in a folder and set `SNOWLARK_V8_DIR` to it. To make it
   the pinned library, set `name` and `lib-sha256` in `pin.txt` (and the `url` lines once it is published).

`vendor/v8` carries only what cargo compiles, not V8's source, so `V8_FROM_SOURCE=1` works only with the `v8`
dependency pointed at a full checkout, which is what `build-v8.ps1` does.

## Linux

The Linux library is the same V8 at the same commit with the same arguments (the crate's, pointer
compression, PartitionAlloc off), built for `x86_64-unknown-linux-gnu`: a plain build, without the profile or
ThinLTO steps of the Windows one. No library of this version is published for Linux, so a Linux machine builds
it once:

1. Clone the crate's repository at the commit `vendor/v8/SNOWLARK-PATCHES.md` names, with its submodules
   (`git submodule update --init --recursive --depth 1`), in the Linux file system.
2. `tools/linux/build-v8.sh <checkout>` builds it with the crate's own build (its clang, gn and ninja are
   downloaded into the build folder) and copies `librusty_v8.a` and `src_binding.rs` to where
   `tools/linux/cargo.sh` looks, printing the archive's SHA-256. It needs a C toolchain, `python3` and
   `pkg-config` (`docs/linux.md`); `use_glib=false` keeps glib, which V8 does not use, out of the build.
3. `tools/v8-pgo/pin.txt` names the Linux library with `linux-name` and `linux-lib-sha256`.

`tools/linux/cargo.sh` is the Linux counterpart of `dev.ps1`'s V8 step: while `Cargo.lock`'s `v8` is the pin's
`crate`, it sets `RUSTY_V8_ARCHIVE`, `RUSTY_V8_ARCHIVE_SHA256` and `RUSTY_V8_SRC_BINDING_PATH` from the folder
`SNOWLARK_V8_DIR`, else `<SNOWLARK_DEV_DIR>/v8/<linux-name>`, else `target/v8/<linux-name>` in the checkout, and
runs cargo with the remaining arguments (`tools/linux/cargo.sh test --profile iter -p snowlark-js`).

The crate builds the Linux library with `v8_monolithic_for_shared_library=true`: V8's thread-locals use the
model a shared object needs, which an executable does not mind.

A new V8 version, or a library built with other arguments, must be re-verified against the engine's direct
uses of V8: `docs/v8-upgrade.md`.
