<#
  Export the exact current-version section of the tracked changelog and append
  the digest of the installer being published. There is intentionally no
  generated-notes fallback.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidatePattern('^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$')]
    [string]$Version,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$InstallerPath,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$OutputPath,

    [string]$ChangelogPath
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'release-manifest-lib.ps1')

if (-not $ChangelogPath) {
    $ChangelogPath = Join-Path $root 'docs\CHANGELOG.md'
    & git -C $root ls-files --error-unmatch -- 'docs/CHANGELOG.md' 2>$null | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw 'release changelog must be tracked: docs/CHANGELOG.md'
    }
}
$section = Get-ReleaseChangelogSection -ChangelogPath $ChangelogPath -Version $Version

$installer = Get-Item -LiteralPath $InstallerPath -ErrorAction Stop
Assert-ReleasePeMetadata -Path $installer.FullName -Version $Version -Description 'SageThumbs 2K Setup'
$sha256 = Get-ReleaseSha256 -Path $installer.FullName

$body = Format-ReleaseNotesBody -Section $section -Version $Version
$notes = @"
$body

## Verified installer

- **File:** ``$($installer.Name)``
- **Size:** $($installer.Length) bytes
- **SHA-256:** ``$sha256``
"@

$outputDirectory = Split-Path $OutputPath -Parent
if ($outputDirectory) {
    New-Item -ItemType Directory -Path $outputDirectory -Force | Out-Null
}
$notes | Set-Content -LiteralPath $OutputPath -Encoding utf8
Write-Host "[notes] curated $Version notes -> $OutputPath" -ForegroundColor Green
