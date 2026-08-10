<# Fast, dependency-free boundary tests for check-release-size.ps1. #>
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$checker = Join-Path $PSScriptRoot 'check-release-size.ps1'
$productionPolicy = Join-Path $root 'scripts\packaging\size-budget.json'
$scratch = Join-Path ([IO.Path]::GetTempPath()) ("st2k-size-guard-" + [guid]::NewGuid().ToString('N'))
$script:passed = 0

function Set-SparseLength([string]$path, [int64]$length) {
    New-Item -ItemType Directory -Path (Split-Path $path -Parent) -Force | Out-Null
    $stream = [IO.File]::Open($path, [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try { $stream.SetLength($length) } finally { $stream.Dispose() }
}

function Write-TestPolicy([string]$path, [bool]$armInstallerCalibrated = $false) {
    $arm = [ordered]@{
        referenceVersion = 'arm-test'
        installerReferenceCalibrated = $armInstallerCalibrated
        referenceRustPayloadBytes = 1500
        rustPayloadGrowthAllowanceBytes = 100
        maxRustPayloadBytes = 1600
        referenceMagickPayloadBytes = 3000
        magickPayloadGrowthAllowanceBytes = 300
        maxMagickPayloadBytes = 3300
        rationale = 'ARM64 Full test policy'
    }
    if ($armInstallerCalibrated) {
        $arm.referenceInstallerBytes = 900
        $arm.referenceInstallerSha256 = ('b' * 64)
        $arm.installerGrowthAllowanceBytes = 100
        $arm.maxInstallerBytes = 1000
    }
    [ordered]@{
        schemaVersion = 3
        architectures = [ordered]@{
            x64 = [ordered]@{ full = [ordered]@{
                referenceVersion = 'x64-test'
                installerReferenceCalibrated = $true
                referenceInstallerBytes = 1000
                referenceInstallerSha256 = ('a' * 64)
                installerGrowthAllowanceBytes = 100
                maxInstallerBytes = 1100
                referenceRustPayloadBytes = 2000
                rustPayloadGrowthAllowanceBytes = 200
                maxRustPayloadBytes = 2200
                referenceMagickPayloadBytes = 3000
                magickPayloadGrowthAllowanceBytes = 300
                maxMagickPayloadBytes = 3300
                rationale = 'x64 Full test policy'
            }}
            arm64 = [ordered]@{ full = $arm }
        }
    } | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $path -Encoding UTF8
}

function Set-StageTotal([string]$stage, [int64]$total) {
    if ($total -lt 2) { throw 'test stage total must be at least 2 bytes' }
    Set-SparseLength (Join-Path $stage 'sagethumbs2k.dll') 1
    Set-SparseLength (Join-Path $stage 'SageThumbs2K.exe') 1
    Set-SparseLength (Join-Path $stage 'st2k.exe') ($total - 2)
}

function Set-MagickTotal([string]$stage, [int64]$total) { Set-SparseLength (Join-Path $stage 'magick\payload.bin') $total }
function Assert-Passes([string]$name, [scriptblock]$body) {
    try { & $body *> $null } catch { throw "expected PASS for '$name', got: $($_.Exception.Message)" }
    Write-Host "  PASS  $name" -ForegroundColor Green; $script:passed++
}
function Assert-FailsLike([string]$name, [string]$pattern, [scriptblock]$body) {
    try { & $body *> $null; throw "expected FAILURE for '$name'" }
    catch {
        if ($_.Exception.Message -notlike $pattern) {
            throw "expected '$name' to match '$pattern', got: $($_.Exception.Message)"
        }
    }
    Write-Host "  PASS  $name (failed closed)" -ForegroundColor Green; $script:passed++
}

New-Item -ItemType Directory -Path $scratch -Force | Out-Null
try {
    $policy = Join-Path $scratch 'policy.json'; $installer = Join-Path $scratch 'setup.exe'; $stage = Join-Path $scratch 'stage'
    Write-TestPolicy $policy

    Set-SparseLength $installer 1100; Set-StageTotal $stage 2200; Set-MagickTotal $stage 3300
    Assert-Passes 'x64 defaults to Full and accepts exact ceilings' { & $checker -InstallerPath $installer -PolicyPath $policy -StagePath $stage }
    Set-SparseLength $installer 1101
    Assert-FailsLike 'x64 installer ceiling plus one byte' '*installer exceeds its limit*' { & $checker -InstallerPath $installer -PolicyPath $policy }
    Set-SparseLength $installer 1000; Set-StageTotal $stage 2201
    Assert-FailsLike 'x64 Rust ceiling plus one byte' '*Rust payload exceeds its limit*' { & $checker -InstallerPath $installer -PolicyPath $policy -StagePath $stage }
    Set-StageTotal $stage 2200; Set-MagickTotal $stage 3301
    Assert-FailsLike 'x64 ImageMagick ceiling plus one byte' '*ImageMagick payload exceeds its limit*' { & $checker -InstallerPath $installer -PolicyPath $policy -StagePath $stage }

    Remove-Item -LiteralPath (Join-Path $stage 'magick') -Recurse -Force
    Set-StageTotal $stage 1600; Set-SparseLength $installer 900
    Assert-FailsLike 'ARM64 refuses an uncalibrated installer reference after raw stage validation' '*no calibrated installer reference*' {
        & $checker -Architecture arm64 -InstallerPath $installer -PolicyPath $policy -StagePath $stage
    }
    Set-MagickTotal $stage 1
    # ARM64 used to REJECT any staged ImageMagick outright. It is Full now, so staging one
    # must no longer be the REASON it fails: the only remaining objection here is the
    # uncalibrated installer reference. Asserting the failure reason is what proves the old
    # architecture-based prohibition is gone.
    Assert-FailsLike 'ARM64 Full objects only to calibration, not to a staged ImageMagick payload' '*no calibrated installer reference*' {
        & $checker -Architecture arm64 -InstallerPath $installer -PolicyPath $policy -StagePath $stage
    }

    $malformed = Join-Path $scratch 'malformed.json'
    Set-Content -LiteralPath $malformed -Value '{ not-json' -Encoding UTF8
    Assert-FailsLike 'malformed policy JSON' '*not valid JSON*' {
        & $checker -InstallerPath $installer -PolicyPath $malformed
    }

    Write-TestPolicy $policy
    $inconsistent = Get-Content -LiteralPath $policy -Raw | ConvertFrom-Json
    $inconsistent.architectures.x64.full.maxRustPayloadBytes = 2199
    $inconsistent | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $policy -Encoding UTF8
    Remove-Item -LiteralPath (Join-Path $stage 'magick') -Recurse -Force
    Set-StageTotal $stage 2000
    Assert-FailsLike 'x64 inconsistent Rust arithmetic' '*Rust maximum must equal*' {
        & $checker -InstallerPath $installer -PolicyPath $policy -StagePath $stage
    }

    Write-TestPolicy $policy -armInstallerCalibrated $true
    Remove-Item -LiteralPath (Join-Path $stage 'magick') -Recurse -Force -ErrorAction SilentlyContinue
    Set-StageTotal $stage 1600
    Assert-Passes 'ARM64 calibrated Full policy accepts its exact ceilings' {
        & $checker -Architecture arm64 -InstallerPath $installer -PolicyPath $policy -StagePath $stage
    }

    $prod = Get-Content -LiteralPath $productionPolicy -Raw | ConvertFrom-Json
    if ([int64]$prod.schemaVersion -ne 3) { throw 'production size policy must use schema v3' }
    foreach ($architecture in 'x64', 'arm64') {
        $profileName = 'full'
        $profile = $prod.architectures.$architecture.$profileName
        if ($null -eq $profile) { throw "production policy missing $architecture/$profileName" }
        $referenceRust = [int64]$profile.referenceRustPayloadBytes
        $maxRust = [int64]$profile.maxRustPayloadBytes
        Set-StageTotal $stage $referenceRust
        Remove-Item -LiteralPath (Join-Path $stage 'magick') -Recurse -Force -ErrorAction SilentlyContinue
        Set-SparseLength $installer 1
        if ($architecture -eq 'x64') {
            Set-MagickTotal $stage ([int64]$profile.referenceMagickPayloadBytes)
            Set-SparseLength $installer ([int64]$profile.referenceInstallerBytes)
            Assert-Passes 'production x64 reference sizes' { & $checker -InstallerPath $installer -PolicyPath $productionPolicy -StagePath $stage }
        } else {
            # Calibrated 2026-08-01 with the first real ARM64 installer, so this now
            # asserts the reference PASSES rather than that the profile is still gated.
            # ARM64 is Full now: it stages a magick payload exactly like x64.
            Set-SparseLength $installer ([int64]$profile.referenceInstallerBytes)
            Assert-Passes 'production ARM64 reference sizes' {
                & $checker -Architecture arm64 -InstallerPath $installer -PolicyPath $productionPolicy -StagePath $stage
            }
        }
        Set-StageTotal $stage ($maxRust + 1)
        Assert-FailsLike "production $architecture Rust ceiling plus one byte" '*Rust payload exceeds its limit*' {
            & $checker -Architecture $architecture -InstallerPath $installer -PolicyPath $productionPolicy -StagePath $stage
        }
    }
    Write-Host "[size-test] ALL GREEN ($script:passed cases)" -ForegroundColor Green
} finally {
    if (Test-Path -LiteralPath $scratch) { Remove-Item -LiteralPath $scratch -Recurse -Force }
}
