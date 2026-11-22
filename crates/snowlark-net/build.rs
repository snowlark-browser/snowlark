// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Gives the crate the release of the engine family its V8 comes from, as
//! `SNOWLARK_CHROMIUM_MAJOR`: the major version of the `v8` crate in the workspace's `Cargo.lock`,
//! which is the release line of the V8 it builds (crate 158.x is V8 15.8, of release 158). The
//! default user agent's `Chrome/` token and the compatibility brand's version follow it, so they
//! move with every V8 roll and are written nowhere by hand.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let lock = manifest.join("../../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    println!("cargo:rerun-if-changed=build.rs");
    let text = std::fs::read_to_string(&lock)
        .unwrap_or_else(|e| panic!("reading {}: {e}", lock.display()));
    let major = v8_major(&text)
        .unwrap_or_else(|| panic!("no `v8` package with a version in {}", lock.display()));
    println!("cargo:rustc-env=SNOWLARK_CHROMIUM_MAJOR={major}");
}

/// The major version of the `[[package]]` named `v8` in a `Cargo.lock`.
fn v8_major(lock: &str) -> Option<u32> {
    let mut lines = lock.lines();
    while let Some(line) = lines.next() {
        if line.trim() == "name = \"v8\"" {
            let version = lines.next()?.trim().strip_prefix("version = \"")?;
            return version.split(['.', '"']).next()?.parse().ok();
        }
    }
    None
}
