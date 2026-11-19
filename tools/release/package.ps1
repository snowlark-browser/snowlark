# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds the preview release zip of the browser (shipped as Snowlark):
#
#     .\tools\release\package.ps1 [-OutDir <dev dir>\release] [-SkipBuild] [-AllowOther]
#
# From a clean checkout at the newest origin/main it makes a `--release` build, then packs
#     snowlark.exe               target\release\snowlark.exe (no PDB, no snowlark-cli)
#     THIRD-PARTY-NOTICES.txt    tools\release\notices.py
#     README.txt                 tools\release\README.txt with the build's date and revision
# into <OutDir>\snowlark-preview-<date>-<sha>.zip and prints its size and SHA-256.
#
# -SkipBuild packs the release build that is already there (it must be of this revision: the
# script reads the revision the executable was built from and stops on another one).
# -AllowOther packs a checkout that is not the newest origin/main, or not clean (for trying the
# script out; never for a zip that is published).
#
# The MPL-2.0 source that goes with a release is exported by tools\release\stylo_source.py.
param(
    [string]$OutDir = '',
    [switch]$SkipBuild,
    [switch]$AllowOther
)
. (Join-Path $PSScriptRoot '..\devenv.ps1')
if (-not $OutDir) { $OutDir = Join-Path (Get-DevDir) 'release' }
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
Set-Location $root

$ErrorActionPreference = 'Continue'
git fetch --quiet origin
$ErrorActionPreference = 'Stop'
$head = (git rev-parse HEAD).Trim()
$main = (git rev-parse origin/main).Trim()
$dirty = git status --porcelain --untracked-files=no
if (-not $AllowOther) {
    if ($head -ne $main) { throw "HEAD ($head) is not the newest origin/main ($main)" }
    if ($dirty) { throw "the checkout has uncommitted changes:`n$($dirty -join "`n")" }
}
$sha = $head.Substring(0, 8)
$date = (Get-Date).ToUniversalTime().ToString('yyyy-MM-dd')

if (-not $SkipBuild) {
    $drive = ([IO.Path]::GetPathRoot([IO.Path]::GetFullPath($OutDir))).TrimEnd('\', ':')
    $free = [math]::Round((Get-PSDrive $drive).Free / 1GB)
    if ($free -lt 35) { throw "$($drive): has $free GB free; a release build needs about 35 GB" }
    # (cargo writes its progress to stderr, which Windows PowerShell turns into errors when the output is
    # redirected: they must not stop the script)
    $ErrorActionPreference = 'Continue'
    & "$root\dev.ps1" build --release
    $code = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($code -ne 0) { throw "the release build failed ($code)" }
}
$exe = Join-Path $root 'target\release\snowlark.exe'
if (-not (Test-Path $exe)) { throw "no $exe" }
$product = (Get-Item $exe).VersionInfo.ProductName
if ($product -ne 'Snowlark') { throw "$exe has no Snowlark version resource (ProductName '$product')" }
# The executable carries the revision it was built from (the About page shows it).
$bytes = [IO.File]::ReadAllBytes($exe)
$text = [Text.Encoding]::ASCII.GetString($bytes)
$short = $head.Substring(0, 7)
if (-not $text.Contains($short) -and -not $AllowOther) {
    throw "$exe was not built from $short (rebuild without -SkipBuild)"
}
$bytes = $null; $text = $null

$name = "snowlark-preview-$date-$sha"
$stage = Join-Path $OutDir $name
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item $exe (Join-Path $stage 'snowlark.exe')

# Code signing, when SNOWLARK_SIGN_CERT names a code-signing certificate (its SHA-1 thumbprint) in the
# user's certificate store; a cloud or smart-card key may ask its owner to approve. The signature is
# SHA-256 with an RFC 3161 timestamp (SNOWLARK_SIGN_TIMESTAMP, Certum's by default), and is verified.
$signCert = Get-DevSetting 'SNOWLARK_SIGN_CERT'
if ($signCert) {
    $timestamp = Get-DevSetting 'SNOWLARK_SIGN_TIMESTAMP' 'http://time.certum.pl'
    $signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\x64\\' } | Sort-Object FullName | Select-Object -Last 1
    if (-not $signtool) { throw 'signtool.exe not found (install the Windows SDK)' }
    $signed = Join-Path $stage 'snowlark.exe'
    $ErrorActionPreference = 'Continue'
    & $signtool.FullName sign /sha1 $signCert /fd sha256 /tr $timestamp /td sha256 /d 'Snowlark' /du 'https://snowlark.org' $signed
    $code = $LASTEXITCODE
    if ($code -eq 0) { & $signtool.FullName verify /pa $signed | Out-Null; $code = $LASTEXITCODE }
    $ErrorActionPreference = 'Stop'
    if ($code -ne 0) { throw "signing or verifying $signed failed ($code)" }
    "signed:  $((Get-AuthenticodeSignature $signed).SignerCertificate.Subject)"
} else {
    'signed:  no (set SNOWLARK_SIGN_CERT to sign)'
}

if (-not $env:CARGO_HOME) { $env:CARGO_HOME = Get-CargoHome }
$ErrorActionPreference = 'Continue'
python "$root\tools\release\notices.py" (Join-Path $stage 'THIRD-PARTY-NOTICES.txt')
$code = $LASTEXITCODE
$ErrorActionPreference = 'Stop'
if ($code -ne 0) { throw 'the notices were not written' }

$readme = [IO.File]::ReadAllText("$root\tools\release\README.txt")
$readme = $readme.Replace('{DATE}', $date).Replace('{SHA}', $sha).Replace("`r`n", "`n").Replace("`n", "`r`n")
[IO.File]::WriteAllText((Join-Path $stage 'README.txt'), $readme, (New-Object Text.UTF8Encoding $false))

$zip = Join-Path $OutDir "$name.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Add-Type -AssemblyName System.IO.Compression.FileSystem
# (the folder itself is the zip's one top-level entry, so unzipping makes one folder)
[IO.Compression.ZipFile]::CreateFromDirectory($stage, $zip, [IO.Compression.CompressionLevel]::Optimal, $true)

$size = (Get-Item $zip).Length
$hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
"zip:     $zip"
"files:   $((Get-ChildItem $stage | ForEach-Object { '{0} ({1:N0} bytes)' -f $_.Name, $_.Length }) -join ', ')"
"size:    $size bytes ($([math]::Round($size / 1MB, 1)) MB)"
"sha256:  $hash"
