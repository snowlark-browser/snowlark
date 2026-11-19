// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --reftest`: screenshots of reftest pages, waiting for `reftest-wait` as wptrunner does.

use std::process::Command;

#[test]
fn takes_screenshots_after_reftest_wait() {
    let dir = std::env::temp_dir().join(format!("snowlark-reftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // The test turns its box green once it is told it rendered (TestRendered), then lets the
    // screenshot happen; the reference is green from the start; the third page never lets go.
    std::fs::write(
        dir.join("test.html"),
        "<!DOCTYPE html><html class=reftest-wait><style>div{width:100px;height:100px;background:red}</style>\
         <div id=d></div><script>document.documentElement.addEventListener('TestRendered', () => {\
         d.style.background = 'green';\
         setTimeout(() => document.documentElement.classList.remove('reftest-wait'), 50); });</script>",
    )
    .unwrap();
    std::fs::write(
        dir.join("ref.html"),
        "<!DOCTYPE html><style>div{width:100px;height:100px;background:green}</style><div></div>",
    )
    .unwrap();
    std::fs::write(
        dir.join("stuck.html"),
        "<!DOCTYPE html><html class=reftest-wait><p>waiting</p>",
    )
    .unwrap();
    let url = |name: &str| {
        url::Url::from_file_path(dir.join(name))
            .unwrap()
            .to_string()
    };
    std::fs::write(
        dir.join("list.txt"),
        format!(
            "{} test.png\n{} ref.png size=300x200\n{} stuck.png timeout=2000\n",
            url("test.html"),
            url("ref.html"),
            url("stuck.html")
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_snowlark"))
        .arg("--reftest")
        .arg(dir.join("list.txt"))
        .arg(dir.join("out"))
        .output()
        .unwrap();
    let results = std::fs::read_to_string(dir.join("out/results.jsonl")).unwrap_or_default();
    let test_png = std::fs::read(dir.join("out/test.png")).unwrap_or_default();
    let ref_png = std::fs::read(dir.join("out/ref.png")).unwrap_or_default();
    let stuck_png = std::fs::exists(dir.join("out/stuck.png")).unwrap_or(false);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<&str> = results.lines().collect();
    assert_eq!(lines.len(), 3, "{results}");
    assert!(
        lines[0].contains("\"status\":\"OK\"") && lines[0].contains("\"wait\":\"removed\""),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].contains("\"status\":\"OK\"") && lines[1].contains("\"wait\":null"),
        "{}",
        lines[1]
    );
    assert!(
        lines[2].contains("\"status\":\"TIMEOUT\"") && lines[2].contains("never removed"),
        "{}",
        lines[2]
    );
    // PNG header: width and height (big-endian) at bytes 16..24.
    let size = |png: &[u8]| {
        (
            u32::from_be_bytes(png[16..20].try_into().unwrap()),
            u32::from_be_bytes(png[20..24].try_into().unwrap()),
        )
    };
    assert_eq!(size(&test_png), (800, 600));
    assert_eq!(size(&ref_png), (300, 200));
    // a timed-out page is still captured, to look at
    assert!(stuck_png);
}

#[test]
fn reports_bad_lists() {
    let dir = std::env::temp_dir().join(format!("snowlark-reftest-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("list.txt"), "http://example.test/a.html\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_snowlark"))
        .arg("--reftest")
        .arg(dir.join("list.txt"))
        .arg(dir.join("out"))
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn a_panic_in_a_callback_is_the_pages_crash_and_ends_the_process() {
    let dir = std::env::temp_dir().join(format!("snowlark-reftest-panic-{}", std::process::id()));
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
    std::fs::write(
        dir.join("list.txt"),
        format!(
            "{} first.png
{} second.png
",
            url("first.html"),
            url("second.html")
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_snowlark"))
        .arg("--reftest")
        .arg(dir.join("list.txt"))
        .arg(dir.join("out"))
        .env("SNOWLARK_TEST_PANIC", "callback")
        .env("SNOWLARK_PROFILE_DIR", dir.join("profile"))
        .output()
        .unwrap();
    let results = std::fs::read_to_string(dir.join("out/results.jsonl")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(3), "{results}");
    assert_eq!(results.lines().count(), 1, "{results}");
    assert!(
        results.contains("CRASH: SNOWLARK_TEST_PANIC=callback"),
        "{results}"
    );
}
