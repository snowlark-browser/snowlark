// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --computed`: prints computed values for the elements a selector matches.

use std::process::Command;

#[test]
fn prints_computed_values_of_matching_elements() {
    let dir = std::env::temp_dir().join(format!("snowlark-computed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("linked.css"), "p { font-size: 20px; }").unwrap();
    std::fs::write(
        dir.join("page.html"),
        "<!DOCTYPE html><link rel=stylesheet href=linked.css>\
         <style>.x { color: rgb(1, 2, 3); width: 10vw; }</style><p class=x id=a>a</p><p class=x>b</p>",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_snowlark"))
        .args(["--computed"])
        .arg(dir.join("page.html"))
        .args([".x", "color", "font-size", "width", "--viewport", "500x300"])
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap().replace("\r\n", "\n");
    assert_eq!(
        text,
        "p#a.x\n  color: rgb(1, 2, 3)\n  font-size: 20px\n  width: 50px\n\
         p.x\n  color: rgb(1, 2, 3)\n  font-size: 20px\n  width: 50px\n"
    );
}

#[test]
fn reports_bad_arguments() {
    let out = Command::new(env!("CARGO_BIN_EXE_snowlark"))
        .args(["--computed"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}
