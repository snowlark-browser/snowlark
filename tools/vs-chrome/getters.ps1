# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# getters.js in several snowlark-cli builds and in headless Chrome, alternating, with the median ns per
# access of each test. Builds are NAME=path pairs:
#
#   .\tools\vs-chrome\getters.ps1 -Runs 7 base=<folder>\snowlark-cli.exe new=target\iter\snowlark-cli.exe
#
# Ask for an otherwise idle machine first: these are nanoseconds, and load moves them 10-20%.
# Chrome runs headless with a profile of its own, so it neither touches the user's nor takes focus.
[CmdletBinding(PositionalBinding = $false)]
param(
    [int]$Runs = 5,
    [string]$Chrome = "C:\Program Files\Google\Chrome\Application\chrome.exe",
    [switch]$NoChrome,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$Arms
)
$ErrorActionPreference = "Stop"
$js = Get-Content -Raw (Join-Path $PSScriptRoot "getters.js")
if ($js.Contains('"')) { throw "getters.js has a double quote" }
$tmp = Join-Path ([IO.Path]::GetTempPath()) "snowlark-vs-chrome-getters"
New-Item -ItemType Directory -Force $tmp | Out-Null
$html = Join-Path $tmp "getters.html"
Set-Content -Encoding utf8 $html "<!doctype html><html><body><pre id=r></pre><script>document.getElementById('r').textContent =`n$js`n;</script></body></html>"
$url = "file:///" + ($html -replace '\\', '/')
$builds = [ordered]@{}
foreach ($a in $Arms) {
    $k, $v = $a -split '=', 2
    $builds[$k] = (Resolve-Path $v).Path
}
if (-not $NoChrome) { $builds['chrome'] = $Chrome }
$results = @{}
foreach ($k in $builds.Keys) { $results[$k] = @() }
for ($i = 0; $i -lt $Runs; $i++) {
    foreach ($k in $builds.Keys) {
        if ($k -eq 'chrome') {
            $out = Join-Path $tmp "out.txt"
            $p = Start-Process -FilePath $Chrome -PassThru -WindowStyle Hidden -RedirectStandardOutput $out -RedirectStandardError (Join-Path $tmp "err.txt") `
                -ArgumentList '--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check', "--user-data-dir=$tmp\profile", '--dump-dom', $url
            $p | Wait-Process -Timeout 180
            $m = [regex]::Match((Get-Content -Raw $out), '<pre id="r">(.*?)</pre>')
            $results[$k] += ([System.Net.WebUtility]::HtmlDecode($m.Groups[1].Value) | ConvertFrom-Json)
        } else {
            $results[$k] += (& $builds[$k] --eval $js 2>$null | Select-Object -Last 1 | ConvertFrom-Json)
        }
    }
}
$mid = [int][math]::Floor($Runs / 2)
$first = @($builds.Keys)[0]
$names = $results[$first][0].PSObject.Properties.Name | Where-Object { $_ -ne 'sink' }
"{0,-17}" -f "ns/access" + (($builds.Keys | ForEach-Object { "{0,10}" -f $_ }) -join '')
foreach ($n in $names) {
    $row = "{0,-17}" -f $n
    foreach ($k in $builds.Keys) {
        $v = [double](($results[$k] | ForEach-Object { [double]$_.$n } | Sort-Object)[$mid])
        $row += "{0,10}" -f $v.ToString("0.00", [cultureinfo]::InvariantCulture)
    }
    $row
}
