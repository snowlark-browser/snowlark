# v8 158.0.0 (V8 15.8.62.23), the Rust side only

The `v8` crate on crates.io stops at 152.2.0 (V8 15.2). This is the crate's own roll to V8 15.8, from its
repository's `v8-15.8` branch, commit `d2bc4ef3797288a9686df750671f7ff01e330587` ("Rolling to V8 15.8.62.23",
github.com/denoland/rusty_v8 pull request 2098, also reachable as `refs/pull/2098/head`), used through
`[patch.crates-io]` in the workspace `Cargo.toml`. Only what cargo compiles is here: `Cargo.toml` (cut after
`[build-dependencies]`: no profile, dev-dependencies, examples, tests or benches), `build.rs`, `src/` and
`LICENSE`, unmodified. No Snowlark patches yet.

The V8 library itself is not here: `build.rs` is given one (`RUSTY_V8_ARCHIVE`, `RUSTY_V8_SRC_BINDING_PATH`,
set by `dev.ps1` from `tools/v8-pgo/pin.txt`) and then needs no V8 source. No prebuilt library of this version
is published, so a machine without the pinned one builds it from the same commit with `tools/v8-pgo/`
(`docs/v8-speed.md`, section 6): `prepare.ps1 -Tag v8-15.8 -Src <dir>` clones the repository with its
submodules (check out the commit above if the branch has moved), `build-v8.ps1 -Src <dir>` builds it.

When a 158.x release reaches crates.io, this directory and its `[patch.crates-io]` line go, as for 152.2.
