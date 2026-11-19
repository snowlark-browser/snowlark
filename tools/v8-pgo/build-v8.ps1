# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds the v8 crate's V8 from the checkout prepare.ps1 made, into a target directory of its own, through
# dev.ps1. About 22 minutes with 8 jobs on a fast desktop, 33 instrumented.
#   .\tools\v8-pgo\build-v8.ps1 -Name control
#   .\tools\v8-pgo\build-v8.ps1 -Name instrumented -GnArgs 'chrome_pgo_phase=1'
#   .\tools\v8-pgo\build-v8.ps1 -Name pgo -GnArgs 'chrome_pgo_phase=2 pgo_data_path="<work>/snowlark.profdata"'
#   .\tools\v8-pgo\build-v8.ps1 -Name pgo-lto -GnArgs 'chrome_pgo_phase=2 pgo_data_path="<work>/snowlark.profdata" use_thin_lto=true'
# The library and its bindings land in <Work>\libs\<Name>\ (rusty_v8.lib, src_binding.rs). A ThinLTO library holds
# LLVM bitcode: thinlto-native.py makes it a native one (docs/v8-speed.md, section 6).
# -Src names a checkout elsewhere, as for prepare.ps1.
param([string]$Work = '', [string]$Name, [string]$GnArgs = '', [string]$Src = '')
. (Join-Path $PSScriptRoot '..\devenv.ps1')
if (-not $Work) { $Work = Join-Path (Get-DevDir) 'v8-pgo' }
$repo = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$src = $(if ($Src) { $Src } else { Join-Path $Work 'rusty_v8' }).Replace('\', '/')
$target = Join-Path $Work "target-$Name"
$log = Join-Path $Work "build-$Name.log"
$env:CARGO_TARGET_DIR = $target
$env:V8_FROM_SOURCE = '1'
$env:SNOWLARK_V8_PREBUILT = '1'   # dev.ps1: no pinned library for a build from source
$env:PYTHON = 'python'
$env:PRINT_GN_ARGS = '1'
# The Visual Studio V8's build uses: SNOWLARK_VS, else the newest one with the C++ x64 tools.
$env:GYP_MSVS_OVERRIDE_PATH = Get-DevSetting 'SNOWLARK_VS' (& "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath)
# PartitionAlloc stays out: V8 builds it by default with a shared pointer compression cage when not told it is
# embedded, for its standalone shell, and its malloc shim then defines malloc, free and _msize in the library,
# which the C runtime defines too (the link fails, or the process's allocator silently changes). Off, as it is
# for embedders and without pointer compression.
$env:EXTRA_GN_ARGS = (@('v8_enable_partition_alloc=false') + @($GnArgs | Where-Object { $_ })) -join ' '
Set-Location $repo
$s = Get-Date
# `--config patch`: the workspace's v8 dependency taken from the checkout, without editing Cargo.toml (cargo
# rewrites Cargo.lock for it; it is put back below).
cmd /c "powershell -NoProfile -ExecutionPolicy Bypass -File .\dev.ps1 build --profile iter -p v8 --config patch.crates-io.v8.path='$src' > `"$log`" 2>&1"
$code = $LASTEXITCODE
git checkout -- Cargo.lock
if ($code -eq 0) {
    $out = Join-Path $Work "libs\$Name"
    New-Item -ItemType Directory -Force $out | Out-Null
    Copy-Item "$target\iter\gn_out\obj\rusty_v8.lib", "$target\iter\gn_out\src_binding.rs" $out
    if ($GnArgs -like '*chrome_pgo_phase=1*') {
        # The instrumented library needs the profile runtime at link time (build-cli.ps1 -Instrumented).
        Copy-Item (Get-ChildItem "$target\iter\clang\lib\clang\*\lib\windows\clang_rt.profile-x86_64.lib").FullName $out
    }
}
"exit $code, $([math]::Round(((Get-Date) - $s).TotalMinutes, 1)) minutes, log $log"
