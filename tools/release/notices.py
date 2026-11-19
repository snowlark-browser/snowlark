#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Writes THIRD-PARTY-NOTICES.txt for a release of the browser (shipped as snowlark.exe).

    python tools/release/notices.py <out.txt> [--v8-src <dir>]

What it covers, and where each part comes from:

- Every Rust crate linked into snowlark.exe: `cargo metadata` for the Windows target, walked from the
  `snowlark` package along normal dependency edges only, not into procedural macros (the same walk as
  the credits page, tools/credits/generate.py). Each crate's licence texts are the licence files in
  its source (the crate cache, or vendor/). A crate whose package has no licence file gets the
  standard text of its licence, with the authors from its manifest as the copyright holders, and
  says so.
- The Rust standard library (from the toolchain's share/doc/rust/licenses).
- V8 and the third-party code its build links into the library we link (`--v8-src`: the rusty_v8
  checkout at the commit vendor/v8 is taken from; tools/v8-pgo builds the pinned library from it).
  V8_PARTS lists them: the libraries in the static library's link list (its `rusty_v8.ninja`) and
  the headers V8's own sources include from third_party/.

The registry source must be downloaded (the crate cache dev.ps1 builds with: tools/devenv.py cargo_home).
The script fails when a licence file it expects is missing, rather than writing an incomplete file.
"""
import argparse
import importlib.util
import subprocess
import sys
from pathlib import Path
import os as _os, sys as _sys  # noqa: E401
_sys.path.insert(0, _os.path.join(_os.path.dirname(_os.path.abspath(__file__)), ".."))
import devenv  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
_spec = importlib.util.spec_from_file_location("credits", ROOT / "tools" / "credits" / "generate.py")
credits = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(credits)

# The commit of the rusty_v8 repository vendor/v8 is taken from (vendor/v8/SNOWLARK-PATCHES.md).
V8_COMMIT = "d2bc4ef3797288a9686df750671f7ff01e330587"
DEFAULT_V8_SRC = devenv.dev_dir() / "v8" / "rusty-v8-roll"

# Where the source of our modified MPL-2.0 code is published.
STYLO_SOURCE = "https://github.com/snowlark-browser/third-party/tree/main/stylo"

# (name, licence, home, files relative to the rusty_v8 checkout)
V8_PARTS = [
    ("V8", "BSD-3-Clause", "https://v8.dev/",
     ["v8/LICENSE", "v8/LICENSE.v8", "v8/LICENSE.fdlibm", "v8/LICENSE.strongtalk"]),
    ("V8 builtins and code generation helpers (v8/third_party/v8)", "BSD-3-Clause",
     "https://chromium.googlesource.com/v8/v8/",
     ["v8/third_party/v8/builtins/LICENSE", "v8/third_party/v8/codegen/LICENSE"]),
    ("ICU", "Unicode-3.0 and others (see text)", "https://icu.unicode.org/",
     ["third_party/icu/LICENSE"]),
    ("Abseil", "Apache-2.0", "https://abseil.io/", ["third_party/abseil-cpp/LICENSE"]),
    ("Highway", "Apache-2.0 OR BSD-3-Clause", "https://github.com/google/highway",
     ["third_party/highway/src/LICENSE"]),
    ("simdutf", "Apache-2.0 OR MIT", "https://github.com/simdutf/simdutf",
     ["third_party/simdutf/LICENSE"]),
    ("libc++", "Apache-2.0 WITH LLVM-exception", "https://libcxx.llvm.org/",
     ["third_party/libc++/src/LICENSE.TXT"]),
    ("LLVM libc (shared headers)", "Apache-2.0 WITH LLVM-exception", "https://libc.llvm.org/",
     ["third_party/llvm-libc/src/LICENSE.TXT"]),
    ("Dragonbox", "Apache-2.0 WITH LLVM-exception OR BSL-1.0", "https://github.com/jk-jeon/dragonbox",
     ["third_party/dragonbox/src/LICENSE-Apache2-LLVM", "third_party/dragonbox/src/LICENSE-Boost"]),
    ("fast_float", "Apache-2.0 OR MIT OR BSL-1.0", "https://github.com/fastfloat/fast_float",
     ["third_party/fast_float/src/LICENSE-APACHE", "third_party/fast_float/src/LICENSE-MIT",
      "third_party/fast_float/src/LICENSE-BOOST"]),
    ("FP16", "MIT", "https://github.com/Maratyszcza/FP16", ["third_party/fp16/src/LICENSE"]),
    ("Chromium inspector protocol", "BSD-3-Clause",
     "https://chromium.googlesource.com/deps/inspector_protocol/",
     ["v8/third_party/inspector_protocol/LICENSE"]),
    ("rapidhash", "BSD-2-Clause", "https://github.com/Nicoshev/rapidhash",
     ["v8/third_party/rapidhash-v8/LICENSE"]),
    ("SipHash", "CC0-1.0", "https://github.com/veorq/SipHash", ["v8/third_party/siphash/LICENSE"]),
    ("UTF-8 decoder (Bjoern Hoehrmann)", "MIT", "https://bjoern.hoehrmann.de/utf-8/decoder/dfa/",
     ["v8/third_party/utf8-decoder/LICENSE"]),
    ("Valgrind header", "BSD-style (see text)", "https://valgrind.org/",
     ["v8/third_party/valgrind/LICENSE"]),
    ("fadec", "BSD-3-Clause", "https://github.com/aengelke/fadec", ["v8/third_party/fadec/LICENSE"]),
]

# The Rust crates V8 links for Intl and Temporal (ICU4X, temporal_rs and their dependencies), by
# their folder names in the rusty_v8 checkout's third_party/rust (from rusty_v8.ninja's link list).
V8_RUST_CRATES = [
    "calendrical_calculations", "core_maths", "diplomat_runtime", "either", "fixed_decimal",
    "icu_calendar", "icu_calendar_data", "icu_capi", "icu_collections", "icu_decimal",
    "icu_decimal_data", "icu_locale", "icu_locale_core", "icu_locale_data", "icu_locale_fallback",
    "icu_locale_fallback_data", "icu_normalizer", "icu_normalizer_data", "icu_pattern",
    "icu_plurals", "icu_plurals_data", "icu_provider", "icu_provider_adapters", "ixdtf", "libm",
    "litemap", "num_traits", "potential_utf", "resb", "ryu", "serde", "serde_core", "smallvec",
    "stable_deref_trait", "temporal_capi", "temporal_rs", "timezone_provider", "tinystr",
    "utf16_iter", "utf8_iter", "write16", "writeable", "yoke", "zerofrom", "zerotrie", "zerovec",
    "zoneinfo64",
]

MIT_TEXT = """Permission is hereby granted, free of charge, to any person obtaining a copy of this software and
associated documentation files (the "Software"), to deal in the Software without restriction,
including without limitation the rights to use, copy, modify, merge, publish, distribute,
sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or
substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT
NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT
OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
"""

BSD3_TEXT = """Redistribution and use in source and binary forms, with or without modification, are permitted
provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this list of conditions
   and the following disclaimer.
2. Redistributions in binary form must reproduce the above copyright notice, this list of
   conditions and the following disclaimer in the documentation and/or other materials provided
   with the distribution.
3. Neither the name of the copyright holder nor the names of its contributors may be used to
   endorse or promote products derived from this software without specific prior written
   permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR
IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND
FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER
IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT
OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
"""

RULE = "=" * 100
THIN = "-" * 100


def norm_license(expr):
    """`MIT/Apache-2.0` and `Apache-2.0 OR MIT` are one group."""
    if not expr:
        return "unspecified"
    e = expr.replace("/", " OR ").replace(" / ", " OR ")
    if "AND" in e or "(" in e or "WITH" in e:
        return " ".join(e.split())
    parts = sorted({p.strip() for p in e.split(" OR ") if p.strip()})
    return " OR ".join(parts)


def standard_text(spdx, holders, by_spdx):
    """The standard text of a licence for a crate whose package has no licence file."""
    if spdx == "MIT":
        return f"Copyright (c) the {holders}\n\n{MIT_TEXT}"
    if spdx.startswith("BSD-3-Clause"):
        return f"Copyright (c) the {holders}\n\n{BSD3_TEXT}"
    if spdx in by_spdx:
        return by_spdx[spdx]
    sys.exit(f"no standard text for {spdx}")


def file_text(path):
    if not path.is_file():
        sys.exit(f"missing licence file: {path}")
    return credits.read_text(path)


def crate_texts(md):
    """[(group, [(text, [crate lines])])] for the Rust crates linked into snowlark.exe, and the MPL
    crates with where their source is."""
    pkgs = []
    for pkg in credits.linked_packages(md):
        vendored = "vendor" in Path(pkg["manifest_path"]).parts
        if pkg["source"] is None and not vendored:
            continue  # the browser's own crates
        pkgs.append(pkg)
    # Full standard texts found in crates' own files, for crates that have none.
    by_spdx = {}
    for pkg in pkgs:
        for f in credits.license_files(pkg):
            n = f.name.upper()
            t = credits.read_text(f)
            if "MPL" in (pkg.get("license") or "") and "Mozilla Public License Version 2.0" in t:
                by_spdx.setdefault("MPL-2.0", t)
            if n in ("LICENSE-APACHE", "LICENSE-APACHE.TXT", "LICENSE-APACHE.MD") and "Apache License" in t:
                by_spdx.setdefault("Apache-2.0", t)
    groups = {}
    mpl = []
    for pkg in pkgs:
        name, version = pkg["name"], pkg["version"]
        license = pkg.get("license") or ""
        group = norm_license(license)
        vendored = pkg["source"] is None
        home = pkg.get("repository") or pkg.get("homepage") or f"https://crates.io/crates/{name}"
        line = f"{name} {version}  ({home})"
        if vendored:
            line += "  [modified copy]"
        if "MPL" in license:
            where = (STYLO_SOURCE + " (modified; full source and our list of changes)"
                     if vendored else f"https://crates.io/crates/{name}/{version} (unmodified)")
            mpl.append(f"{name} {version}: {where}")
        files = credits.license_files(pkg)
        texts = [credits.read_text(f) for f in files]
        if not texts:
            authors = ", ".join(a.split(" <")[0] for a in pkg.get("authors") or []) or f"{name} authors"
            holders = f"{name} authors ({authors})" if pkg.get("authors") else f"{name} authors"
            first = group.split(" OR ")[0]
            texts = [standard_text(first, holders, by_spdx)]
            line += "  [its package has no licence file: the standard text of its licence follows]"
        g = groups.setdefault(group, {})
        for t in texts:
            g.setdefault(t, []).append(line)
    out = []
    for group in sorted(groups, key=lambda k: (-sum(len(v) for v in groups[k].values()), k)):
        entries = sorted(groups[group].items(), key=lambda kv: kv[1][0].lower())
        out.append((group, entries))
    return out, sorted(mpl), len(pkgs)


def write(out_path, v8_src):
    md = credits.metadata()
    groups, mpl, ncrates = crate_texts(md)
    head = subprocess.run(["git", "-C", str(v8_src), "rev-parse", "HEAD"], capture_output=True,
                          text=True).stdout.strip()
    if head != V8_COMMIT:
        sys.exit(f"{v8_src} is at {head or 'no commit'}, not {V8_COMMIT} (vendor/v8's commit)")
    lines = []
    w = lines.append
    w("Snowlark: third-party software notices")
    w("")
    w("Snowlark contains the open source software listed below. Each part is used under the licence")
    w("shown, and the full text of each licence follows its list of components.")
    w("")
    w("Contents")
    w("  1. Source code of Mozilla Public License 2.0 components")
    w("  2. V8 and the libraries it includes")
    w("  3. The Rust standard library")
    w(f"  4. Rust crates ({ncrates}), grouped by licence")
    w("")
    w(RULE)
    w("1. Source code of Mozilla Public License 2.0 components")
    w(RULE)
    w("")
    w("These components are licensed under the Mozilla Public License 2.0 (its text is in part 4,")
    w("under MPL-2.0). Their source code is available at the addresses below. Stylo is included in")
    w("modified form; the published source is the complete source of the version included here,")
    w("with a list of the changes.")
    w("")
    for m in mpl:
        w(f"  {m}")
    w("")
    w(RULE)
    w("2. V8 and the libraries it includes")
    w(RULE)
    w("")
    w(f"V8 (https://v8.dev/), the JavaScript engine, from the rusty_v8 project")
    w(f"(https://github.com/denoland/rusty_v8, commit {V8_COMMIT}),")
    w("is linked statically together with the libraries below.")
    for name, lic, home, files in V8_PARTS:
        w("")
        w(THIN)
        w(f"{name}  ({home})")
        w(f"Licence: {lic}")
        w(THIN)
        for f in files:
            w("")
            w(file_text(v8_src / f).rstrip("\n"))
    vendor = v8_src / "third_party" / "rust" / "chromium_crates_io" / "vendor"
    w("")
    w(THIN)
    w("Rust crates linked into V8 (ICU4X, temporal_rs and their dependencies)")
    w(THIN)
    seen = {}
    for crate in V8_RUST_CRATES:
        dirs = sorted(vendor.glob(f"{crate.replace('_', '?')}-v*"))
        if not dirs:
            sys.exit(f"no folder for V8's crate {crate} in {vendor}")
        for d in dirs:
            pkg = {"manifest_path": str(d / "Cargo.toml"), "license_file": None}
            files = credits.license_files(pkg)
            if not files:
                sys.exit(f"no licence file in {d}")
            for f in files:
                seen.setdefault(credits.read_text(f), []).append(d.name)
    for text, names in seen.items():
        w("")
        w("Used by: " + ", ".join(sorted(set(names))))
        w("")
        w(text.rstrip("\n"))
    w("")
    w(RULE)
    w("3. The Rust standard library")
    w(RULE)
    w("")
    w("The Rust standard library (https://www.rust-lang.org/) is linked into the program. It is")
    w("licensed under the MIT license or the Apache License 2.0, at your option. Copyright (c) the")
    w("Rust Project Developers.")
    rustc_sysroot = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True,
                                   text=True).stdout.strip()
    lic = Path(rustc_sysroot) / "share" / "doc" / "rust" / "licenses"
    for f in ("MIT.txt", "Apache-2.0.txt"):
        w("")
        w(file_text(lic / f).rstrip("\n"))
    w("")
    w(RULE)
    w(f"4. Rust crates ({ncrates}), grouped by licence")
    w(RULE)
    for group, entries in groups:
        names = sorted({c for _, cs in entries for c in cs}, key=str.lower)
        w("")
        w(RULE)
        w(f"Licence: {group}")
        w(RULE)
        w("")
        for c in names:
            w(f"  {c}")
        for text, cs in entries:
            w("")
            w(THIN)
            w("Licence text of: " + ", ".join(sorted({c.split('  ')[0] for c in cs}, key=str.lower)))
            w(THIN)
            w("")
            w(text.rstrip("\n"))
    w("")
    data = "\r\n".join(lines) + "\r\n"
    Path(out_path).write_bytes(data.encode("utf-8"))
    print(f"{out_path}: {ncrates} crates in {len(groups)} licence groups, "
          f"{len(V8_PARTS)} V8 parts, {len(data) // 1024} KB")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--v8-src", type=Path, default=DEFAULT_V8_SRC)
    a = ap.parse_args()
    write(a.out, a.v8_src)


if __name__ == "__main__":
    main()
