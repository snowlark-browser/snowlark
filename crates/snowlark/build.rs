// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Compiles the application icon into the executables as a Windows resource. `rc.exe` comes with the
//! Visual Studio environment `dev.ps1` sets up; without it the build goes on, with the system's
//! default icon.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=res/snowlark.rc");
    println!("cargo:rerun-if-changed=res/snowlark.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out_dir = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    let res = Path::new(&out_dir).join("snowlark.res");
    // The version resource's numbers, from the package version ("0.1.0").
    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let part = |name: &str| std::env::var(name).expect("cargo sets the version parts");
    let defines = [
        format!("/dVER_MAJOR={}", part("CARGO_PKG_VERSION_MAJOR")),
        format!("/dVER_MINOR={}", part("CARGO_PKG_VERSION_MINOR")),
        format!("/dVER_PATCH={}", part("CARGO_PKG_VERSION_PATCH")),
        format!("/dVER_STRING={version}"),
    ];
    let compiled = Command::new("rc")
        .args(["/nologo", "/i", "res"])
        .args(&defines)
        .arg("/fo")
        .arg(&res)
        .arg("res/snowlark.rc")
        .status();
    match compiled {
        Ok(status) if status.success() => {
            println!("cargo:rustc-link-arg-bins={}", res.display());
        }
        other => println!("cargo:warning=no application icon: rc.exe gave {other:?}"),
    }
}
