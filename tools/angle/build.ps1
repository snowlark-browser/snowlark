# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds ANGLE's libEGL.dll and libGLESv2.dll (Direct3D 11 back end only, release) from the commit
# tools\angle\pin.txt names, from its official source, into <SNOWLARK_DEV_DIR>\angle\<name>. See docs/webgl.md.
#
#   .\tools\angle\build.ps1 [-Jobs 8] [-VisualStudio <path of a Visual Studio 2022 installation>]
#
# Needs git, Python 3 and Visual Studio 2022 with the C++ tools and a Windows 10/11 SDK. The source with its
# tools (about 14 GB) goes to <dev dir>\angle\src, depot_tools to <dev dir>\angle\depot_tools.

param([int]$Jobs = 8, [string]$VisualStudio = $null)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot '..\devenv.ps1')
$devDir = Get-DevSetting 'SNOWLARK_DEV_DIR'
if (-not $devDir) { throw 'SNOWLARK_DEV_DIR is not set (dev.env)' }
$pin = @{}
foreach ($line in Get-Content (Join-Path $PSScriptRoot 'pin.txt')) {
    if ($line -match '^([A-Za-z0-9-]+)=(.+)$') { $pin[$matches[1]] = $matches[2].Trim() }
}
$root = Join-Path $devDir 'angle'
$src = Join-Path $root 'src'
$tools = Join-Path $root 'depot_tools'
New-Item -ItemType Directory -Force $root | Out-Null
$env:DEPOT_TOOLS_WIN_TOOLCHAIN = '0'
if ($VisualStudio) {
    $env:GYP_MSVS_OVERRIDE_PATH = $VisualStudio
    $env:vs2022_install = $VisualStudio
}
if (-not (Test-Path $tools)) { git clone -q https://chromium.googlesource.com/chromium/tools/depot_tools.git $tools }
$env:PATH = "$tools;" + $env:PATH
if (-not (Test-Path $src)) { git clone -q https://chromium.googlesource.com/angle/angle $src }
git -C $src fetch -q origin
git -C $src checkout -q $pin['commit']
Push-Location $src
try {
    python scripts\bootstrap.py
    & (Join-Path $tools 'gclient.bat') sync --no-history --shallow -j $Jobs
    # The toolchain files name the newest Windows SDK and its version macro; build with the newest SDK installed
    # here instead.
    $sdk = (Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\Include' -Directory |
        Where-Object { Test-Path (Join-Path $_.FullName 'um') } | Sort-Object Name | Select-Object -Last 1).Name
    foreach ($f in 'build\vs_toolchain.py', 'build\toolchain\win\setup_toolchain.py') {
        $t = [IO.File]::ReadAllText((Join-Path $src $f))
        [IO.File]::WriteAllText((Join-Path $src $f), [regex]::Replace($t, "SDK_VERSION = '10\.0\.\d+\.0'", "SDK_VERSION = '$sdk'"))
    }
    $ddk = Select-String -Path "C:\Program Files (x86)\Windows Kits\10\Include\$sdk\shared\sdkddkver.h" -Pattern '#define WDK_NTDDI_VERSION\s+(\w+)'
    if ($ddk) {
        $f = Join-Path $src 'build\config\win\BUILD.gn'
        $t = [IO.File]::ReadAllText($f)
        [IO.File]::WriteAllText($f, [regex]::Replace($t, '"NTDDI_VERSION=NTDDI_WIN1\w+"', "`"NTDDI_VERSION=$($ddk.Matches[0].Groups[1].Value)`""))
    }
    $out = Join-Path $src 'out\Release'
    New-Item -ItemType Directory -Force $out | Out-Null
    $gnArgs = @(
        'is_debug = false', 'is_component_build = false', 'target_cpu = "x64"', 'symbol_level = 1',
        'dcheck_always_on = false', 'angle_enable_vulkan = false', 'angle_enable_gl = false',
        'angle_enable_d3d9 = false', 'angle_enable_null = false', 'angle_enable_swiftshader = false',
        'angle_enable_wgpu = false', 'angle_build_tests = false', 'angle_has_frame_capture = false',
        'angle_enable_metal = false')
    [IO.File]::WriteAllText((Join-Path $out 'args.gn'), ($gnArgs -join "`n") + "`n")
    & (Join-Path $tools 'gn.bat') gen out/Release
    & (Join-Path $tools 'autoninja.bat') -j $Jobs -C out/Release libEGL libGLESv2
    if ($LASTEXITCODE -ne 0) { throw "the ANGLE build failed ($LASTEXITCODE)" }
} finally { Pop-Location }
$dest = Join-Path $root $pin['name']
New-Item -ItemType Directory -Force $dest | Out-Null
foreach ($f in 'libEGL', 'libGLESv2') {
    Copy-Item (Join-Path $src "out\Release\$f.dll") $dest
    $hash = (Get-FileHash (Join-Path $dest "$f.dll") -Algorithm SHA256).Hash.ToLower()
    if ($hash -ne $pin["$f-sha256"]) { Write-Host "$f.dll: SHA-256 $hash differs from pin.txt's (a different toolchain or SDK)" }
}
Write-Host "ANGLE $($pin['commit']) is in $dest"
