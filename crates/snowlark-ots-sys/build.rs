// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Compiles OTS (`vendor/ots`), the WOFF 2.0 decoder it calls (`vendor/woff2`) and the C interface
//! over them (`shim/ots_shim.cc`) into one static library. OTS's two decompressors, zlib for WOFF 1.0
//! and Brotli for WOFF 2.0, are not compiled: `shim/zlib.h` and `shim/brotli/decode.h` route their
//! one call each to functions the caller passes in (the Rust `flate2` and `brotli-decompressor`).

use std::path::{Path, PathBuf};

fn sources(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "cc"))
        .collect();
    files.sort();
    files
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest.join("../../vendor");
    let ots = vendor.join("ots");
    let woff2 = vendor.join("woff2");
    let shim = manifest.join("shim");

    // OTS's table parsers, without the Graphite ones (`OTS_GRAPHITE` stays undefined), as the
    // library is built for web fonts elsewhere.
    let mut files = sources(&ots.join("src"));
    // The WOFF 2.0 decoder and the parts of the library it uses (not the encoder or the tools).
    for f in [
        "table_tags.cc",
        "variable_length.cc",
        "woff2_common.cc",
        "woff2_dec.cc",
        "woff2_out.cc",
    ] {
        files.push(woff2.join("src").join(f));
    }
    files.push(shim.join("ots_shim.cc"));

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .files(&files)
        .include(&shim)
        .include(ots.join("include"))
        .include(ots.join("src"))
        .include(woff2.join("include"))
        .define("NDEBUG", None)
        .warnings(false);
    if build.get_compiler().is_like_msvc() {
        build.flag("/EHsc").flag("/utf-8");
    }
    build.compile("snowlark_ots");

    println!("cargo:rerun-if-changed={}", shim.display());
    println!("cargo:rerun-if-changed={}", ots.display());
    println!("cargo:rerun-if-changed={}", woff2.display());
}
