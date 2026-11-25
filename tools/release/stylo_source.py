#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Exports the source of our modified Stylo (MPL-2.0), as the Mozilla Public License requires for
software distributed in executable form, into a folder ready to be the public repository.

    python tools/release/stylo_source.py <out-dir> [--rev <commit>]

The folder holds `vendor/stylo` at `--rev` (default HEAD) exactly as it is built, the licence text,
a README naming the upstream release and listing the changes (from vendor/stylo/SNOWLARK-PATCHES.md),
and CHANGES.diff, the full diff from the unmodified release to this source. Before writing, it checks
that the vendored base commit is byte for byte the crates.io release (when the release is in the
crate cache), so the README's claim about the base is true.
"""
import argparse
import io
import os
import re
import shutil
import subprocess
import sys
import tarfile
from pathlib import Path
import os as _os, sys as _sys  # noqa: E401
_sys.path.insert(0, _os.path.join(_os.path.dirname(_os.path.abspath(__file__)), ".."))
import devenv  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
# The commit that put the unmodified release into vendor/stylo ("stylo 0.24.0 from crates.io, unmodified").
BASE = "21d9f425"
VERSION = "0.24.0"
# servo/stylo's commit the 0.24.0 package was made from (its .cargo_vcs_info.json), folder `style`.
UPSTREAM_COMMIT = "32359dc3f17780ee269980cb7a3a7f8597a1dc0e"


def git(*args, binary=False):
    out = subprocess.run(["git", "-C", str(ROOT), *args], check=True, capture_output=True).stdout
    return out if binary else out.decode("utf-8")


def tree(rev):
    """{path relative to vendor/stylo: bytes} at `rev`."""
    data = git("archive", "--format=tar", rev, "vendor/stylo", binary=True)
    files = {}
    with tarfile.open(fileobj=io.BytesIO(data)) as t:
        for m in t.getmembers():
            if m.isfile():
                files[m.name[len("vendor/stylo/"):]] = t.extractfile(m).read()
    return files


def check_base():
    home = devenv.cargo_home()
    regs = sorted((home / "registry" / "src").glob(f"*/stylo-{VERSION}"))
    if not regs:
        print(f"stylo {VERSION} is not in the crate cache: the base is not checked")
        return
    reg = regs[0]
    # cargo's unpack marker is not part of the package (the 0.24.0 base commit happens to carry it)
    base = {k: v for k, v in tree(BASE).items() if k.rsplit("/", 1)[-1] != ".cargo-ok"}
    released = {
        str(p.relative_to(reg)).replace("\\", "/"): p.read_bytes()
        for p in reg.rglob("*") if p.is_file() and p.name != ".cargo-ok"
    }
    norm = lambda b: b.replace(b"\r\n", b"\n")  # noqa: E731
    differ = sorted(
        k for k in set(base) | set(released)
        if k not in base or k not in released or norm(base[k]) != norm(released[k])
    )
    if differ:
        sys.exit(f"the base commit {BASE} differs from crates.io stylo {VERSION}: {differ[:10]}")
    print(f"base {BASE} matches crates.io stylo {VERSION} ({len(base)} files)")


def patch_list(text):
    """The numbered entries of SNOWLARK-PATCHES.md, as Markdown."""
    start = text.find("\n1. ")
    return text[start + 1:].strip() + "\n" if start >= 0 else text


def mpl_text():
    """The MPL-2.0 text, from a crate in the cache that carries it (cssparser)."""
    home = devenv.cargo_home()
    for p in sorted((home / "registry" / "src").glob("*/cssparser-*/LICENSE")):
        t = p.read_text(encoding="utf-8")
        if "Mozilla Public License Version 2.0" in t:
            return t.replace("\r\n", "\n")
    sys.exit("no MPL-2.0 text found in the crate cache")


README = """# Stylo, as used in Snowlark

This repository is the source code of the CSS engine in [Snowlark](https://snowlark.org), published
under the Mozilla Public License 2.0 (`LICENSE`). It is [Stylo](https://github.com/servo/stylo), the
CSS engine of Servo and Firefox, with changes for Snowlark.

## Base

- The `stylo` crate, version {version}, from crates.io: <https://crates.io/crates/stylo/{version}>
- made from servo/stylo commit [`{upstream_short}`](https://github.com/servo/stylo/tree/{upstream}/style),
  folder `style`.

## Contents

- The complete source of the `stylo` crate as it is built into Snowlark {rev_note}, including the
  files that are unchanged from the release.
- `CHANGES.diff`: every change from the unmodified release to this source, as one diff.
- `SNOWLARK-PATCHES.md`: the list of changes with the reason for each. In the source, each change
  is marked `Snowlark patch`.

The other Stylo crates Snowlark uses (`selectors`, `stylo_atoms`, `stylo_dom`, `stylo_traits`,
`stylo_static_prefs`, `to_shmem`) and `cssparser` are used unmodified from crates.io.

## Changes

{changes}
"""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out", type=Path)
    ap.add_argument("--rev", default="HEAD")
    a = ap.parse_args()
    rev = git("rev-parse", a.rev).strip()
    check_base()
    files = tree(rev)
    if a.out.exists():
        shutil.rmtree(a.out)
    a.out.mkdir(parents=True)
    for rel, data in files.items():
        p = a.out / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)
    diff = git("diff", "--no-color", "--no-renames", f"--relative=vendor/stylo", BASE, rev, "--",
               "vendor/stylo", binary=True)
    (a.out / "CHANGES.diff").write_bytes(diff)
    (a.out / "LICENSE").write_text(mpl_text(), encoding="utf-8", newline="\n")
    patches = files["SNOWLARK-PATCHES.md"].decode("utf-8")
    readme = README.format(
        version=VERSION,
        upstream=UPSTREAM_COMMIT,
        upstream_short=UPSTREAM_COMMIT[:12],
        rev_note=f"(source revision {rev[:12]})",
        changes=patch_list(patches),
    )
    (a.out / "README.md").write_text(readme, encoding="utf-8", newline="\n")
    changed = len(re.findall(rb"^diff --git ", diff, re.M))
    print(f"{a.out}: {len(files)} source files, {changed} changed, from {rev[:12]}")


if __name__ == "__main__":
    main()
