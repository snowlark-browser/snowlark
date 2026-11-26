// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! All of this crate's integration tests in one test binary (one file, one module): `child` runs a
//! real second process, `loopback` the same code in one. The process boundary is Windows-only so far
//! (docs/linux.md stage 4).

#[cfg(windows)]
mod child;
#[cfg(windows)]
mod link;
#[cfg(windows)]
mod loopback;

#[test]
fn every_test_file_is_listed() {
    let list = include_str!("it.rs");
    for entry in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/tests")).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if let Some(module) = name.strip_suffix(".rs") {
            let line = format!("mod {module};");
            assert!(module == "it" || list.contains(&line), "{line} is missing");
        }
    }
}
