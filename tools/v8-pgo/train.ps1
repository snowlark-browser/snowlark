# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# The PGO training run: an instrumented snowlark-cli (build-cli.ps1 -Instrumented) over Speedometer 3.1 (2
# iterations), the micro benchmarks and three sites, then the counts merged into <Work>\snowlark.profdata with
# the llvm-profdata of V8's clang (Chromium's `coverage_tools` package, fetched on first use). About 2 minutes.
#   .\tools\v8-pgo\train.ps1 -Name instrumented
# -Src names the crate's checkout when it is not <Work>\rusty_v8 (as for prepare.ps1).
param([string]$Work = '', [string]$Name = 'instrumented', [string]$Src = '')
. (Join-Path $PSScriptRoot '..\devenv.ps1')
if (-not $Work) { $Work = Join-Path (Get-DevDir) 'v8-pgo' }
$srcDir = if ($Src) { $Src } else { Join-Path $Work 'rusty_v8' }
$repo = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$cli = Join-Path $Work "bin\$Name\snowlark-cli.exe"
$out = Join-Path $Work 'profile'
if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Force $out | Out-Null
$env:LLVM_PROFILE_FILE = "$out\v8-%p.profraw"
$env:SNOWLARK_NO_ACTIVATE = '1'
Set-Location $repo
$s = Get-Date
cmd /c "`"$cli`" --speedometer --iterations 2 --out $out\speedo > $out\speedo.log 2>&1"
"speedometer exit $LASTEXITCODE"
cmd /c "python tools/micro-bench/run.py --snowlark `"$cli`" > $out\micro.log 2>&1"
"micro exit $LASTEXITCODE"
$i = 0
foreach ($u in 'https://en.wikipedia.org/wiki/Web_browser', 'https://www.youtube.com/watch?v=jNQXAC9IVRw', 'https://www.bbc.com/news') {
    $i++
    cmd /c "`"$cli`" --render $u $out\site$i.png > $out\site$i.log 2>&1"
    "render $u exit $LASTEXITCODE"
}
Remove-Item Env:LLVM_PROFILE_FILE
$raw = (Get-ChildItem $out -Filter *.profraw | Where-Object Length -gt 0).FullName
if (-not $raw) { throw "no profile written (was the CLI built with -Instrumented?)" }
$tools = Join-Path $Work 'clang-cov'
$profdata = Join-Path $tools 'bin\llvm-profdata.exe'
if (-not (Test-Path $profdata)) {
    python (Join-Path $srcDir 'tools\clang\scripts\update.py') --package coverage_tools --output-dir $tools
}
& $profdata merge -o (Join-Path $Work 'snowlark.profdata') @raw
"$($raw.Count) profiles merged into $Work\snowlark.profdata, $([math]::Round(((Get-Date) - $s).TotalMinutes, 1)) minutes"
