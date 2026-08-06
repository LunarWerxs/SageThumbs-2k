<#
.SYNOPSIS
  Verifies the exact ImageMagick source tree approved for release packaging.

.DESCRIPTION
  Release packaging must not silently consume whichever ImageMagick directory happens to
  sort first under Program Files. This checker validates both the reported product identity
  and a deterministic SHA-256 inventory of every file that can enter the bundle.

  The inventory algorithm is documented in packaging/imagemagick-source.json. A deliberate
  ImageMagick upgrade requires reviewing the new upstream build, updating that pin, and
  rerunning the format regression corpus.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$SourcePath,

    [string]$PinPath = (Join-Path (Split-Path $PSScriptRoot -Parent) 'packaging\imagemagick-source.json'),

    # Which architecture's bundle this is. Only the runtime `magick.exe -version` check
    # cares: it can run solely when this matches the host, so a cross-architecture build
    # defers that one step to CI. Everything else is architecture-agnostic hashing.
    [ValidateSet('x64', 'arm64')]
    [string]$Architecture = 'x64',

    [switch]$PassThru
)

$ErrorActionPreference = 'Stop'

function Get-HexSha256 {
    param([Parameter(Mandatory)][byte[]]$Bytes)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        return [Convert]::ToHexString($sha.ComputeHash($Bytes)).ToLowerInvariant()
    } finally {
        $sha.Dispose()
    }
}

function Get-ApprovedInventory {
    param([Parameter(Mandatory)][string]$Root)

    $files = [System.Collections.Generic.List[System.IO.FileInfo]]::new()
    foreach ($name in 'magick.exe', 'License.txt', 'NOTICE.txt') {
        $path = Join-Path $Root $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "Pinned ImageMagick source is missing required file: $name"
        }
        $files.Add((Get-Item -LiteralPath $path))
    }
    Get-ChildItem -LiteralPath $Root -File |
        Where-Object { $_.Extension -in '.dll', '.xml' } |
        ForEach-Object { $files.Add($_) }

    $modules = Join-Path $Root 'modules'
    if (-not (Test-Path -LiteralPath $modules -PathType Container)) {
        throw "Pinned ImageMagick source is missing its modules directory: $modules"
    }
    Get-ChildItem -LiteralPath $modules -Recurse -File | ForEach-Object { $files.Add($_) }

    # De-duplicate the explicitly named files from the extension scan, then use an
    # ordinal-ignore-case ordering so the digest is independent of locale and filesystem
    # enumeration order.
    $byRelativePath = [System.Collections.Generic.Dictionary[string, System.IO.FileInfo]]::new(
        [System.StringComparer]::OrdinalIgnoreCase
    )
    foreach ($file in $files) {
        $relative = [System.IO.Path]::GetRelativePath($Root, $file.FullName).Replace('\', '/')
        $byRelativePath[$relative] = $file
    }
    $relativePaths = [string[]]$byRelativePath.Keys
    [Array]::Sort($relativePaths, [System.StringComparer]::OrdinalIgnoreCase)

    $records = [System.Collections.Generic.List[string]]::new()
    [int64]$totalBytes = 0
    foreach ($relative in $relativePaths) {
        $file = $byRelativePath[$relative]
        if ($totalBytes -gt [int64]::MaxValue - $file.Length) {
            throw 'ImageMagick source inventory byte count overflowed Int64'
        }
        $totalBytes += $file.Length
        $hash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        $records.Add("$relative|$($file.Length)|$hash")
    }

    $text = [string]::Join("`n", $records) + "`n"
    $bytes = [System.Text.UTF8Encoding]::new($false).GetBytes($text)
    [pscustomobject]@{
        FileCount  = $records.Count
        TotalBytes = $totalBytes
        Sha256     = Get-HexSha256 -Bytes $bytes
    }
}

$resolvedSource = (Resolve-Path -LiteralPath $SourcePath).Path.TrimEnd('\')
$resolvedPin = (Resolve-Path -LiteralPath $PinPath).Path
$pin = Get-Content -LiteralPath $resolvedPin -Raw | ConvertFrom-Json
if ($pin.schemaVersion -ne 1) {
    throw "Unsupported ImageMagick source pin schemaVersion '$($pin.schemaVersion)'"
}

$expectedDirectory = [string]$pin.identity.installDirectoryName
if ([System.IO.Path]::GetFileName($resolvedSource) -cne $expectedDirectory) {
    throw "ImageMagick source directory must be '$expectedDirectory'; got '$resolvedSource'"
}

$magickExe = Join-Path $resolvedSource 'magick.exe'
$versionInfo = (Get-Item -LiteralPath $magickExe).VersionInfo
if ($versionInfo.FileVersion -cne [string]$pin.identity.fileVersion) {
    throw "ImageMagick magick.exe FileVersion mismatch: expected '$($pin.identity.fileVersion)', got '$($versionInfo.FileVersion)'"
}

# The runtime identity check EXECUTES magick.exe, which only works when the pinned bundle
# matches the host architecture: an x64 machine cannot run an ARM64 binary at all. Rather
# than drop the check, defer it to the arm64 CI job, which runs on native ARM hardware.
# What still holds locally is strictly stronger than a printed version string: the PE
# FileVersion above, plus the 195-file/byte-count/SHA-256 inventory below, which pins the
# exact upstream bytes. Never "fix" a cross-architecture skip by weakening the inventory.
$hostArch = if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq 'Arm64') { 'arm64' } else { 'x64' }
if ($Architecture -cne $hostArch) {
    $versionLine = "(runtime identity DEFERRED: $Architecture bundle on an $hostArch host; the arm64 CI job runs it natively)"
    Write-Host "  $versionLine" -ForegroundColor Yellow
} else {
    $versionOutput = @(& $magickExe -version 2>&1)
    if ($LASTEXITCODE -ne 0 -or $versionOutput.Count -eq 0) {
        throw "Pinned ImageMagick magick.exe did not run successfully: $magickExe"
    }
    $versionLine = [string]$versionOutput[0]
    if ($versionLine -notmatch [string]$pin.identity.versionLinePattern) {
        throw "ImageMagick runtime identity mismatch. Expected /$($pin.identity.versionLinePattern)/; got '$versionLine'"
    }
}

$inventory = Get-ApprovedInventory -Root $resolvedSource
if ($inventory.FileCount -ne [int]$pin.inventory.fileCount) {
    throw "ImageMagick source inventory count mismatch: expected $($pin.inventory.fileCount), got $($inventory.FileCount)"
}
if ($inventory.TotalBytes -ne [int64]$pin.inventory.totalBytes) {
    throw "ImageMagick source inventory byte count mismatch: expected $($pin.inventory.totalBytes), got $($inventory.TotalBytes)"
}
if ($inventory.Sha256 -cne [string]$pin.inventory.sha256) {
    throw "ImageMagick source inventory SHA-256 mismatch: expected $($pin.inventory.sha256), got $($inventory.Sha256)"
}

Write-Host "[magick-source] PASS $versionLine" -ForegroundColor Green
Write-Host "  inventory: $($inventory.FileCount) files, $($inventory.TotalBytes) bytes, $($inventory.Sha256)" -ForegroundColor DarkGray

if ($PassThru) {
    [pscustomobject]@{
        SourcePath = $resolvedSource
        VersionLine = $versionLine
        Inventory = $inventory
    }
}

# Set the exit code EXPLICITLY on the success path.
#
# This script fails by throwing, so reaching here means it passed - but a PowerShell script
# that never runs a native command also never sets $LASTEXITCODE. Callers written as
#     & check-magick-source.ps1 ...
#     if ($LASTEXITCODE) { throw ... }
# then read a variable that may not exist, and release-manifest-lib.ps1 turns on StrictMode,
# under which that is a terminating error rather than a zero.
#
# It only bit in one combination, which is why it survived: normally `cargo build` has run
# first and left a 0 behind. With `-SkipBuild` nothing native runs beforehand, and on ARM64
# the identity check takes its DEFERRED branch and returns without an exit either - so
# `build-release.ps1 -Portable -SkipBuild -Architecture arm64` died here. That is exactly the
# invocation release.ps1 makes at [4a/6], so it would have failed a release, after main was
# pushed and CI was green.
exit 0
