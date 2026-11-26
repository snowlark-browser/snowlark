# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""A stand-in for a Linux C compiler, archiver and pkg-config, for `cargo check` from Windows only
(check.ps1).

Build scripts compile C (ring, zstd, mimalloc, ...) for the target, and Windows has no compiler for
Linux. `cargo check` never links, so empty outputs are enough: the compiler writes an empty object
for `-o`, the archiver an empty archive. Build scripts that look a system library up (the window's
xkbcommon) are told it is there: `--libs` names it, `--modversion` says 99.0. Never use it for a
build.

    python stub_cc.py cc <compiler arguments>
    python stub_cc.py ar <operations> <archive> [members...]
    python stub_cc.py pkg-config <pkg-config arguments>
"""

import sys


def main(argv: list[str]) -> int:
    if not argv:
        return 2
    kind, args = argv[0], argv[1:]
    if kind == "ar":
        if len(args) >= 2:
            with open(args[1], "wb") as f:
                f.write(b"!<arch>\n")
        return 0
    if kind == "pkg-config":
        names = [a for a in args if not a.startswith("-")]
        if "--modversion" in args:
            print("\n".join("99.0" for _ in names))
        elif "--libs" in args:
            print(" ".join(f"-l{n}" for n in names))
        return 0
    if "--version" in args or "-v" in args:
        print("stub-cc 1.0 (check only)")
        return 0
    out = None
    for i, a in enumerate(args):
        if a == "-o" and i + 1 < len(args):
            out = args[i + 1]
        elif a.startswith("-o") and len(a) > 2:
            out = a[2:]
    if out:
        open(out, "wb").close()
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
