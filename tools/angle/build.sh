#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds ANGLE's libEGL.so and libGLESv2.so for Linux (release) from the commit tools/angle/pin.txt names,
# from its official source, into $SNOWLARK_DEV_DIR/angle/<name>-linux: the Linux counterpart of build.ps1
# (docs/webgl.md, docs/linux.md). The back end is OpenGL through the system's EGL, with no window system: the
# WebGL service draws into pbuffers only, on a surfaceless EGL display.
#
#   SNOWLARK_DEV_DIR=<dir> tools/angle/build.sh [jobs]
#
# Needs git, Python 3 and curl; the source with its tools (the Linux sysroot and clang ANGLE builds with) goes to
# $SNOWLARK_DEV_DIR/angle/src, depot_tools to $SNOWLARK_DEV_DIR/angle/depot_tools.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
jobs="${1:-8}"
dev="${SNOWLARK_DEV_DIR:?SNOWLARK_DEV_DIR is not set}"
pin() { sed -n "s/^$1=//p" "$here/pin.txt" | tr -d '\r'; }
commit="$(pin commit)"
name="$(pin name)-linux"
root="$dev/angle"
src="$root/src"
tools="$root/depot_tools"
mkdir -p "$root"
[ -d "$tools" ] || git clone -q https://chromium.googlesource.com/chromium/tools/depot_tools.git "$tools"
export PATH="$tools:$PATH"
[ -d "$src" ] || git clone -q https://chromium.googlesource.com/angle/angle "$src"
git -C "$src" fetch -q origin
git -C "$src" checkout -q "$commit"
cd "$src"
python3 scripts/bootstrap.py
gclient sync --no-history --shallow --nohooks -j "$jobs"
# The last hook configures Siso (Chromium's remote build), which this ninja build does not use; on Linux it fails
# where the checkout's own depot_tools has no CIPD client for the platform.
if ! gclient runhooks; then
    echo "build.sh: a hook failed (the last one, Siso's configuration, is not needed: the build uses ninja)"
fi
mkdir -p out/Release
cat > out/Release/args.gn <<EOF
is_debug = false
is_component_build = false
target_cpu = "x64"
symbol_level = 1
dcheck_always_on = false
use_siso = false
angle_enable_gl = true
angle_use_x11 = false
angle_use_wayland = false
angle_enable_vulkan = false
angle_enable_null = false
angle_enable_swiftshader = false
angle_enable_wgpu = false
angle_build_tests = false
angle_has_frame_capture = false
EOF
gn gen out/Release
autoninja -j "$jobs" -C out/Release libEGL libGLESv2
dest="$root/$name"
mkdir -p "$dest"
for f in libEGL libGLESv2; do
    cp "out/Release/$f.so" "$dest/"
    hash="$(sha256sum "$dest/$f.so" | cut -d' ' -f1)"
    want="$(pin "$f-linux-sha256")"
    if [ "$hash" != "$want" ]; then
        echo "$f.so: SHA-256 $hash differs from pin.txt's ($want)"
    fi
done
echo "ANGLE $commit is in $dest"
