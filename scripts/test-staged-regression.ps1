<#
  Exercise the full format corpus against the exact installer payload.

  packaging\stage\x64 keeps ImageMagick in a subdirectory for Inno Setup, while the
  installer flattens that subdirectory into the application directory. This
  wrapper reproduces the installed layout in a disposable directory, copies the
  staged st2k.exe beside the staged Magick runtime, and delegates to
  regression.ps1. It prevents a local Program Files ImageMagick from masking a
  missing coder/delegate or a bad no-op text-stack shim in the release bundle.
#>
[CmdletBinding()]
param(
    [string]$StagePath,
    [string]$Corpus,
    [int]$Size = 96
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
if (-not $StagePath) {
    $StagePath = Join-Path $root 'packaging\stage\x64'
}
$stage = (Resolve-Path -LiteralPath $StagePath -ErrorAction Stop).Path
$stagedCli = Join-Path $stage 'st2k.exe'
$stagedMagick = Join-Path $stage 'magick'
if (-not (Test-Path -LiteralPath $stagedCli -PathType Leaf)) {
    throw "staged CLI not found: $stagedCli"
}
if (-not (Test-Path -LiteralPath (Join-Path $stagedMagick 'magick.exe') -PathType Leaf)) {
    throw "staged ImageMagick bundle not found: $stagedMagick"
}

$runtime = Join-Path (
    [IO.Path]::GetTempPath()
) ("st2k-staged-regression-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $runtime | Out-Null
try {
    Copy-Item -LiteralPath $stagedCli -Destination (Join-Path $runtime 'st2k.exe')
    Copy-Item -Path (Join-Path $stagedMagick '*') -Destination $runtime -Recurse

    $arguments = @(
        '-NoProfile',
        '-File', (Join-Path $PSScriptRoot 'regression.ps1'),
        '-Size', $Size,
        '-St2kPath', (Join-Path $runtime 'st2k.exe'),
        '-MagickPath', (Join-Path $runtime 'magick.exe')
    )
    if ($Corpus) {
        $arguments += @('-Corpus', $Corpus)
    }

    & pwsh @arguments
    if ($LASTEXITCODE -ne 0) {
        throw "staged format regression failed with exit code $LASTEXITCODE"
    }
} finally {
    if (Test-Path -LiteralPath $runtime) {
        Remove-Item -LiteralPath $runtime -Recurse -Force
    }
}

Write-Host '[staged-regression] exact flattened installer runtime passed.' -ForegroundColor Green
