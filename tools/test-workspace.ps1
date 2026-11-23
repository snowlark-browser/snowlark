# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Runs every crate's tests once, the way a change is checked before it is merged, and prints a short summary.
#
#   .\tools\test-workspace.ps1                      every crate (`--workspace`)
#   .\tools\test-workspace.ps1 -Affected            the crates the branch changes (against its merge base with
#                                                   origin/main, or -Base <rev>) and every crate depending on them
#                                                   (-ListOnly: print them and stop)
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
    [switch]$Affected,
    [switch]$ListOnly,
    [string]$Base = '',
    [string]$Log = '',
    [string]$FromLog = ''
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot

# The crates a change can break: those whose files it touches, those whose vendored or IDL inputs it touches, and
# every crate that depends on one of them (normal, dev or build dependency, transitively). A change to the
# workspace's own build files counts as touching every crate.
function Get-AffectedCrates([string]$base) {
    $crates = @{}
    $deps = @{}
    foreach ($dir in Get-ChildItem (Join-Path $repo 'crates') -Directory) {
        $toml = Join-Path $dir.FullName 'Cargo.toml'
        if (-not (Test-Path $toml)) { continue }
        $text = Get-Content -Raw -Encoding utf8 $toml
        $name = if ($text -match '(?m)^name\s*=\s*"([^"]+)"') { $Matches[1] } else { $dir.Name }
        $crates[$dir.Name] = [pscustomobject]@{ Name = $name; Text = $text }
        $deps[$name] = @([regex]::Matches($text, 'path\s*=\s*"\.\./([^"/]+)"') | ForEach-Object { $_.Groups[1].Value })
    }
    $dirToName = @{}
    foreach ($k in $crates.Keys) { $dirToName[$k] = $crates[$k].Name }
    if (-not $base) {
        $base = (git -C $repo merge-base origin/main HEAD 2>$null)
        if (-not $base) { $base = 'origin/main' }
    }
    $files = @(git -C $repo diff --name-only $base) + @(git -C $repo ls-files --others --exclude-standard)
    $seeds = @{}
    $all = $false
    $why = New-Object System.Collections.Generic.List[string]
    # The WPT gate measures what pages observe in `snowlark --wpt`. Every crate that mode links and runs counts,
    # which is all of them but the window's browser half (`snowlark-shell`) and the IDL generator; so do the
    # vendored crates, the IDL, the build files and the WPT runner itself.
    $gateWhy = New-Object System.Collections.Generic.List[string]
    foreach ($f in ($files | Where-Object { $_ } | Sort-Object -Unique)) {
        if (($f -match '^crates/([^/]+)/' -and $Matches[1] -notin @('snowlark-shell', 'snowlark-idl')) -or
            $f -match '^(vendor/|idl/|tools/wpt/|Cargo\.toml|Cargo\.lock|rust-toolchain|\.cargo/|dev\.ps1)') {
            if ($gateWhy.Count -lt 3) { $gateWhy.Add($f) }
        }
        if ($f -match '^crates/([^/]+)/') {
            if ($dirToName.ContainsKey($Matches[1])) { $seeds[$dirToName[$Matches[1]]] = $true }
        } elseif ($f -match '^vendor/([^/]+)/') {
            $v = $Matches[1]
            $users = @($crates.Values | Where-Object { $_.Text -match "(?m)^$([regex]::Escape($v))\s*=" } | ForEach-Object { $_.Name })
            if ($users.Count -eq 0) { $all = $true; $why.Add("$f (no crate names $v)") } else { foreach ($u in $users) { $seeds[$u] = $true } }
        } elseif ($f -match '^idl/') {
            foreach ($u in 'snowlark-idl', 'snowlark-bindings') { if ($deps.ContainsKey($u)) { $seeds[$u] = $true } }
        } elseif ($f -match '^(Cargo\.toml|Cargo\.lock|rust-toolchain(\.toml)?|\.cargo/|dev\.ps1|build\.rs)') {
            $all = $true; $why.Add($f)
        }
        # Anything else (tools, docs, notes, benchmarks, Markdown) builds into no crate.
    }
    if ($all) { return [pscustomobject]@{ All = $true; Crates = @(); Why = $why; Files = $files.Count; Gate = $gateWhy } }
    $result = @{}
    $queue = New-Object System.Collections.Generic.Queue[string]
    foreach ($s in $seeds.Keys) { $result[$s] = $true; $queue.Enqueue($s) }
    while ($queue.Count -gt 0) {
        $c = $queue.Dequeue()
        foreach ($k in $crates.Keys) {
            $n = $crates[$k].Name
            if (-not $result.ContainsKey($n)) {
                $dependsOn = $deps[$n] | ForEach-Object { $dirToName[$_] }
                if ($dependsOn -contains $c) { $result[$n] = $true; $queue.Enqueue($n) }
            }
        }
    }
    $list = @($result.Keys | Sort-Object)
    return [pscustomobject]@{ All = ($list.Count -eq $crates.Count); Crates = $list; Why = $why; Files = $files.Count; Seeds = @($seeds.Keys | Sort-Object); Gate = $gateWhy }
}

if ($Affected -and -not $FromLog) {
    $a = Get-AffectedCrates $Base
    if ($a.Gate.Count -gt 0) {
        Write-Host "test-workspace: WPT gate needed (pages can observe: $($a.Gate -join ', ')$(if ($a.Gate.Count -ge 3) { ', ...' }))"
    } else {
        Write-Host 'test-workspace: WPT gate not needed (nothing `snowlark --wpt` runs has changed)'
    }
    if ($a.All) {
        Write-Host "test-workspace: the change reaches every crate$(if ($a.Why.Count) { ' (' + ($a.Why -join ', ') + ')' }); running all of them"
        $Packages = @()
    } elseif ($a.Crates.Count -eq 0) {
        Write-Host "test-workspace: the change ($($a.Files) files) touches no crate; nothing to test"
        exit 0
    } else {
        Write-Host "test-workspace: changed $($a.Seeds -join ', '); with their dependents: $($a.Crates -join ', ')"
        $Packages = $a.Crates
    }
    if ($ListOnly) { exit 0 }
}

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
