// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --render` captures a page whose script never yields: the task is
//! terminated after `SNOWLARK_RENDER_TIMEOUT_MS`, the page is captured as it is, and `SNOWLARK_EVAL`
//! still runs.

use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn a_task_that_never_yields_is_terminated_and_the_page_captured() {
    let dir = std::env::temp_dir().join(format!("snowlark-render-watchdog-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("page.html"),
        "<!DOCTYPE html><style>body { margin: 0; background: rgb(0, 128, 0) }</style>\
         <script>addEventListener('load', () => setTimeout(() => { for (;;) {} }, 0))</script>",
    )
    .unwrap();
    let png = dir.join("out.png");
    let started = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_snowlark"))
        .arg("--render")
        .arg(dir.join("page.html"))
        .arg(&png)
        .args(["100", "--height", "50"])
        .env("SNOWLARK_RENDER_TIMEOUT_MS", "1000")
        .env("SNOWLARK_EVAL", "document.readyState")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(started.elapsed() < Duration::from_secs(20));
    assert!(stderr.contains("terminated"), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "complete");
    let image = snowlark_paint::image::decode_file(&png).unwrap();
    let c = image.frames[0].pixel(8, 8);
    assert_eq!((c.r, c.g, c.b), (0, 128, 0));
}
