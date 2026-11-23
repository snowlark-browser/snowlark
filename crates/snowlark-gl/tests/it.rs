// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The crate's integration tests: one binary, each file a module.

mod service;

#[test]
fn every_test_file_is_listed() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let listed = include_str!("it.rs");
    for e in std::fs::read_dir(dir).unwrap() {
        let name = e.unwrap().file_name().into_string().unwrap();
        if let Some(m) = name.strip_suffix(".rs") {
            if m != "it" {
                assert!(
                    listed.contains(&format!("mod {m};")),
                    "tests/{name} is not in it.rs"
                );
            }
        }
    }
}
