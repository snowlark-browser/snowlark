#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds the v8 crate's V8 for Linux from a full checkout of the crate's repository, as
# tools/v8-pgo/build-v8.ps1 does on Windows: the workspace's `v8` dependency taken from the checkout, so the
# workspace's features (pointer compression) apply (docs/v8-build.md).
#
#   tools/linux/build-v8.sh <checkout> [<out dir>]
#
# <checkout> is the crate's repository at the commit vendor/v8/SNOWLARK-PATCHES.md names, with its submodules.
# The library and its bindings land in <out dir> (default: where tools/linux/cargo.sh looks for the pinned
# library) as librusty_v8.a and src_binding.rs, with their SHA-256 printed for tools/v8-pgo/pin.txt. The build's
# own output goes to <checkout>/../target-v8-<linux-name>. Jobs: CARGO_BUILD_JOBS (default 8); it runs at the
# lowest CPU priority.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
src="$(cd "$1" && pwd)"
name="$(sed -n 's/^linux-name=//p' "$root/tools/v8-pgo/pin.txt" | head -n 1 | tr -d '\r')"
if [ -n "${2:-}" ]; then
    out="$2"
elif [ -n "${SNOWLARK_DEV_DIR:-}" ]; then
    out="$SNOWLARK_DEV_DIR/v8/$name"
else
    out="$root/target/v8/$name"
fi
target="$(dirname "$src")/target-v8-$name"

# V8's code generators run `python` (gn's default script executable); some systems have only python3.
shim="$target/python-shim"
mkdir -p "$shim"
if ! command -v python >/dev/null 2>&1; then
    ln -sf "$(command -v python3)" "$shim/python"
fi
export PATH="$shim:$PATH"

export CARGO_TARGET_DIR="$target"
export V8_FROM_SOURCE=1
export PRINT_GN_ARGS=1
# As on Windows: PartitionAlloc stays out (V8 builds it, with a shared pointer compression cage and a malloc
# shim, only for its standalone shell). glib is for the browser's own code, not V8's.
export EXTRA_GN_ARGS="v8_enable_partition_alloc=false use_glib=false ${EXTRA_GN_ARGS:-}"

cd "$root"
start=$(date +%s)
code=0
# `--config patch`: the workspace's v8 dependency taken from the checkout without editing Cargo.toml (cargo
# rewrites Cargo.lock for it; it is put back below).
nice -n 19 cargo build --profile iter -j "${CARGO_BUILD_JOBS:-8}" -p v8 \
    --config "patch.crates-io.v8.path=\"$src\"" || code=$?
git checkout -q -- Cargo.lock
echo "build-v8.sh: exit $code, $(( ($(date +%s) - start) / 60 )) minutes"
if [ "$code" -ne 0 ]; then
    exit "$code"
fi
mkdir -p "$out"
cp "$target/iter/build/gn_out/obj/librusty_v8.a" "$target/iter/build/gn_out/src_binding.rs" "$out/"
echo "build-v8.sh: $out"
sha256sum "$out/librusty_v8.a"
