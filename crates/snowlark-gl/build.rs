// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Puts ANGLE's libraries beside the built executables: from the folder `SNOWLARK_ANGLE_DIR`
//! names (set by `dev.ps1`, or `tools/linux/cargo.sh` on Linux, from `tools/angle/pin.txt`, see
//! `docs/webgl.md`), into the profile's output folder and its `deps` (where test binaries run).
//! Without the setting nothing is copied and the build has no WebGL.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=SNOWLARK_ANGLE_DIR");
    let Some(dir) = std::env::var_os("SNOWLARK_ANGLE_DIR").map(PathBuf::from) else {
        return;
    };
    let windows = std::env::var("CARGO_CFG_WINDOWS").is_ok();
    let files = if windows {
        ["libEGL.dll", "libGLESv2.dll"]
    } else {
        ["libEGL.so", "libGLESv2.so"]
    };
    for f in files {
        println!("cargo:rerun-if-changed={}", dir.join(f).display());
    }
    if !files.iter().all(|f| dir.join(f).is_file()) {
        println!(
            "cargo:warning=SNOWLARK_ANGLE_DIR ({}) does not hold {} and {}: no WebGL",
            dir.display(),
            files[0],
            files[1]
        );
        return;
    }
    // OUT_DIR is <target>/<profile>/build/<crate>-<hash>/out (<crate>/<hash>/out in newer cargo).
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let Some(profile) = out
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "build"))
        .and_then(|p| p.parent())
    else {
        return;
    };
    for to in [profile.to_path_buf(), profile.join("deps")] {
        if std::fs::create_dir_all(&to).is_err() {
            continue;
        }
        for f in files {
            let (src, dst) = (dir.join(f), to.join(f));
            let same = match (std::fs::metadata(&src), std::fs::metadata(&dst)) {
                (Ok(a), Ok(b)) => a.len() == b.len() && a.modified().ok() <= b.modified().ok(),
                _ => false,
            };
            // (a running browser holds the old copy open: keep it rather than fail the build)
            if !same && std::fs::copy(&src, &dst).is_err() {
                println!("cargo:warning=could not copy {f} to {}", to.display());
            }
        }
    }
}
