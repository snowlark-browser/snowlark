# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Runs proc-a with a time limit and prints what it said. Stops only its own process, by PID.
#   .\run.ps1 <name> <limit seconds> <proc-a arguments...>
$name = $args[0]
$limit = [int]$args[1]
$rest = @($args | Select-Object -Skip 2)
$t = $PSScriptRoot
$runs = Join-Path $t 'runs'
New-Item -ItemType Directory -Force $runs | Out-Null
$out = Join-Path $runs $name
$argv = @($rest) + @('--out', $out)
$p = Start-Process -FilePath (Join-Path $(if ($env:P0_BIN) { $env:P0_BIN } else { Join-Path $t 'target\release' }) 'proc-a.exe') -ArgumentList $argv -PassThru -NoNewWindow `
    -RedirectStandardOutput "$out.stdout" -RedirectStandardError "$out.stderr"
if (-not $p.WaitForExit($limit * 1000)) {
    Write-Output "TIMEOUT $name (pid $($p.Id))"
    Stop-Process -Id $p.Id -Force
}
Get-Content "$out.stdout" | Where-Object { $_ -notmatch 'first frame presented|generation \d+ started' }
Get-Content "$out.stderr" -Tail 5
