// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --render --animation-time MS`: an animated GIF is captured on the frame of that time, with
//! scripts and without (the fixture's frames: red 100 ms, lime 200 ms, blue 300 ms, forever).

use std::path::Path;
use std::process::Command;

use snowlark_paint::Color;

fn capture(dir: &Path, ms: u32, scripts: bool) -> Color {
    let png = dir.join(format!("at-{ms}-{scripts}.png"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_snowlark"));
    cmd.arg("--render")
        .arg(dir.join("page.html"))
        .arg(&png)
        .args(["100", "--height", "50", "--animation-time", &ms.to_string()]);
    if !scripts {
        cmd.arg("--no-scripts");
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let image = snowlark_paint::image::decode_file(&png).unwrap();
    image.frames[0].pixel(8, 8)
}

#[test]
fn captures_the_frame_of_the_animation_time() {
    let dir = std::env::temp_dir().join(format!("snowlark-render-anim-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let gif = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../snowlark-render/tests/fixtures/anim-rgb.gif");
    std::fs::copy(gif, dir.join("anim.gif")).unwrap();
    std::fs::write(
        dir.join("page.html"),
        "<!DOCTYPE html><style>body { margin: 0 }</style><img src=anim.gif>",
    )
    .unwrap();
    for scripts in [false, true] {
        assert_eq!(
            capture(&dir, 50, scripts),
            Color::rgb(255, 0, 0),
            "{scripts}"
        );
        assert_eq!(
            capture(&dir, 150, scripts),
            Color::rgb(0, 255, 0),
            "{scripts}"
        );
        assert_eq!(
            capture(&dir, 950, scripts),
            Color::rgb(0, 0, 255),
            "{scripts}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Web Animations: an `Element.animate()` fade started while the page loads is captured at the
/// animation time after its start, whatever the load took.
#[test]
fn captures_a_script_animation_at_the_animation_time() {
    let dir = std::env::temp_dir().join(format!("snowlark-render-waapi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("page.html"),
        "<!DOCTYPE html><style>body { margin: 0; background: white } \
         div { width: 50px; height: 50px; background: rgb(0, 0, 255) }</style><div></div>\
         <script>document.querySelector('div').animate([{ opacity: 1 }, { opacity: 0 }], \
         { duration: 1000, fill: 'forwards' });</script>",
    )
    .unwrap();
    // Opacity 0.75 over white, then 0.25.
    assert_eq!(capture(&dir, 250, true), Color::rgb(64, 64, 255));
    assert_eq!(capture(&dir, 750, true), Color::rgb(191, 191, 255));
    let _ = std::fs::remove_dir_all(&dir);
}
