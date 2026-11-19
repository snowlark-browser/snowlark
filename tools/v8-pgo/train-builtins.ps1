# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# The builtins' profile (docs/v8-speed.md, 3c): a snowlark-cli built on a V8 library with
# `v8_enable_builtins_profiling=true` (build-v8.ps1 -GnArgs, build-cli.ps1) runs Speedometer 3.1 (2 iterations) and
# three sites, each process writing V8's basic block counts of the embedded builtins as its isolate is disposed
# (`--turbo-profiling-output`); the logs are joined (get_hints.py adds up the counts of a block seen twice) and
# V8's get_hints.py makes the branch hints and block counts mksnapshot reads (`v8_builtins_profiling_log_file`).
#   .\tools\v8-pgo\train-builtins.ps1 -Name bprof -Src <dev dir>\v8\rusty-v8-roll
# Writes <Work>\builtins.profile. Each isolate writes the file when it is disposed, the last one winning, so the
# script checks that each log holds the page's counts (a run's largest entry count), not a short-lived worker's.
param([string]$Work = '', [string]$Name = 'bprof', [string]$Src = '')
. (Join-Path $PSScriptRoot '..\devenv.ps1')
if (-not $Work) { $Work = Join-Path (Get-DevDir) 'v8-pgo' }
$srcDir = if ($Src) { $Src } else { Join-Path $Work 'rusty_v8' }
$cli = Join-Path $Work "bin\$Name\snowlark-cli.exe"
$out = Join-Path $Work 'builtins-train'
if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Force $out | Out-Null
$env:SNOWLARK_NO_ACTIVATE = '1'
$s = Get-Date
$runs = @(@{ n = 'speedo'; a = "--speedometer --iterations 2 --out `"$out\speedo`"" })
$i = 0
foreach ($u in 'https://en.wikipedia.org/wiki/Web_browser', 'https://www.youtube.com/watch?v=jNQXAC9IVRw', 'https://www.bbc.com/news') {
    $i++
    $runs += @{ n = "site$i"; a = "--render $u `"$out\site$i.png`"" }
}
$logs = @()
foreach ($r in $runs) {
    $log = Join-Path $out "$($r.n).bb"
    $env:SNOWLARK_V8_FLAGS = "--turbo-profiling-output=$log"
    cmd /c "`"$cli`" $($r.a) > `"$out\$($r.n).log`" 2>&1"
    $code = $LASTEXITCODE
    $max = 0
    if (Test-Path $log) {
        foreach ($l in [System.IO.File]::ReadLines($log)) {
            $f = $l.Split("`t")
            if ($f[0] -eq 'block' -and $f[2] -eq '0' -and [double]$f[3] -gt $max) { $max = [double]$f[3] }
        }
        $logs += $log
    }
    "$($r.n): exit $code, largest builtin entry count $max"
}
Remove-Item Env:SNOWLARK_V8_FLAGS
if (-not $logs) { throw "no builtins profile written (was V8 built with v8_enable_builtins_profiling=true?)" }
$all = Join-Path $out 'all.bb'
# V8 never closes the file, and the process ends without the C runtime's exit handlers, so each log loses its
# last buffer: keep only the builtins whose data is complete (a builtin's hash is its last line).
$w = [System.IO.StreamWriter]::new($all, $false, [System.Text.Encoding]::ASCII)
foreach ($log in $logs) {
    $done = @{}
    foreach ($l in [System.IO.File]::ReadLines($log)) {
        $f = $l.Split("`t")
        if ($f[0] -eq 'builtin_hash' -and $f.Count -ge 3) { $done[$f[1]] = $true }
    }
    foreach ($l in [System.IO.File]::ReadLines($log)) {
        $f = $l.Split("`t")
        if ($f.Count -ge 2 -and $done.ContainsKey($f[1])) { $w.Write($l + "`n") }
    }
}
$w.Close()
python (Join-Path $srcDir 'v8\tools\builtins-pgo\get_hints.py') $all (Join-Path $Work 'builtins.profile')
"$($logs.Count) logs joined into $Work\builtins.profile, $([math]::Round(((Get-Date) - $s).TotalMinutes, 1)) minutes"
