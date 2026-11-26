# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Checks from Windows that the crates free of platform code compile for Linux (docs/linux.md):
# `cargo check --target x86_64-unknown-linux-gnu` through dev.ps1.
#
#   .\tools\linux\check.ps1                     the crates that check today, with their tests
#   .\tools\linux\check.ps1 -p snowlark-layout  any other cargo arguments instead
#
# Needs the target once: `rustup target add x86_64-unknown-linux-gnu`. Build scripts compile C for
# the target (ring, zstd, mimalloc, ...) and Windows has no compiler for Linux; `cargo check` never
# links, so stub_cc.py stands in for the compiler and archiver and writes empty outputs. A build or a
# test run needs a Linux machine.

$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)

# The crates that check on Linux today; a crate joins when it does (docs/linux.md, "Where it stands").
$crates = @(
    'snowlark-dom', 'snowlark-idl', 'snowlark-audio', 'snowlark-idb', 'snowlark-style',
    'snowlark-net', 'snowlark-js', 'snowlark-bindings', 'snowlark-graphics', 'snowlark-paint',
    'snowlark-layout', 'snowlark-render', 'snowlark-ipc', 'snowlark-tab'
)

$python = (Get-Command python -ErrorAction Stop).Source
$stub = Join-Path $PSScriptRoot 'stub_cc.py'
$tools = Join-Path $root 'target\linux-check'
New-Item -ItemType Directory -Force $tools | Out-Null
$cc = Join-Path $tools 'cc.cmd'
$ar = Join-Path $tools 'ar.cmd'
Set-Content -Encoding ascii $cc "@`"$python`" `"$stub`" cc %*"
Set-Content -Encoding ascii $ar "@`"$python`" `"$stub`" ar %*"
$env:CC_x86_64_unknown_linux_gnu = $cc
$env:CXX_x86_64_unknown_linux_gnu = $cc
$env:AR_x86_64_unknown_linux_gnu = $ar
$pkg = Join-Path $tools 'pkg-config.cmd'
Set-Content -Encoding ascii $pkg "@`"$python`" `"$stub`" pkg-config %*"
$env:PKG_CONFIG_x86_64_unknown_linux_gnu = $pkg
$env:PKG_CONFIG_ALLOW_CROSS = '1'

$cargoArgs = if ($args.Count -gt 0) { @($args) } else { @($crates | ForEach-Object { '-p', $_ }) + '--all-targets' }
& (Join-Path $root 'dev.ps1') check --target x86_64-unknown-linux-gnu @cargoArgs
exit $LASTEXITCODE
