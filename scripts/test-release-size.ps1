<#
  Fast, dependency-free tests for check-release-size.ps1.
  Uses sparse temporary files, so the byte-boundary cases do not consume the
  nominal 10+ MiB on disk.
#>

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$checker = Join-Path $PSScriptRoot 'check-release-size.ps1'
$productionPolicy = Join-Path $root 'packaging\size-budget.json'
$scratch = Join-Path ([IO.Path]::GetTempPath()) ("st2k-size-guard-" + [guid]::NewGuid().ToString('N'))
$script:passed = 0

function Set-SparseLength([string]$path, [int64]$length) {
    $parent = Split-Path $path -Parent
    New-Item -ItemType Directory -Path $parent -Force | Out-Null
    $stream = [IO.File]::Open($path, [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try { $stream.SetLength($length) } finally { $stream.Dispose() }
}

function Write-TestPolicy(
    [string]$path,
    [int64]$referenceInstaller = 1000,
    [int64]$installerAllowance = 100,
    [int64]$maxInstaller = 1100,
    [int64]$referenceRust = 2000,
    [int64]$rustAllowance = 200,
    [int64]$maxRust = 2200,
    [int64]$referenceMagick = 3000,
    [int64]$magickAllowance = 300,
    [int64]$maxMagick = 3300
) {
    @{
        schemaVersion = 2
        referenceVersion = 'test'
        referenceInstallerBytes = $referenceInstaller
        referenceInstallerSha256 = ('a' * 64)
        installerGrowthAllowanceBytes = $installerAllowance
        maxInstallerBytes = $maxInstaller
        referenceRustPayloadBytes = $referenceRust
        rustPayloadGrowthAllowanceBytes = $rustAllowance
        maxRustPayloadBytes = $maxRust
        referenceMagickPayloadBytes = $referenceMagick
        magickPayloadGrowthAllowanceBytes = $magickAllowance
        maxMagickPayloadBytes = $maxMagick
        rationale = 'test policy'
    } | ConvertTo-Json | Set-Content -LiteralPath $path -Encoding UTF8
}

function Set-StageTotal([string]$stage, [int64]$total) {
    if ($total -lt 2) { throw 'test stage total must be at least 2 bytes' }
    Set-SparseLength (Join-Path $stage 'sagethumbs2k.dll') 1
    Set-SparseLength (Join-Path $stage 'SageThumbs2K.exe') 1
    Set-SparseLength (Join-Path $stage 'st2k.exe') ($total - 2)
}

function Set-MagickTotal([string]$stage, [int64]$total) {
    Set-SparseLength (Join-Path $stage 'magick\payload.bin') $total
}

function Assert-Passes([string]$name, [scriptblock]$body) {
    try {
        & $body *> $null
        Write-Host "  PASS  $name" -ForegroundColor Green
        $script:passed++
    } catch {
        throw "expected PASS for '$name', got: $($_.Exception.Message)"
    }
}

function Assert-Fails([string]$name, [scriptblock]$body) {
    $failed = $false
    try { & $body *> $null } catch { $failed = $true }
    if (-not $failed) { throw "expected FAILURE for '$name'" }
    Write-Host "  PASS  $name (failed closed)" -ForegroundColor Green
    $script:passed++
}

New-Item -ItemType Directory -Path $scratch -Force | Out-Null
try {
    $policy = Join-Path $scratch 'policy.json'
    $installer = Join-Path $scratch 'setup.exe'
    $stage = Join-Path $scratch 'stage'
    Write-TestPolicy $policy

    Set-SparseLength $installer 1100
    Set-StageTotal $stage 2200
    Set-MagickTotal $stage 3300
    Assert-Passes 'exact installer, Rust, and ImageMagick ceilings' {
        & $checker -InstallerPath $installer -PolicyPath $policy -StagePath $stage
    }

    Set-SparseLength $installer 1101
    Assert-Fails 'installer ceiling plus one byte' {
        & $checker -InstallerPath $installer -PolicyPath $policy -StagePath $stage
    }

    Set-SparseLength $installer 1000
    Set-StageTotal $stage 2201
    Assert-Fails 'Rust payload ceiling plus one byte' {
        & $checker -InstallerPath $installer -PolicyPath $policy -StagePath $stage
    }

    Set-StageTotal $stage 2200
    Set-MagickTotal $stage 3301
    Assert-Fails 'ImageMagick payload ceiling plus one byte' {
        & $checker -InstallerPath $installer -PolicyPath $policy -StagePath $stage
    }

    # ---- the SHIPPING policy ------------------------------------------------
    # DERIVE these from the policy file; never hardcode a release's bytes here.
    # Hardcoding rots silently: a reviewed rebaseline raises a ceiling, every
    # pinned value falls below it, and the "plus one byte" cases stop testing a
    # boundary at all. The 1.4.1-hdr rebaseline did exactly that to all three,
    # and INVERTED the duplicated-table case below (it started passing, i.e. the
    # guard reported green for a payload it was written to reject).
    $prod = Get-Content -LiteralPath $productionPolicy -Raw | ConvertFrom-Json
    $prodInstaller = [int64]$prod.referenceInstallerBytes
    $prodMaxInstaller = [int64]$prod.maxInstallerBytes
    $prodMaxRust = [int64]$prod.maxRustPayloadBytes
    $prodMaxMagick = [int64]$prod.maxMagickPayloadBytes

    Set-SparseLength $installer $prodInstaller
    Set-StageTotal $stage ([int64]$prod.referenceRustPayloadBytes)
    Set-MagickTotal $stage ([int64]$prod.referenceMagickPayloadBytes)
    Assert-Passes 'production policy reference sizes' {
        & $checker -InstallerPath $installer -PolicyPath $productionPolicy -StagePath $stage
    }

    Set-SparseLength $installer $prodMaxInstaller
    Set-StageTotal $stage $prodMaxRust
    Set-MagickTotal $stage $prodMaxMagick
    Assert-Passes 'production ceilings exactly' {
        & $checker -InstallerPath $installer -PolicyPath $productionPolicy -StagePath $stage
    }

    Set-SparseLength $installer ($prodMaxInstaller + 1)
    Assert-Fails 'production installer ceiling plus one byte' {
        & $checker -InstallerPath $installer -PolicyPath $productionPolicy -StagePath $stage
    }

    Set-SparseLength $installer $prodInstaller
    Set-StageTotal $stage ($prodMaxRust + 1)
    Assert-Fails 'production Rust ceiling plus one byte' {
        & $checker -InstallerPath $installer -PolicyPath $productionPolicy -StagePath $stage
    }

    Set-StageTotal $stage $prodMaxRust
    Set-MagickTotal $stage ($prodMaxMagick + 1)
    Assert-Fails 'production ImageMagick ceiling plus one byte' {
        & $checker -InstallerPath $installer -PolicyPath $productionPolicy -StagePath $stage
    }

    # The pre-patch EXR lookup tables duplicated static data across all three Rust
    # artifacts. What this case actually proves is that the RAW payload check
    # rejects that duplication even though solid compression hides it and the
    # installer stays under its own ceiling — a property of the CHECKER, not of
    # whatever the product happens to weigh today. So it runs against a policy
    # pinned to the 1.3.6-rc1 baseline it was measured against. Run against the
    # LIVE policy it expires the first time the product legitimately outgrows a
    # past regression, which it now has.
    $historic = Join-Path $scratch 'historic-policy.json'
    Write-TestPolicy $historic `
        -referenceInstaller 13118612 -installerAllowance 131072 -maxInstaller 13249684 `
        -referenceRust 22216192 -rustAllowance 262144 -maxRust 22478336 `
        -referenceMagick 29827612 -magickAllowance 262144 -maxMagick 30089756
    Set-SparseLength $installer 13118612
    Set-SparseLength (Join-Path $stage 'sagethumbs2k.dll') 6785024
    Set-SparseLength (Join-Path $stage 'SageThumbs2K.exe') 9118720
    Set-SparseLength (Join-Path $stage 'st2k.exe') 6974464
    Set-MagickTotal $stage 29827612
    Assert-Fails 'known duplicated-table Rust payload' {
        & $checker -InstallerPath $installer -PolicyPath $historic -StagePath $stage
    }

    # ...and the installer check ALONE would have shipped it. This is the whole
    # reason the staged raw-payload budget exists, so assert it rather than
    # assuming it: same bytes, no -StagePath, passes.
    Assert-Passes 'duplicated tables hide under the installer ceiling' {
        & $checker -InstallerPath $installer -PolicyPath $historic
    }

    Assert-Fails 'missing installer' {
        & $checker -InstallerPath (Join-Path $scratch 'missing.exe') -PolicyPath $policy
    }

    Set-SparseLength $installer 1000
    Set-StageTotal $stage 2000
    Remove-Item -LiteralPath (Join-Path $stage 'st2k.exe') -Force
    Assert-Fails 'missing staged Rust artifact' {
        & $checker -InstallerPath $installer -PolicyPath $policy -StagePath $stage
    }

    $malformed = Join-Path $scratch 'malformed.json'
    Set-Content -LiteralPath $malformed -Value '{ not-json' -Encoding UTF8
    Assert-Fails 'malformed policy JSON' {
        & $checker -InstallerPath $installer -PolicyPath $malformed
    }

    Write-TestPolicy $policy -maxInstaller 1099
    Assert-Fails 'inconsistent policy arithmetic' {
        & $checker -InstallerPath $installer -PolicyPath $policy
    }

    Write-Host "[size-test] ALL GREEN ($script:passed cases)" -ForegroundColor Green
} finally {
    if (Test-Path -LiteralPath $scratch) {
        Remove-Item -LiteralPath $scratch -Recurse -Force
    }
}
