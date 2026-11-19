// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! All of this crate's integration tests in one test binary (one link instead of 9; each file
//! below is a module, so a test's name is `<file>::<test>`: `--test it csp::` runs one file's tests).
//! A new file in `tests/` needs one `mod` line here; `every_test_file_is_listed` fails if it is missing.
//! A test that needs a process of its own (process-wide state it changes) gets its own `[[test]]`
//! target in Cargo.toml instead.
//! (`common` is a module of several files below, each with its own copy: its statics are per file, as when
//! each file was its own binary.)
#![allow(clippy::duplicate_mod)]

#[path = "animations.rs"]
mod animations;
#[path = "cascade.rs"]
mod cascade;
#[path = "color_scheme.rs"]
mod color_scheme;
#[path = "damage.rs"]
mod damage;
#[path = "invalidation.rs"]
mod invalidation;
#[path = "perf.rs"]
mod perf;
#[path = "selectors.rs"]
mod selectors;
#[path = "shadow.rs"]
mod shadow;
#[path = "values.rs"]
mod values;
#[path = "values4.rs"]
mod values4;

/// `autotests = false` means a file in `tests/` that is not a module above would never run.
#[test]
fn every_test_file_is_listed() {
    let listed = include_str!("it.rs");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        let Some(stem) = name.strip_suffix(".rs") else {
            continue;
        };
        if stem != "it" && !listed.lines().any(|l| l.trim() == format!("mod {stem};")) {
            missing.push(name);
        }
    }
    assert!(
        missing.is_empty(),
        "add `mod <name>;` to tests/it.rs for: {missing:?}"
    );
}
