// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Which C++ ABI the V8 library's mangled names and calling conventions follow on the target
//! (`docs/v8-upgrade.md`): `v8_abi_msvc` (Windows x64, MSVC), `v8_abi_itanium` (Linux x86-64), and
//! `v8_abi` for either. Code that reaches V8 by mangled name is compiled only where one holds.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(v8_abi, v8_abi_msvc, v8_abi_itanium)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let abi = match (os.as_str(), env.as_str(), arch.as_str()) {
        ("windows", "msvc", "x86_64") => Some("v8_abi_msvc"),
        ("linux", "gnu", "x86_64") => Some("v8_abi_itanium"),
        _ => None,
    };
    if let Some(abi) = abi {
        println!("cargo::rustc-cfg=v8_abi");
        println!("cargo::rustc-cfg={abi}");
    }
}
