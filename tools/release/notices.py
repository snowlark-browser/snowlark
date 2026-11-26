#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Writes THIRD-PARTY-NOTICES.txt for a release of the browser (shipped as snowlark.exe).

    python tools/release/notices.py <out.txt> [--v8-src <dir>] [--angle-src <dir>] [--platform linux]

With `--platform linux` it describes the Linux build (tools/release/package.sh) instead: the crates
`cargo metadata` gives for the Linux target, V8's Linux library (which also links libc++abi), ANGLE
as tools/angle/build.sh builds it (libEGL.so and libGLESv2.so, the OpenGL back end), Skia (the
prebuilt library skia-bindings links, with the libraries in it: SKIA_PARTS, their licence files
kept in tools/release/skia), and no dav1d (the Linux build decodes AVIF without it). The system's
own libraries the program loads (fontconfig, FreeType, HarfBuzz, xkbcommon, Wayland, EGL) are not
shipped and not listed. The file has LF line ends there, CRLF on Windows.

What it covers, and where each part comes from:

- Every Rust crate linked into snowlark.exe: `cargo metadata` for the Windows target, walked from the
  `snowlark` package along normal dependency edges only, not into procedural macros (the same walk as
  the credits page, tools/credits/generate.py). Each crate's licence texts are the licence files in
  its source (the crate cache, or vendor/). A crate whose package has no licence file gets the
  standard text of its licence, with the authors from its manifest as the copyright holders, and
  says so.
- The C and C++ libraries compiled in (VENDORED_LIBRARIES: OTS and woff2, from vendor/, and dav1d,
  whose licence files are kept in tools/dav1d).
- The Rust standard library (from the toolchain's share/doc/rust/licenses).
- V8 and the third-party code its build links into the library we link (`--v8-src`: the rusty_v8
  checkout at the commit vendor/v8 is taken from; tools/v8-pgo builds the pinned library from it).
  V8_PARTS lists them: the libraries in the static library's link list (its `rusty_v8.ninja`) and
  the headers V8's own sources include from third_party/.
- ANGLE (libEGL.dll and libGLESv2.dll, shipped beside the program) and the third-party code its
  build links into libGLESv2 (`--angle-src`: the checkout tools/angle/build.ps1 builds the commit
  tools/angle/pin.txt names from). ANGLE_PARTS lists them: the link inputs of libGLESv2.dll
  (`ninja -C out/Release -t query libGLESv2.dll`, walked through the static libraries it names),
  each with the licence file its README.chromium names.

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

# The C and C++ libraries compiled into the program (THIRD-PARTY.md): those vendored in this
# repository, and dav1d, built from its release by tools/dav1d/build.ps1 (its licence files are
# kept in tools/dav1d). (name, licence, home, licence files relative to the repository).
VENDORED_LIBRARIES = [
    ("dav1d (the AV1 decoder)", "BSD-2-Clause; x86inc.asm: ISC",
     "https://code.videolan.org/videolan/dav1d, release " + dict(
         line.split("=", 1) for line in (ROOT / "tools" / "dav1d" / "pin.txt").read_text(encoding="utf-8").splitlines()
         if "=" in line and not line.startswith("#"))["tag"],
     ["tools/dav1d/LICENSE.dav1d", "tools/dav1d/LICENSE.x86inc"]),
    ("OTS (OpenType Sanitizer)", "BSD-3-Clause",
     "https://github.com/khaledhosny/ots, commit c5d249ce90ab0aa3e76eb0dfad28746f407201d0",
     ["vendor/ots/LICENSE"]),
    ("woff2 (the WOFF 2.0 decoder)", "MIT",
     "https://github.com/google/woff2, commit 1c69169e9e1811dccd6c54c532fedda300233968",
     ["vendor/woff2/LICENSE"]),
]

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

# The ANGLE checkout tools/angle/build.ps1 builds from, and the commit it must be at.
DEFAULT_ANGLE_SRC = devenv.dev_dir() / "angle" / "src"
ANGLE_PIN = ROOT / "tools" / "angle" / "pin.txt"

# (name, licence, home, files relative to the ANGLE checkout, note): ANGLE itself and what its build
# links into libGLESv2.dll with the Direct3D 11 back end only (tools/angle/build.ps1's arguments).
# A part whose licence text is already given in this section names it in its note instead.
ANGLE_PARTS = [
    ("ANGLE", "BSD-3-Clause", "https://chromium.googlesource.com/angle/angle", ["LICENSE"], ""),
    ("Khronos EGL and OpenGL ES headers", "Apache-2.0", "https://registry.khronos.org/",
     [], "The text of the Apache License 2.0 is given under Abseil below."),
    ("xxHash", "BSD-2-Clause", "https://github.com/Cyan4973/xxHash",
     ["src/common/third_party/xxhash/src/LICENSE"], ""),
    ("volk", "MIT", "https://github.com/zeux/volk", ["src/third_party/volk/src/LICENSE.md"], ""),
    ("Abseil", "Apache-2.0", "https://abseil.io/", ["third_party/abseil-cpp/LICENSE"], ""),
    ("astc-encoder", "Apache-2.0", "https://github.com/ARM-software/astc-encoder",
     ["third_party/astc-encoder/src/LICENSE.txt"], ""),
    ("Perfetto", "Apache-2.0 and others (see text)", "https://perfetto.dev/",
     ["third_party/perfetto/LICENSE"], ""),
    ("zlib", "Zlib", "https://github.com/madler/zlib", ["third_party/zlib/LICENSE"], ""),
    ("zlib compression utilities (third_party/zlib/google)", "BSD-3-Clause",
     "https://chromium.googlesource.com/chromium/src/third_party/zlib/", ["buildtools/LICENSE"], ""),
    ("libc++", "Apache-2.0 WITH LLVM-exception", "https://libcxx.llvm.org/",
     ["third_party/libc++/src/LICENSE.TXT"], ""),
    ("LLVM libc (shared headers)", "Apache-2.0 WITH LLVM-exception", "https://libc.llvm.org/",
     ["third_party/llvm-libc/src/LICENSE.TXT"], ""),
    ("compiler-rt builtins", "Apache-2.0 WITH LLVM-exception", "https://compiler-rt.llvm.org/",
     [], "The licence text is the one given under libc++ above."),
]

# What tools/angle/build.sh's Linux build (the OpenGL back end through the system's EGL, no Vulkan)
# links into libGLESv2.so: the link inputs of `ninja -C out/Release -t query libGLESv2.so`, walked as
# for Windows.
ANGLE_PARTS_LINUX = ANGLE_PARTS + [
    ("libc++abi", "Apache-2.0 WITH LLVM-exception", "https://libcxx.llvm.org/",
     [], "The licence text is the one given under libc++ above."),
    # (libdrm has no licence file: each source file carries its notice; `#notice` takes it)
    ("libdrm", "MIT", "https://gitlab.freedesktop.org/mesa/drm",
     [f"third_party/libdrm/src/{f}.c#notice" for f in ("xf86drm", "xf86drmHash", "xf86drmMode", "xf86drmRandom")],
     ""),
]

# V8's Linux library also links libc++abi (Windows' uses the C runtime's).
V8_PARTS_LINUX = V8_PARTS + [
    ("libc++abi", "Apache-2.0 WITH LLVM-exception", "https://libcxx.llvm.org/",
     ["third_party/libc++abi/src/LICENSE.TXT"]),
]

# Skia and what its prebuilt library (skia-bindings, features ganesh, gl, jpeg and pdf; FreeType is the
# system's) links in: (name, licence, home, files relative to the repository). The licence files are kept
# in tools/release/skia, from the revisions the DEPS file of Skia m158.4 (rust-skia's) pins; renew them with
# a Skia upgrade.
SKIA_PARTS = [
    ("Skia", "BSD-3-Clause", "https://skia.org/ (rust-skia's m158.4)", ["tools/release/skia/LICENSE.skia"]),
    ("libpng", "libpng-2.0", "https://www.libpng.org/", ["tools/release/skia/LICENSE.libpng"]),
    ("libjpeg-turbo", "IJG AND BSD-3-Clause AND Zlib", "https://libjpeg-turbo.org/",
     ["tools/release/skia/LICENSE.libjpeg-turbo", "tools/release/skia/README.ijg"]),
    ("zlib (Chromium's copy)", "Zlib", "https://chromium.googlesource.com/chromium/src/third_party/zlib/",
     ["tools/release/skia/LICENSE.zlib"]),
    ("Expat", "MIT", "https://libexpat.github.io/", ["tools/release/skia/LICENSE.expat"]),
    ("Wuffs", "Apache-2.0 OR MIT", "https://github.com/google/wuffs", ["tools/release/skia/LICENSE.wuffs"]),
]

# The Rust target each platform's program is built for.
TARGETS = {"windows": "x86_64-pc-windows-msvc", "linux": "x86_64-unknown-linux-gnu"}

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
    """A licence file's text; for `<source file>#notice`, the first comment of that source file that
    holds a copyright notice, without its comment marks."""
    notice = path.name.endswith("#notice")
    if notice:
        path = path.with_name(path.name[: -len("#notice")])
    if not path.is_file():
        sys.exit(f"missing licence file: {path}")
    text = credits.read_text(path)
    if not notice:
        return text
    for block in text.split("/*")[1:]:
        body = block.split("*/", 1)[0]
        if "Copyright" in body:
            lines = [ln.strip() for ln in body.splitlines()]
            lines = [ln[1:].strip() if ln.startswith("*") else ln for ln in lines]
            return "\n".join(lines).strip("\n") + "\n"
    sys.exit(f"no copyright notice in {path}")


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
        license = pkg.get("license") or credits.MANIFEST_LICENSES.get(name) or ""
        group = norm_license(license)
        vendored = pkg["source"] is None
        home = pkg.get("repository") or credits.package_home(pkg)
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


def git_head(path):
    return subprocess.run(["git", "-C", str(path), "rev-parse", "HEAD"], capture_output=True,
                          text=True).stdout.strip()


def angle_commit():
    for line in ANGLE_PIN.read_text(encoding="utf-8").splitlines():
        if line.startswith("commit="):
            return line.split("=", 1)[1].strip()
    sys.exit(f"{ANGLE_PIN} names no commit")


def write(out_path, v8_src, angle_src, platform="windows"):
    linux = platform == "linux"
    md = credits.metadata(TARGETS[platform])
    groups, mpl, ncrates = crate_texts(md)
    v8_parts = V8_PARTS_LINUX if linux else V8_PARTS
    angle_parts = ANGLE_PARTS_LINUX if linux else ANGLE_PARTS
    angle_libs = ("libEGL.so", "libGLESv2.so") if linux else ("libEGL.dll", "libGLESv2.dll")
    # (the Linux build has no dav1d: tools/release/package.sh builds without SNOWLARK_DAV1D_DIR)
    vendored = [v for v in VENDORED_LIBRARIES if not (linux and v[0].startswith("dav1d"))]
    sections = ["Source code of Mozilla Public License 2.0 components",
                "V8 and the libraries it includes",
                f"ANGLE ({angle_libs[0]}, {angle_libs[1]}) and the libraries it includes"]
    if linux:
        sections.append("Skia and the libraries it includes")
    sections += ["Other C and C++ libraries", "The Rust standard library",
                 f"Rust crates ({ncrates}), grouped by licence"]
    number = {s.split(" ")[0]: i + 1 for i, s in enumerate(sections)}

    def heading(word):
        i = number[word]
        return f"{i}. {sections[i - 1]}"
    head = git_head(v8_src)
    if head != V8_COMMIT:
        sys.exit(f"{v8_src} is at {head or 'no commit'}, not {V8_COMMIT} (vendor/v8's commit)")
    angle = angle_commit()
    head = git_head(angle_src)
    if head != angle:
        sys.exit(f"{angle_src} is at {head or 'no commit'}, not {angle} (tools/angle/pin.txt)")
    lines = []
    w = lines.append
    w("Snowlark: third-party software notices")
    w("")
    w("Snowlark contains the open source software listed below. Each part is used under the licence")
    w("shown, and the full text of each licence follows its list of components.")
    w("")
    w("Contents")
    for i, s in enumerate(sections):
        w(f"  {i + 1}. {s}")
    w("")
    w(RULE)
    w(heading("Source"))
    w(RULE)
    w("")
    w(f"These components are licensed under the Mozilla Public License 2.0 (its text is in part {number['Rust']},")
    w("under MPL-2.0). Their source code is available at the addresses below. Stylo is included in")
    w("modified form; the published source is the complete source of the version included here,")
    w("with a list of the changes.")
    w("")
    for m in mpl:
        w(f"  {m}")
    w("")
    w(RULE)
    w(heading("V8"))
    w(RULE)
    w("")
    w(f"V8 (https://v8.dev/), the JavaScript engine, from the rusty_v8 project")
    w(f"(https://github.com/denoland/rusty_v8, commit {V8_COMMIT}),")
    w("is linked statically together with the libraries below.")
    for name, lic, home, files in v8_parts:
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
    w(heading("ANGLE"))
    w(RULE)
    w("")
    w("ANGLE (https://chromium.googlesource.com/angle/angle, commit")
    w(f"{angle}), the OpenGL ES implementation WebGL runs on, is")
    w(f"built from its unmodified source into {angle_libs[0]} and {angle_libs[1]}, which ship beside the")
    w(f"program. {angle_libs[1]} includes the libraries below.")
    for name, lic, home, files, note in angle_parts:
        w("")
        w(THIN)
        w(f"{name}  ({home})")
        w(f"Licence: {lic}")
        w(THIN)
        if note:
            w("")
            w(note)
        for f in files:
            w("")
            w(file_text(angle_src / f).rstrip("\n"))
    if linux:
        w("")
        w(RULE)
        w(heading("Skia"))
        w(RULE)
        w("")
        w("Skia, the 2D graphics library pages are drawn with, is linked statically as the rust-skia")
        w("project builds it from its unmodified source (https://github.com/rust-skia/rust-skia), together")
        w("with the libraries below.")
        for name, lic, home, files in SKIA_PARTS:
            w("")
            w(THIN)
            w(f"{name}  ({home})")
            w(f"Licence: {lic}")
            w(THIN)
            for f in files:
                w("")
                w(file_text(ROOT / f).rstrip("\n"))
    w("")
    w(RULE)
    w(heading("Other"))
    w(RULE)
    w("")
    w("These libraries are compiled from their unmodified source into the program.")
    for name, lic, home, files in vendored:
        w("")
        w(THIN)
        w(f"{name}  ({home})")
        w(f"Licence: {lic}")
        w(THIN)
        for f in files:
            w("")
            w(file_text(ROOT / f).rstrip("\n"))
    w("")
    w(RULE)
    w(heading("The"))
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
    w(heading("Rust"))
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
    end = "\n" if linux else "\r\n"
    data = end.join(lines) + end
    Path(out_path).write_bytes(data.encode("utf-8"))
    skia = f", {len(SKIA_PARTS)} Skia parts" if linux else ""
    print(f"{out_path}: {ncrates} crates in {len(groups)} licence groups, "
          f"{len(v8_parts)} V8 parts, {len(angle_parts)} ANGLE parts{skia}, {len(data) // 1024} KB")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--v8-src", type=Path, default=DEFAULT_V8_SRC)
    ap.add_argument("--angle-src", type=Path, default=DEFAULT_ANGLE_SRC)
    ap.add_argument("--platform", choices=sorted(TARGETS), default="windows")
    a = ap.parse_args()
    write(a.out, a.v8_src, a.angle_src, a.platform)


if __name__ == "__main__":
    main()
