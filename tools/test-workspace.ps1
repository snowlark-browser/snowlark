# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Runs every crate's tests once, the way a change is checked before it is merged, and prints a short summary.
#
#   .\tools\test-workspace.ps1                      every crate (`--workspace`)
#   .\tools\test-workspace.ps1 -Packages a,b        only those crates
#   .\tools\test-workspace.ps1 -FromLog <file>      summarise an earlier run's log without running anything
#
# It runs `dev.ps1 test --profile iter --no-fail-fast`: the `iter` profile shares its build with the `iter` binary
# (debug builds have overflow checks that fail tests no optimised build fails), and `--no-fail-fast` runs every test
# binary even after one fails. The whole output goes to the log (default `target\test-workspace.log`); the console
# shows only the summary. Failures listed in `tools/test-workspace-known.txt` are marked known.
#
# Exit code: 0 when nothing failed or every failure is known, 1 when a failure is not known, 2 when the build failed.

param(
    [string[]]$Packages = @(),
    [string]$Log = '',
    [string]$FromLog = ''
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot

if ($FromLog) {
    $Log = $FromLog
} else {
    if (-not $Log) { $Log = Join-Path $repo 'target\test-workspace.log' }
    $logDir = Split-Path -Parent $Log
    if ($logDir -and -not (Test-Path $logDir)) { New-Item -ItemType Directory -Force $logDir | Out-Null }
    $cargoArgs = @('test', '--profile', 'iter', '--no-fail-fast')
    if ($Packages.Count -gt 0) {
        foreach ($p in $Packages) { $cargoArgs += @('-p', $p) }
    } else {
        $cargoArgs += '--workspace'
    }
    $started = Get-Date
    Write-Host "test-workspace: dev.ps1 $($cargoArgs -join ' ') > $Log"
    # Native output through cmd keeps each line whole (PowerShell wraps redirected stderr lines).
    & cmd /c "powershell -NoProfile -ExecutionPolicy Bypass -File `"$repo\dev.ps1`" $($cargoArgs -join ' ') > `"$Log`" 2>&1"
    $elapsed = [int]((Get-Date) - $started).TotalSeconds
    Write-Host "test-workspace: finished in $elapsed s"
}

if (-not (Test-Path $Log)) { Write-Host "test-workspace: no log at $Log"; exit 2 }
$lines = Get-Content -Encoding utf8 $Log

# Known failures: one per line, `<binary>|<failure>|<why>`, matched as regular expressions against the binary's
# name (`<crate> lib`, `<crate> it`, ...) and the failure (a test's name, or `exit code N`).
$known = @()
$knownFile = Join-Path $PSScriptRoot 'test-workspace-known.txt'
if (Test-Path $knownFile) {
    foreach ($k in Get-Content -Encoding utf8 $knownFile) {
        if (-not $k.Trim() -or $k.TrimStart().StartsWith('#')) { continue }
        $parts = $k.Split('|')
        if ($parts.Count -ge 2) { $known += [pscustomobject]@{ Binary = $parts[0].Trim(); Failure = $parts[1].Trim(); Why = ($parts[2..($parts.Count - 1)] -join '|').Trim() } }
    }
}

function Get-BinaryName([string]$what, [string]$exe) {
    # `unittests src\lib.rs (target\iter\deps\snowlark_page-0123abcd.exe)` -> `snowlark-page lib`
    $crate = ([IO.Path]::GetFileNameWithoutExtension($exe) -replace '-[0-9a-f]{16}$', '') -replace '_', '-'
    if ($what -match '^unittests src[\\/]lib\.rs') { return "$crate lib" }
    if ($what -match '^unittests src[\\/](?:bin[\\/])?(.+)\.rs') { return "$crate bin $($Matches[1])" }
    if ($what -match '^tests[\\/](.+)\.rs') { return "(crate not named) $($Matches[1])" }
    return "$crate $what"
}

$binaries = [ordered]@{}
$byExe = @{}
$current = $null
$buildFailed = $false
foreach ($line in $lines) {
    if ($line -match '^\s*Running (.+?) \((.+)\)\s*$') {
        $exe = $Matches[2]
        $name = Get-BinaryName $Matches[1] $exe
        $current = [pscustomobject]@{ Name = $name; Exe = $exe; Result = $null; Passed = 0; Failed = 0; Ignored = 0; Tests = New-Object System.Collections.Generic.List[string]; Exit = $null }
        $binaries[[IO.Path]::GetFileName($exe)] = $current
        $byExe[[IO.Path]::GetFileName($exe)] = $current
        continue
    }
    # An integration test binary is named after its target (`it`), not its crate; a failure's rerun hint names both.
    if ($current -and $line -match 'to rerun pass `-p (\S+) (?:--lib|--test (\S+)|--bin (\S+))`') {
        $current.Name = if ($Matches[2]) { "$($Matches[1]) $($Matches[2])" } elseif ($Matches[3]) { "$($Matches[1]) bin $($Matches[3])" } else { "$($Matches[1]) lib" }
        continue
    }
    if ($line -match '^\s*Doc-tests ') { $current = $null; continue }
    if ($current -and $line -match '^test (\S+) \.\.\. FAILED') { $current.Tests.Add($Matches[1]); continue }
    if ($current -and $line -match '^test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored') {
        $current.Result = $Matches[1]; $current.Passed = [int]$Matches[2]; $current.Failed = [int]$Matches[3]; $current.Ignored = [int]$Matches[4]
        continue
    }
    if ($line -match "process didn't exit successfully: ``([^``]+)``.*\((?:exit code|exit status): ([^,)]+)") {
        $exeName = [IO.Path]::GetFileName(($Matches[1] -split ' ')[0])
        if ($byExe.ContainsKey($exeName)) { $byExe[$exeName].Exit = $Matches[2] }
        continue
    }
    if ($line -match '^error(\[E\d+\])?: could not compile|^error\[E\d+\]') { $buildFailed = $true }
}

function Find-Known([string]$binary, [string]$failure) {
    foreach ($k in $known) {
        if ($binary -match $k.Binary -and $failure -match $k.Failure) { return $k }
    }
    return $null
}

$passed = 0; $failedTests = 0; $ignored = 0
$report = New-Object System.Collections.Generic.List[string]
$newFailures = 0
foreach ($b in $binaries.Values) {
    $passed += $b.Passed; $failedTests += $b.Failed; $ignored += $b.Ignored
    $failures = @()
    foreach ($t in $b.Tests) { $failures += $t }
    if ($null -eq $b.Result) {
        $failures += $(if ($b.Exit) { "exit code $($b.Exit) before its result line" } else { 'no result line (did not finish)' })
    } elseif ($b.Exit -and $b.Exit -ne '0' -and $b.Failed -eq 0) {
        $failures += "exit code $($b.Exit) after $($b.Passed) passed"
    }
    foreach ($f in $failures) {
        $k = Find-Known $b.Name $f
        if ($k) {
            $report.Add("  known  $($b.Name): $f ($($k.Why))")
        } else {
            $report.Add("  NEW    $($b.Name): $f")
            $newFailures++
        }
    }
}

Write-Host ("test-workspace: {0} binaries, {1} passed, {2} failed, {3} ignored" -f $binaries.Count, $passed, $failedTests, $ignored)
foreach ($r in $report) { Write-Host $r }
Write-Host "test-workspace: full log $Log"
if ($buildFailed) {
    Write-Host 'test-workspace: BUILD FAILED (search the log for "error")'
    exit 2
}
if ($binaries.Count -eq 0) {
    Write-Host 'test-workspace: no test binary ran (see the log)'
    exit 2
}
if ($newFailures -gt 0) {
    Write-Host "test-workspace: $newFailures failure(s) not known: look at them before merging"
    exit 1
}
Write-Host $(if ($report.Count -gt 0) { 'test-workspace: OK (only known failures)' } else { 'test-workspace: OK' })
exit 0
