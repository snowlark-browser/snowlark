# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Checks out the v8 crate's sources at a release tag, for building V8 ourselves (docs/v8-speed.md, section 6),
# and makes them build on this machine. Safe to run again on an existing checkout.
#   .\tools\v8-pgo\prepare.ps1 -Tag v152.2.0
# -Src names a checkout elsewhere (the crate's fork a newer V8 is rolled in: -Src <dev dir>\v8\rusty-v8-roll).
param([string]$Work = '', [string]$Tag = 'v152.2.0', [string]$Src = '')
. (Join-Path $PSScriptRoot '..\devenv.ps1')
if (-not $Work) { $Work = Join-Path (Get-DevDir) 'v8-pgo' }
$ErrorActionPreference = 'Stop'
$src = if ($Src) { $Src } else { Join-Path $Work 'rusty_v8' }
New-Item -ItemType Directory -Force $Work | Out-Null
if (-not (Test-Path (Join-Path $src '.git'))) {
    # The crate on crates.io leaves out files a source build needs (ICU's data, vendored crates' build
    # scripts); the repository with its submodules has them.
    git clone --depth 1 --branch $Tag --recurse-submodules --shallow-submodules -j 8 `
        https://github.com/denoland/rusty_v8.git $src
    if ($LASTEXITCODE -ne 0) { throw "clone failed" }
}
# tools/win is a submodule with `update = none`: its mirror can name commits it does not serve, so the
# repository's own instructions take the pinned commit's archive.
$win = Join-Path $src 'tools\win'
if (-not (Test-Path (Join-Path $win 'DebugVisualizers\BUILD.gn'))) {
    $commit = (git -C $src ls-tree HEAD tools/win).Split()[2]
    $tgz = Join-Path $Work 'tools-win.tgz'
    Invoke-WebRequest -UseBasicParsing "https://chromium.googlesource.com/chromium/src/tools/win/+archive/$commit.tar.gz" -OutFile $tgz
    New-Item -ItemType Directory -Force $win | Out-Null
    tar -xzf $tgz -C $win
}
# The build pins Windows SDK 10.0.26100 and a Windows 11 24H2 target; a machine with SDK 10.0.22621 has as its
# newest target NTDDI_WIN10_NI. Neither changes V8's code (the SDK's headers only).
$sdk = (Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\include" -Directory | Sort-Object Name | Select-Object -Last 1).Name
function Edit-File($path, $from, $to) {
    $t = [IO.File]::ReadAllText($path)
    if ($t.Contains($from)) { [IO.File]::WriteAllText($path, $t.Replace($from, $to)) }
}
if ($sdk -ne '10.0.26100.0') {
    foreach ($f in 'build\toolchain\win\setup_toolchain.py', 'build\vs_toolchain.py') {
        Edit-File (Join-Path $src $f) "SDK_VERSION = '10.0.26100.0'" "SDK_VERSION = '$sdk'"
    }
    Edit-File (Join-Path $src 'build\config\win\BUILD.gn') '"NTDDI_VERSION=NTDDI_WIN11_GE"' '"NTDDI_VERSION=NTDDI_WIN10_NI"'
}
# With ThinLTO (`use_thin_lto=true`) the build also turns on whole-program devirtualization, which assumes every
# subclass of a V8 class is in the LTO unit. Ours are not all there: `snowlark-js/src/streaming.rs` lays out an
# `ExternalSourceStream` vtable by hand in Rust. So it stays off (only ThinLTO builds read this condition).
Edit-File (Join-Path $src 'build\config\compiler\BUILD.gn') 'if (!is_android || current_cpu != "arm") {' `
    'if (false && (!is_android || current_cpu != "arm"))  {  # snowlark: no whole-program devirtualization (Rust subclasses V8 classes outside the LTO unit)'
# The PGO configuration lists the browser's profile pin as an input even when `pgo_data_path` names ours.
$pin = Join-Path $src 'chrome\build\win64.pgo.txt'
if (-not (Test-Path $pin)) {
    New-Item -ItemType Directory -Force (Split-Path $pin) | Out-Null
    [IO.File]::WriteAllText($pin, "snowlark: pgo_data_path names the profile`n")
}
"ready: $src (SDK $sdk)"
