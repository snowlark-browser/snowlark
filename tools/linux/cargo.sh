#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# cargo on Linux with the pinned V8 library, as dev.ps1 is on Windows (docs/v8-build.md, docs/linux.md).
#
#   tools/linux/cargo.sh test --profile iter -p snowlark-js
#
# The library is the one tools/v8-pgo/pin.txt names for Linux (`linux-name`, `linux-lib-sha256`): from
# SNOWLARK_V8_DIR, else <SNOWLARK_DEV_DIR>/v8/<linux-name>, else target/v8/<linux-name> in the checkout, a
# folder that holds librusty_v8.a and src_binding.rs (tools/linux/build-v8.sh makes them). A RUSTY_V8_ARCHIVE or
# V8_FROM_SOURCE already in the environment is left alone. Build output follows CARGO_TARGET_DIR as usual.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

pin_value() {
    sed -n "s/^$1=//p" "$root/tools/v8-pgo/pin.txt" | head -n 1 | tr -d '\r'
}

if [ -z "${RUSTY_V8_ARCHIVE:-}" ] && [ -z "${V8_FROM_SOURCE:-}" ]; then
    name="$(pin_value linux-name)"
    crate="$(pin_value crate)"
    locked="$(sed -n '/^name = "v8"$/{n;s/^version = "\(.*\)"$/\1/p}' "$root/Cargo.lock" | head -n 1)"
    if [ "$locked" != "$crate" ]; then
        echo "cargo.sh: Cargo.lock has v8 $locked but the pinned V8 library is for $crate; leaving V8 to the v8 crate" >&2
    else
        if [ -n "${SNOWLARK_V8_DIR:-}" ]; then
            dir="$SNOWLARK_V8_DIR"
        elif [ -n "${SNOWLARK_DEV_DIR:-}" ]; then
            dir="$SNOWLARK_DEV_DIR/v8/$name"
        else
            dir="$root/target/v8/$name"
        fi
        if [ ! -f "$dir/librusty_v8.a" ] || [ ! -f "$dir/src_binding.rs" ]; then
            echo "cargo.sh: no V8 library in $dir; build it with tools/linux/build-v8.sh (docs/v8-build.md)" >&2
            exit 1
        fi
        export RUSTY_V8_ARCHIVE="$dir/librusty_v8.a"
        export RUSTY_V8_ARCHIVE_SHA256="$(pin_value linux-lib-sha256)"
        export RUSTY_V8_SRC_BINDING_PATH="$dir/src_binding.rs"
    fi
fi

# ANGLE, WebGL's OpenGL ES (docs/webgl.md): the Linux build of the commit tools/angle/pin.txt names, in
# SNOWLARK_ANGLE_DIR or <SNOWLARK_DEV_DIR>/angle/<name>-linux (tools/angle/build.sh makes it). snowlark-gl's build
# script copies its libraries beside the executables; without them getContext("webgl") returns null.
if [ -z "${SNOWLARK_ANGLE_DIR:-}" ] && [ -n "${SNOWLARK_DEV_DIR:-}" ]; then
    angle="$SNOWLARK_DEV_DIR/angle/$(sed -n 's/^name=//p' "$root/tools/angle/pin.txt" | tr -d '\r')-linux"
    if [ -f "$angle/libGLESv2.so" ]; then
        export SNOWLARK_ANGLE_DIR="$angle"
    fi
fi

cd "$root"
exec cargo "$@"
