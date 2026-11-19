# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Local settings for the PowerShell tools, as tools/devenv.py reads them for the Python ones: the environment,
# then `dev.env` (KEY=VALUE lines) in the repository root or the main checkout, then a default.
# Dot-source it:  . (Join-Path $PSScriptRoot '..\devenv.ps1')

# ($PSScriptRoot is this file's folder, tools\, even when dot-sourced.)
$script:DevRepo = Split-Path -Parent $PSScriptRoot

function Get-DevSettingsFile {
    $own = Join-Path $script:DevRepo 'dev.env'
    if (Test-Path $own) { return $own }
    $common = git -C $script:DevRepo rev-parse --path-format=absolute --git-common-dir 2>$null
    if ($common) {
        $main = Join-Path (Split-Path -Parent $common) 'dev.env'
        if (Test-Path $main) { return $main }
    }
    return $null
}

function Get-DevSetting([string]$Name, [string]$Default = $null) {
    $value = [Environment]::GetEnvironmentVariable($Name, 'Process')
    if ($value) { return $value }
    $file = Get-DevSettingsFile
    if ($file) {
        foreach ($line in Get-Content $file) {
            if ($line -match '^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*?)\s*$' -and $matches[1] -eq $Name) { return $matches[2] }
        }
    }
    return $Default
}

# SNOWLARK_DEV_DIR: scratch space for build output, kept builds and tool results; `.dev` in the repository by default.
function Get-DevDir { Get-DevSetting 'SNOWLARK_DEV_DIR' (Join-Path $script:DevRepo '.dev') }
# SNOWLARK_SPEEDOMETER: a local copy of Speedometer 3.1; <dev dir>\speedometer-3.1 by default.
function Get-SpeedometerDir { Get-DevSetting 'SNOWLARK_SPEEDOMETER' (Join-Path (Get-DevDir) 'speedometer-3.1') }
# The crate cache dev.ps1 builds with: CARGO_HOME, else <SNOWLARK_DEV_DIR>\.cargo-home, else ~\.cargo.
function Get-CargoHome {
    if ($env:CARGO_HOME) { return $env:CARGO_HOME }
    $dev = Get-DevSetting 'SNOWLARK_DEV_DIR'
    if ($dev) { return Join-Path $dev '.cargo-home' }
    return Join-Path $env:USERPROFILE '.cargo'
}
