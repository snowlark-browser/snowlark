#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Prints the `LAYOUTS` entries of crates/snowlark-media/src/linux/ffmpeg_layout.rs: layout.c compiled
# against each FFmpeg header folder given (a source tree, or /usr/include for the installed -dev
# packages), in the order given.
#
#   tools/ffmpeg/layout.sh /path/to/ffmpeg-6.1 /path/to/ffmpeg-7.1 /usr/include
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="$(mktemp -d)"
trap 'rm -rf "$out"' EXIT
for headers in "$@"; do
    cc -I "$headers" -o "$out/layout" "$here/layout.c"
    "$out/layout"
done
