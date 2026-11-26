#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Writes crates/snowlark-shell/src/credits.json.gz, the data of the snowlark://credits page.

    python tools/credits/generate.py            # rewrite the data file
    python tools/credits/generate.py --check    # exit 1 if the file is out of date

The Rust components are the crates linked into snowlark.exe: `cargo metadata` for the Windows target,
walked from the `snowlark` package along normal dependency edges only (no build-dependencies, no
dev-dependencies) and not into procedural macros, which run in the compiler and are not linked.
Each crate's license is the `license` field of its manifest and its texts are the license files in
its source (the crate cache, or vendor/). A crate with no license file in its source says so; nothing
is invented. The non-Rust components are listed in NON_RUST below with where each comes from.

The registry source must be downloaded (the crate cache dev.ps1 builds with: tools/devenv.py cargo_home).
"""
import gzip
import hashlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path
import os as _os, sys as _sys  # noqa: E401
_sys.path.insert(0, _os.path.join(_os.path.dirname(_os.path.abspath(__file__)), ".."))
import devenv  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "crates" / "snowlark-shell" / "src" / "credits.json.gz"
TARGET = "x86_64-pc-windows-msvc"
LICENSE_FILE = re.compile(
    r"^(licen[cs]e|copying|unlicense|notice|copyright)([-_. ].*)?$", re.I
)

# Components that are not crates. `texts` are paths (relative to the registry source, the vendor
# directory or the repository) of license files found in the build's inputs; a component with none
# says so on the page.
NON_RUST = [
    {
        "name": "V8",
        "version": "from the v8 crate",
        "license": "BSD-3-Clause",
        "home": "https://v8.dev/",
        "note": "JavaScript engine, linked statically through the v8 crate (rusty_v8's prebuilt "
        "library). The v8 crate's package does not include V8's LICENSE file, so its text is "
        "not reproduced here; it is at https://chromium.googlesource.com/v8/v8/+/main/LICENSE.",
    },
    {
        "name": "Code bundled with V8",
        "version": "",
        "license": "various (see note)",
        "home": "https://chromium.googlesource.com/v8/v8/+/main/third_party/",
        "note": "V8's prebuilt library includes ICU (Unicode License), Abseil (Apache-2.0), "
        "Highway (Apache-2.0 or BSD-3-Clause), simdutf (Apache-2.0 or MIT), fast_float "
        "(Apache-2.0, MIT or BSL-1.0), Dragonbox (Apache-2.0 or BSL-1.0), FP16 (MIT) and "
        "libc++ (Apache-2.0 with LLVM exceptions). The v8 crate's package contains their "
        "sources but not their license files, so the texts are not reproduced here.",
    },
    {
        "name": "ANGLE",
        "version": "the commit in tools/angle/pin.txt",
        "license": "BSD-3-Clause",
        "home": "https://chromium.googlesource.com/angle/angle",
        "note": "OpenGL ES implementation WebGL runs on, over Direct3D 11: libEGL.dll and "
        "libGLESv2.dll, built from source and loaded at run time from the program's folder. "
        "Its license is at https://chromium.googlesource.com/angle/angle/+/main/LICENSE; the "
        "code it links from its own third_party folder keeps its own licenses there.",
    },
    {
        "name": "OTS (OpenType Sanitizer)",
        "version": "c5d249ce",
        "license": "BSD-3-Clause",
        "home": "https://github.com/khaledhosny/ots",
        "note": "Checks every web font before the text engine reads it. Compiled from its unmodified "
        "source (vendor/ots).",
        "texts": ["vendor/ots/LICENSE"],
    },
    {
        "name": "woff2",
        "version": "1c69169e",
        "license": "MIT",
        "home": "https://github.com/google/woff2",
        "note": "The WOFF 2.0 decoder OTS uses. Compiled from its unmodified source (vendor/woff2).",
        "texts": ["vendor/woff2/LICENSE"],
    },
    {
        "name": "dav1d",
        "version": "the release in tools/dav1d/pin.txt",
        "license": "BSD-2-Clause; ISC (x86inc.asm)",
        "home": "https://code.videolan.org/videolan/dav1d",
        "note": "The AV1 decoder of AVIF images (through CrabbyAvif), a static library built from "
        "its unmodified source by tools/dav1d/build.ps1. Its assembly uses the x264 project's "
        "x86inc.asm, under the ISC license.",
        "texts": ["tools/dav1d/LICENSE.dav1d", "tools/dav1d/LICENSE.x86inc"],
    },
    {
        "name": "uBlock Origin scriptlets and redirect resources",
        "version": "",
        "license": "GPL-3.0",
        "home": "https://github.com/gorhill/uBlock",
        "note": "Not part of the program: downloaded at run time into the profile folder, "
        "never bundled (THIRD-PARTY.md).",
        "texts": [],
    },
    {
        "name": "Filter lists (uBlock filters, EasyList, EasyPrivacy, Online Malicious URL "
        "Blocklist, Peter Lowe's list)",
        "version": "",
        "license": "GPL-3.0; GPL-3.0 or CC-BY-SA-3.0; CC0-1.0; free to use",
        "home": "https://github.com/uBlockOrigin/uAssets",
        "note": "Not part of the program: downloaded at run time from their publishers into the "
        "profile folder, never bundled (THIRD-PARTY.md).",
        "texts": [],
    },
]


def run(*args, cwd=ROOT):
    return subprocess.run(args, cwd=cwd, check=True, capture_output=True, text=True, encoding="utf-8").stdout


def metadata(target=TARGET):
    env = dict(os.environ)
    env.setdefault("CARGO_HOME", str(devenv.cargo_home()))
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--offline", "--filter-platform", target],
        cwd=ROOT, check=True, capture_output=True, text=True, encoding="utf-8", env=env,
    ).stdout
    return json.loads(out)


def linked_packages(md):
    """The packages (by id) linked into snowlark.exe."""
    pkgs = {p["id"]: p for p in md["packages"]}
    nodes = {n["id"]: n for n in md["resolve"]["nodes"]}
    root = next(p["id"] for p in md["packages"] if p["name"] == "snowlark")
    seen, stack = set(), [root]
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes[pid]["deps"]:
            if not any(k["kind"] is None for k in dep["dep_kinds"]):
                continue
            target = pkgs[dep["pkg"]]
            if any("proc-macro" in t["kind"] for t in target["targets"]):
                continue
            stack.append(dep["pkg"])
    return [pkgs[i] for i in sorted(seen)]


def license_files(pkg):
    d = Path(pkg["manifest_path"]).parent
    found = []
    for base in (d, d / "license", d / "licenses", d / "LICENSES"):
        if not base.is_dir():
            continue
        for f in sorted(base.iterdir()):
            if f.is_file() and LICENSE_FILE.match(f.name) and f.stat().st_size < 200_000:
                found.append(f)
    if pkg.get("license_file"):
        f = d / pkg["license_file"]
        if f.is_file() and f not in found:
            found.append(f)
    return found


def read_text(f):
    data = f.read_bytes()
    try:
        s = data.decode("utf-8")
    except UnicodeDecodeError:
        s = data.decode("latin-1")
    return s.replace("\r\n", "\n").strip("\n") + "\n"


# Crates whose manifest names no license, with the license of the license file in their source.
MANIFEST_LICENSES = {
    # (CrabbyAvif, a git dependency: its LICENSE is the Apache License 2.0)
    "crabby-avif": "Apache-2.0",
}


def package_license(pkg):
    return (pkg.get("license") or MANIFEST_LICENSES.get(pkg["name"])
            or ("see the license file" if pkg.get("license_file") else ""))


def package_home(pkg):
    """The package's home page: its manifest's, else its git repository, else its crates.io page."""
    source = pkg.get("source") or ""
    git = source[4:].split("?")[0].split("#")[0] if source.startswith("git+") else None
    return pkg.get("homepage") or pkg.get("repository") or git or f"https://crates.io/crates/{pkg['name']}"


def build():
    md = metadata()
    texts, index = [], {}

    def text_id(s):
        h = hashlib.sha256(s.encode()).hexdigest()
        if h not in index:
            index[h] = len(texts)
            texts.append(s)
        return index[h]

    comps = []
    for pkg in linked_packages(md):
        if pkg["source"] is None and "vendor" not in Path(pkg["manifest_path"]).parts:
            continue  # Snowlark's own crates
        ids = []
        for f in license_files(pkg):
            ids.append(text_id(read_text(f)))
        comps.append({
            "name": pkg["name"],
            "version": pkg["version"],
            "license": package_license(pkg),
            "home": package_home(pkg),
            "texts": sorted(set(ids)),
            **({"vendored": True} if pkg["source"] is None else {}),
        })
    comps.sort(key=lambda c: (c["name"].lower(), c["version"]))
    # Everything else in Cargo.lock: Snowlark's own crates, and what is not linked into snowlark.exe
    # (build scripts and their dependencies, procedural macros, dev-dependencies, other platforms).
    linked = {f"{c['name']} {c['version']}" for c in comps}
    own = sorted(
        f"{p['name']} {p['version']}"
        for p in md["packages"]
        if p["source"] is None and "vendor" not in Path(p["manifest_path"]).parts
    )
    lock = (ROOT / "Cargo.lock").read_text(encoding="utf-8")
    pairs = re.findall(r'name = "(.*?)"\nversion = "(.*?)"', lock.replace("\r\n", "\n"))
    in_lock = {f"{n} {v}" for n, v in pairs}
    excluded = sorted(in_lock - linked - set(own))
    extra = []
    for c in NON_RUST:
        c = dict(c)
        c["texts"] = [text_id(read_text(ROOT / p)) for p in c.get("texts", [])]
        extra.append(c)
    return {
        "v": 1,
        "texts": texts,
        "crates": comps,
        "others": extra,
        "excluded": excluded,
        "own": own,
    }


def encode(data):
    raw = json.dumps(data, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    return gzip.compress(raw, 9, mtime=0)


def main():
    blob = encode(build())
    if "--check" in sys.argv:
        ok = OUT.exists() and OUT.read_bytes() == blob
        print("up to date" if ok else "credits.json.gz is out of date: run tools/credits/generate.py")
        sys.exit(0 if ok else 1)
    OUT.write_bytes(blob)
    d = json.loads(gzip.decompress(blob))
    missing = [c["name"] for c in d["crates"] if not c["texts"]]
    print(f"{len(d['crates'])} crates, {len(d['texts'])} distinct license texts, {len(blob)} bytes")
    print("without a license text:", ", ".join(missing) or "none")


if __name__ == "__main__":
    main()
