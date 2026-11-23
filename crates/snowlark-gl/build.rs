// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Puts ANGLE's libraries beside the built executables: from the folder `SNOWLARK_ANGLE_DIR`
//! names (set by `dev.ps1` from `tools/angle/pin.txt`, see `docs/webgl.md`), into the profile's
//! output folder and its `deps` (where test binaries run). Without the setting nothing is copied
//! and the build has no WebGL.

use std::path::PathBuf;

const FILES: [&str; 2] = ["libEGL.dll", "libGLESv2.dll"];

fn main() {
    println!("cargo:rerun-if-env-changed=SNOWLARK_ANGLE_DIR");
    let Some(dir) = std::env::var_os("SNOWLARK_ANGLE_DIR").map(PathBuf::from) else {
        return;
    };
    for f in FILES {
        println!("cargo:rerun-if-changed={}", dir.join(f).display());
    }
    if !FILES.iter().all(|f| dir.join(f).is_file()) {
        println!(
            "cargo:warning=SNOWLARK_ANGLE_DIR ({}) does not hold libEGL.dll and libGLESv2.dll: no WebGL",
            dir.display()
        );
        return;
    }
    // OUT_DIR is <target>/<profile>/build/<crate>-<hash>/out.
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let Some(profile) = out.ancestors().nth(3) else {
        return;
    };
    for to in [profile.to_path_buf(), profile.join("deps")] {
        if std::fs::create_dir_all(&to).is_err() {
            continue;
        }
        for f in FILES {
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
