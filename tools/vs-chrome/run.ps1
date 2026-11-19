# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# One script in snowlark-cli --eval and in headless Chrome, with the ratio per test: where we lose to
# Chrome, by layer. js_* have no bindings, dom_* go through them.
#
#   .\tools\vs-chrome\run.ps1                                  # target\release\snowlark-cli.exe, 3 runs
#   .\tools\vs-chrome\run.ps1 -Snowlark <exe> -Runs 5
#
# bench.js must have no double quotes: PowerShell 5.1 strips them from a native program's argument.
# Chrome runs headless with a profile of its own, so it neither touches the user's nor takes focus.
param(
    [string]$Snowlark = "",
    [int]$Runs = 3,
    # (another script of the same shape: an expression that returns JSON of name -> ms)
    [string]$Script = "",
    [string]$Chrome = "C:\Program Files\Google\Chrome\Application\chrome.exe"
)
$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
if (-not $Snowlark) { $Snowlark = Join-Path $root "target\release\snowlark-cli.exe" }
if (-not $Script) { $Script = Join-Path $PSScriptRoot "bench.js" }
$js = Get-Content -Raw $Script
if ($js.Contains('"')) { throw "bench.js has a double quote" }
$tmp = Join-Path ([IO.Path]::GetTempPath()) "snowlark-vs-chrome"
New-Item -ItemType Directory -Force $tmp | Out-Null
$html = Join-Path $tmp "bench.html"
Set-Content -Encoding utf8 $html "<!doctype html><html><body><pre id=r></pre><script>document.getElementById('r').textContent =`n$js`n;</script></body></html>"
$url = "file:///" + ($html -replace '\\', '/')
"chrome " + (Get-Item $Chrome).VersionInfo.ProductVersion
$ro = @(); $rc = @()
for ($i = 0; $i -lt $Runs; $i++) {
    $ro += (& $Snowlark --eval $js 2>$null | Select-Object -Last 1 | ConvertFrom-Json)
    $out = Join-Path $tmp "out.txt"
    $p = Start-Process -FilePath $Chrome -PassThru -WindowStyle Hidden -RedirectStandardOutput $out -RedirectStandardError (Join-Path $tmp "err.txt") `
        -ArgumentList '--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check', "--user-data-dir=$tmp\profile", '--dump-dom', $url
    $p | Wait-Process -Timeout 180
    $m = [regex]::Match((Get-Content -Raw $out), '<pre id="r">(.*?)</pre>')
    $rc += ([System.Net.WebUtility]::HtmlDecode($m.Groups[1].Value) | ConvertFrom-Json)
}
$mid = [int][math]::Floor($Runs / 2)
foreach ($n in $ro[0].PSObject.Properties.Name) {
    $a = ($ro | ForEach-Object { $_.$n } | Sort-Object)[$mid]
    $b = ($rc | ForEach-Object { $_.$n } | Sort-Object)[$mid]
    "{0,-18} snowlark {1,8}  chrome {2,8}  ratio {3:N2}" -f $n, $a, $b, ($a / [math]::Max($b, 0.01))
}
