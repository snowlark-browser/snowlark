# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# One Speedometer run with leak and growth diagnostics, as one report.
#
#   .\tools\speedometer\run.ps1                    # all suites, 5 iterations
#   .\tools\speedometer\run.ps1 -Iterations 3 -Suites TodoMVC-jQuery,TodoMVC-Backbone
#   .\tools\speedometer\run.ps1 -Iterations 3 -Suites TodoMVC-Lit-Complex-DOM -Profile
#   .\tools\speedometer\run.ps1 -Cli <folder>\snowlark-cli.exe   # another build
#
# Runs `snowlark-cli --speedometer` (headless: Speedometer's own runner, results and engine counts at
# each suite boundary, the removed frames still alive after the run), then, when it wrote a heap
# snapshot (frames stayed), the holders of each Window from tools/heap-retainers. Everything ends up
# in <Out>\report.txt; paste that file. Build first: .\dev.ps1 build --release
#
# Speedometer comes from the local mirror (-Mirror, default SNOWLARK_SPEEDOMETER or <dev dir>\speedometer-3.1),
# served on a free port of 127.0.0.1 for the run: a run that loads it from browserbench.org waits on the
# internet, and one fetch that stalls there leaves a suite without its page (the runner then waits for
# it forever; snowlark-cli stops a run after two minutes without a step). -Url runs another copy.
#
# -Profile runs with SNOWLARK_PROFILE=1 (slower: no Fast API, a line per rendering update) and adds to the
# report the rendering updates summed per document (tools/speedometer/render_summary.py: restyled,
# built, laid out against reused, style/build/layout ms) and the page's JS, native and binding
# profiles. Profile for where time goes, not for times.
param(
    [int]$Iterations = 5,
    # (several names: -Suites A,B is an array, joined with commas for the runner)
    [string[]]$Suites = @(),
    [string]$Out = "",
    [switch]$Snapshot,
    [switch]$Profile,
    [string]$Mirror = "",
    [string]$Url = "",
    # (the build to run; default: this checkout's release build)
    [string]$Cli = ""
)
. (Join-Path $PSScriptRoot '..\devenv.ps1')
if (-not $Mirror) { $Mirror = Get-SpeedometerDir }
$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
if (-not $Out) { $Out = Join-Path $root "speedometer-report" }
$cli = if ($Cli) { $Cli } else { Join-Path $root "target\release\snowlark-cli.exe" }
if (-not (Test-Path $cli)) { throw "build first: $root\dev.ps1 build --release ($cli is missing)" }
New-Item -ItemType Directory -Force -Path $Out | Out-Null
Remove-Item -ErrorAction SilentlyContinue (Join-Path $Out "leaks.heapsnapshot")

$cliArgs = @("--speedometer", "--iterations", "$Iterations", "--out", "$Out")
if ($Suites.Count -gt 0) { $cliArgs += @("--suites", ($Suites -join ",")) }
if ($Snapshot) { $cliArgs += "--snapshot" }
$server = $null
if (-not $Url) {
    if (-not (Test-Path (Join-Path $Mirror "index.html"))) { throw "no Speedometer mirror at $Mirror (pass -Mirror or -Url)" }
    $probe = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $probe.Start(); $port = $probe.LocalEndpoint.Port; $probe.Stop()
    $serve = Join-Path $root "tools\speedometer\mirror_server.py"
    $server = Start-Process -FilePath python -ArgumentList @("`"$serve`"", "$port", "--mirror", "`"$Mirror`"") `
        -WindowStyle Hidden -PassThru -RedirectStandardError (Join-Path $Out "server.txt") -RedirectStandardOutput (Join-Path $Out "server-out.txt")
    $deadline = (Get-Date).AddSeconds(15)
    while (-not (Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue)) {
        if ((Get-Date) -gt $deadline -or $server.HasExited) { throw "the mirror's server did not start (see $Out\server.txt)" }
        Start-Sleep -Milliseconds 200
    }
    $Url = "http://127.0.0.1:$port/"
}
$cliArgs += @("--url", $Url)
Write-Host "snowlark-cli $($cliArgs -join ' ')"
$log = Join-Path $Out "stderr.txt"
$stdout = Join-Path $Out "stdout.txt"
$saved = $env:SNOWLARK_GC_STATS
$savedProfile = $env:SNOWLARK_PROFILE
try {
    # (the engine's [gc] and progress lines go to stderr.txt; the report has what matters)
    $env:SNOWLARK_GC_STATS = "1"
    if ($Profile) { $env:SNOWLARK_PROFILE = "1" }
    $quoted = $cliArgs | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }
    $p = Start-Process -FilePath $cli -ArgumentList $quoted -NoNewWindow -Wait -PassThru `
        -RedirectStandardError $log -RedirectStandardOutput $stdout
    if ($p.ExitCode -ne 0) { Write-Host "snowlark-cli exited with $($p.ExitCode) (see $log)" }
} finally {
    $env:SNOWLARK_GC_STATS = $saved
    $env:SNOWLARK_PROFILE = $savedProfile
    if ($server -and -not $server.HasExited) { Stop-Process -Id $server.Id -Force }
}

$report = Join-Path $Out "report.txt"
$snap = Join-Path $Out "leaks.heapsnapshot"
if (Test-Path $snap) {
    Write-Host "analysing $snap ..."
    $holders = & python (Join-Path $root "tools\heap-retainers\retainers.py") $snap --holders --max 2 --depth 14 2>&1
    Add-Content -Path $report -Value "`nheap snapshot holders (tools/heap-retainers --holders):"
    Add-Content -Path $report -Value ($holders | Out-String)
}
if ($Profile) {
    Write-Host "summarizing the profile ..."
    $summary = & python (Join-Path $root "tools\speedometer\render_summary.py") $log 2>&1
    Add-Content -Path $report -Value "`nrendering updates (SNOWLARK_PROFILE=1):"
    Add-Content -Path $report -Value ($summary | Out-String)
    Add-Content -Path $report -Value "`nprofiles (the page's, as it closed):"
    # (printed as the page closes, after everything else: from its first line on)
    $lines = Get-Content -Path $log
    $first = ($lines | Select-String -Pattern '^\[profile\]' | Select-Object -First 1).LineNumber
    if ($first) {
        Add-Content -Path $report -Value (($lines[($first - 1)..($lines.Count - 1)] | Where-Object { $_ -notmatch '^\[(render|gc|speedometer)\]' }) | Out-String)
    }
}
Write-Host "`nreport: $report"
