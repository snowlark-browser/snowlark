#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds the preview .tar.gz of the browser for Linux (the counterpart of package.ps1's zip):
#
#     tools/release/package.sh [--out <dir>] [--skip-build] [--allow-other]
#
# From a clean checkout at the newest origin/main it makes a `--release` build (tools/linux/cargo.sh), then packs
#     snowlark                    target/release/snowlark, its debug information and symbols stripped (kept
#                                 beside the archive as snowlark.debug, linked to it by its debug link)
#     libEGL.so, libGLESv2.so     ANGLE, which WebGL runs on: the Linux build tools/angle/pin.txt pins (the build
#                                 copies them beside the executable; their SHA-256 must be the pinned one),
#                                 stripped the same way
#     THIRD-PARTY-NOTICES.txt     tools/release/notices.py --platform linux
#     README.txt                  tools/release/README-linux.txt with the build's date and revision
# into <out>/snowlark-preview-<date>-<sha>-linux-x86_64.tar.gz (one top-level folder of that name), writes its
# SHA-256 beside it (<archive>.sha256, sha256sum's format) and prints its size and SHA-256. The build carries no
# release name: Linux builds do not check for updates (their releases are not signed).
#
# Needs: SNOWLARK_DEV_DIR (the V8 library and ANGLE, as tools/linux/cargo.sh finds them), the rusty_v8 checkout
# notices.py reads V8's licences from (SNOWLARK_V8_SRC) and ANGLE's checkout (<dev dir>/angle/src, from
# tools/angle/build.sh), python3, objcopy.
#
# --skip-build packs the release build that is already there (it must be of this revision).
# --allow-other packs a checkout that is not the newest origin/main, or not clean, or libraries with other hashes
# (for trying the script out; never for an archive that is published).
#
# Nothing is published: a release is the maintainers' decision.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
dev="${SNOWLARK_DEV_DIR:?SNOWLARK_DEV_DIR is not set}"
out="$dev/release"
build=1
allow=0
while [ $# -gt 0 ]; do
    case "$1" in
        --out) out="$2"; shift 2 ;;
        --skip-build) build=0; shift ;;
        --allow-other) allow=1; shift ;;
        *) echo "package.sh: unknown argument $1" >&2; exit 2 ;;
    esac
done
fail() { echo "package.sh: $*" >&2; exit 1; }
cd "$root"

git fetch --quiet origin || true
head="$(git rev-parse HEAD)"
main="$(git rev-parse origin/main)"
dirty="$(git status --porcelain --untracked-files=no)"
if [ "$allow" = 0 ]; then
    [ "$head" = "$main" ] || fail "HEAD ($head) is not the newest origin/main ($main)"
    [ -z "$dirty" ] || fail "the checkout has uncommitted changes: $dirty"
fi
sha="${head:0:8}"
date="$(date -u +%Y-%m-%d)"
name="snowlark-preview-$date-$sha-linux-x86_64"

if [ "$build" = 1 ]; then
    mkdir -p "$out"
    free=$(( $(df -Pk "$out" | awk 'NR==2 {print $4}') / 1024 / 1024 ))
    [ "$free" -ge 35 ] || fail "$out has $free GB free; a release build needs about 35 GB"
    # (no SNOWLARK_RELEASE: a Linux build never checks for updates)
    tools/linux/cargo.sh build --release -p snowlark || fail "the release build failed"
fi
exe="$root/target/release/snowlark"
[ -f "$exe" ] || fail "no $exe"
# The executable carries the revision it was built from (the About page shows it).
if ! grep -qa "${head:0:7}" "$exe" && [ "$allow" = 0 ]; then
    fail "$exe was not built from ${head:0:7} (rebuild without --skip-build)"
fi

stage="$out/$name"
rm -rf "$stage"
mkdir -p "$stage"
# Debug information and the symbol table go into a file of their own beside the archive (as Windows' PDB stays
# out of the zip); the shipped copy names it by its debug link and keeps only the dynamic symbols.
strip_into() {
    objcopy --only-keep-debug "$1" "$out/$2.debug"
    objcopy --strip-all --add-gnu-debuglink="$out/$2.debug" "$1" "$stage/$2"
}
strip_into "$exe" snowlark

# ANGLE's two libraries, as the build put them beside the executable, checked against the pin: a library built
# from another commit or with other arguments is not the one the notices and the build instructions describe.
pin() { sed -n "s/^$1=//p" "$root/tools/angle/pin.txt" | tr -d '\r'; }
for f in libEGL libGLESv2; do
    lib="$root/target/release/$f.so"
    [ -f "$lib" ] || fail "no $lib (build ANGLE with tools/angle/build.sh, then rebuild)"
    hash="$(sha256sum "$lib" | cut -d' ' -f1)"
    if [ "$hash" != "$(pin "$f-linux-sha256")" ] && [ "$allow" = 0 ]; then
        fail "$lib has SHA-256 $hash, not the one tools/angle/pin.txt pins"
    fi
    strip_into "$lib" "$f.so"
done

angle_src="$dev/angle/src"
python3 tools/release/notices.py --platform linux "$stage/THIRD-PARTY-NOTICES.txt" \
    --v8-src "${SNOWLARK_V8_SRC:?SNOWLARK_V8_SRC is not set (the rusty_v8 checkout)}" --angle-src "$angle_src" \
    || fail "the notices were not written"
sed -e "s/{DATE}/$date/" -e "s/{SHA}/$sha/" tools/release/README-linux.txt > "$stage/README.txt"

archive="$out/$name.tar.gz"
# (one top-level folder; owners and times fixed, so the archive depends on its files only)
mtime="$(git log -1 --format=%cI "$head")"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="$mtime" -C "$out" -cf - "$name" \
    | gzip -9n > "$archive"
hash="$(sha256sum "$archive" | cut -d' ' -f1)"
echo "$hash  $name.tar.gz" > "$archive.sha256"
echo "archive: $archive"
echo "files:   $(cd "$stage" && for f in *; do printf '%s (%s bytes), ' "$f" "$(stat -c %s "$f")"; done)"
echo "size:    $(stat -c %s "$archive") bytes"
echo "sha256:  $hash"
echo "debug:   $out/*.debug (not in the archive)"
