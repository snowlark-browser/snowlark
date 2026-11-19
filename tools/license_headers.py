# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Checks that Snowlark's own source files carry the MPL-2.0 notice, and adds it where it is missing.

    python tools/license_headers.py          # lists the files without it; exit 1 if there are any
    python tools/license_headers.py --fix    # adds it

Covers the tracked `.rs`, `.py`, `.ps1` and `.js` files outside `vendor/` (third-party code keeps its own
licence and headers). A new source file needs the three lines at its top, after a `#!` line if it has one.
"""
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
NOTICE = (
    "This Source Code Form is subject to the terms of the Mozilla Public",
    "License, v. 2.0. If a copy of the MPL was not distributed with this",
    "file, You can obtain one at https://mozilla.org/MPL/2.0/.",
)
COMMENT = {".rs": "//", ".js": "//", ".py": "#", ".ps1": "#"}
EXCLUDE = ("vendor/",)


def files():
    out = subprocess.run(["git", "-C", str(REPO), "ls-files", "--cached", "--others", "--exclude-standard"],
                         capture_output=True, text=True, check=True).stdout.splitlines()
    for rel in out:
        if rel.startswith(EXCLUDE) or Path(rel).suffix not in COMMENT:
            continue
        if (REPO / rel).is_file():
            yield rel


def has_notice(text):
    return NOTICE[0] in text[:600]


def add_notice(text, suffix):
    c = COMMENT[suffix]
    header = "".join(f"{c} {line}\n" for line in NOTICE) + "\n"
    bom = "﻿" if text.startswith("﻿") else ""
    text = text[len(bom):]
    if text.startswith("#!"):
        first, _, rest = text.partition("\n")
        return bom + first + "\n" + header + rest.lstrip("\n")
    return bom + header + text


def main():
    fix = "--fix" in sys.argv[1:]
    missing = []
    for rel in files():
        path = REPO / rel
        data = path.read_bytes()
        text = data.decode("utf-8")
        if has_notice(text):
            continue
        missing.append(rel)
        if fix:
            newline = "\r\n" if b"\r\n" in data[:2000] else "\n"
            new = add_notice(text.replace("\r\n", "\n"), path.suffix)
            if newline == "\r\n":
                new = new.replace("\n", "\r\n")
            path.write_bytes(new.encode("utf-8"))
    for rel in missing:
        print(("added: " if fix else "missing: ") + rel)
    print(f"{len(missing)} file(s) {'given the notice' if fix else 'without the MPL-2.0 notice'}")
    return 0 if fix or not missing else 1


if __name__ == "__main__":
    sys.exit(main())
