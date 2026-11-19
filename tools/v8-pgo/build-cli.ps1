# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds snowlark-cli against a V8 library build-v8.ps1 made (<Work>\libs\<Name>), into <Work>\bin\<Name>\.
#   .\tools\v8-pgo\build-cli.ps1 -Name control                       # release, for timing
#   .\tools\v8-pgo\build-cli.ps1 -Name instrumented -Instrumented    # iter, writes a profile on exit
param([string]$Work = '', [string]$Name, [switch]$Instrumented)
. (Join-Path $PSScriptRoot '..\devenv.ps1')
if (-not $Work) { $Work = Join-Path (Get-DevDir) 'v8-pgo' }
$repo = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$lib = Join-Path $Work "libs\$Name"
$prof = if ($Instrumented) { 'iter' } else { 'release' }
$target = Join-Path $Work "cli-$prof"
$env:CARGO_TARGET_DIR = $target
$env:RUSTY_V8_ARCHIVE = "$lib\rusty_v8.lib"
$env:RUSTY_V8_SRC_BINDING_PATH = "$lib\src_binding.rs"
$features = ''
if ($Instrumented) {
    # The profile runtime from the library's folder; RUSTFLAGS replaces .cargo/config.toml's, so its
    # crt-static is repeated. `pgo-train` writes the profile before the process exits.
    $env:RUSTFLAGS = "-C target-feature=+crt-static -L native=$lib"
    $features = '--features pgo-train'
}
Set-Location $repo
$s = Get-Date
$log = Join-Path $Work "cli-$Name.log"
cmd /c "powershell -NoProfile -ExecutionPolicy Bypass -File .\dev.ps1 build --profile $prof -p snowlark --bin snowlark-cli $features > `"$log`" 2>&1"
$code = $LASTEXITCODE
if ($code -eq 0) {
    $bin = Join-Path $Work "bin\$Name"
    New-Item -ItemType Directory -Force $bin | Out-Null
    Copy-Item "$target\$prof\snowlark-cli.exe", "$target\$prof\snowlark_cli.pdb" $bin
}
"exit $code, $([math]::Round(((Get-Date) - $s).TotalMinutes, 1)) minutes, log $log"
