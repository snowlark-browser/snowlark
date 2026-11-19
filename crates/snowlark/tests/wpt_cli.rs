// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --wpt`: a panic in a V8 callback is that test's crash, not a silent end of the process's
//! script for every test after it; `--font` installs a local font, so a page naming WPT's Ahem gets it.

use std::process::Command;

#[test]
fn a_panic_in_a_callback_is_the_tests_crash_and_ends_the_process() {
    let dir = std::env::temp_dir().join(format!("snowlark-wpt-panic-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let page = "<!DOCTYPE html><title>t</title><script>document.title</script>";
    std::fs::write(dir.join("first.html"), page).unwrap();
    std::fs::write(dir.join("second.html"), page).unwrap();
    let url = |name: &str| {
        url::Url::from_file_path(dir.join(name))
            .unwrap()
            .to_string()
    };
    let list = dir.join("list.txt");
    std::fs::write(
        &list,
        format!(
            "{}
{}
",
            url("first.html"),
            url("second.html")
        ),
    )
    .unwrap();
    let results = dir.join("results.jsonl");
    let out = Command::new(env!("CARGO_BIN_EXE_snowlark"))
        .arg("--wpt")
        .arg(&list)
        .arg(&results)
        .env("SNOWLARK_TEST_PANIC", "callback")
        .env("SNOWLARK_PROFILE_DIR", dir.join("profile"))
        .output()
        .unwrap();
    let text = std::fs::read_to_string(&results).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(3), "{text}");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "the second test is left to a new process: {text}"
    );
    assert!(lines[0].contains("first.html"), "{text}");
    assert!(
        lines[0].contains("CRASH: SNOWLARK_TEST_PANIC=callback"),
        "{text}"
    );
}

/// The width a page measures for four Ahem "X" at 20px (80 with the font, whatever the fallback gives
/// without), reported through the result global the runner reads.
fn ahem_width(font: Option<&std::path::Path>, tag: &str) -> String {
    let dir = std::env::temp_dir().join(format!("snowlark-wpt-ahem-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let page = "<!DOCTYPE html><title>t</title><span id=s style=\"font:20px Ahem\">XXXX</span>        <script>var w = document.getElementById('s').getBoundingClientRect().width;        globalThis.__snowlarkWpt = JSON.stringify({status: 0, message: null,        tests: [{name: 'width', status: 0, message: String(w)}]});</script>";
    std::fs::write(dir.join("ahem.html"), page).unwrap();
    let list = dir.join("list.txt");
    let url = url::Url::from_file_path(dir.join("ahem.html")).unwrap();
    std::fs::write(
        &list,
        format!(
            "{url}
"
        ),
    )
    .unwrap();
    let results = dir.join("results.jsonl");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_snowlark"));
    cmd.arg("--wpt").arg(&list).arg(&results);
    if let Some(font) = font {
        cmd.arg("--font").arg(font);
    }
    let out = cmd
        .env("SNOWLARK_PROFILE_DIR", dir.join("profile"))
        .output()
        .unwrap();
    let text = std::fs::read_to_string(&results).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(0), "{text}");
    text
}

#[test]
fn a_font_argument_installs_ahem_for_the_page() {
    // The copy of WPT's Ahem that the layout tests use.
    let ahem = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../snowlark-layout/tests/fixtures/Ahem.ttf");
    assert!(ahem.exists(), "no {}", ahem.display());
    let with = ahem_width(Some(&ahem), "with");
    assert!(
        with.contains("\"message\":\"80\""),
        "Ahem is 1em per glyph: {with}"
    );
    let without = ahem_width(None, "without");
    assert!(
        !without.contains("\"message\":\"80\""),
        "no Ahem without --font: {without}"
    );
}
