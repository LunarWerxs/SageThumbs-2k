<#
.SYNOPSIS
  Focused fail-closed tests for the pinned ImageMagick packaging gates.

.EXAMPLE
  pwsh scripts/test-magick-packaging.ps1 -BundlePath scripts/packaging/stage/x64/magick
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$BundlePath,

    [string]$SourcePath = 'C:\Program Files\ImageMagick-7.1.2-Q16-HDRI',

    [string]$ObjdumpPath
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$sourceCheck = Join-Path $PSScriptRoot 'check-magick-source.ps1'
$bundleCheck = Join-Path $PSScriptRoot 'check-magick-bundle.ps1'
$pruneCheck = Join-Path $PSScriptRoot 'prune-magick-unreferenced.ps1'
$pinPath = Join-Path $root 'scripts\packaging\imagemagick-source.json'
$resolvedBundle = (Resolve-Path -LiteralPath $BundlePath).Path
if (-not $ObjdumpPath) {
    $inspector = Get-Command objdump, llvm-objdump -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $inspector) { throw 'objdump/llvm-objdump is required for Magick packaging tests' }
    $ObjdumpPath = $inspector.Source
}

$script:passed = 0
. (Join-Path $PSScriptRoot 'test-assert-lib.ps1')
function Pass-Test([string]$Name) {
    $script:passed++
    Write-Host "  PASS $Name" -ForegroundColor Green
}

$temp = Join-Path ([System.IO.Path]::GetTempPath()) ("st2k-magick-tests-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $temp | Out-Null
try {
    & $sourceCheck -SourcePath $SourcePath -PinPath $pinPath
    Pass-Test 'approved source identity + inventory'

    & $bundleCheck -BundlePath $resolvedBundle -ObjdumpPath $ObjdumpPath
    Pass-Test 'dependency-closed flattened bundle smoke'

    $badPin = Join-Path $temp 'bad-pin.json'
    $pin = Get-Content -LiteralPath $pinPath -Raw | ConvertFrom-Json
    $pin.inventory.sha256 = ('0' * 64)
    $pin | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $badPin -Encoding utf8NoBOM
    Assert-ThrowsLike -Name 'source inventory mismatch fails closed' -Pattern 'inventory SHA-256 mismatch' -Action {
        & $sourceCheck -SourcePath $SourcePath -PinPath $badPin
    }

    $missingDependency = Join-Path $temp 'missing-dependency'
    New-Item -ItemType Directory -Path $missingDependency | Out-Null
    Copy-Item -Path (Join-Path $resolvedBundle '*') -Destination $missingDependency -Recurse
    [System.IO.File]::Delete((Join-Path $missingDependency 'vcomp140.dll'))
    Assert-ThrowsLike -Name 'missing transitive VC runtime fails closure' -Pattern 'VCOMP140\.DLL' -Action {
        & $bundleCheck -BundlePath $missingDependency -ObjdumpPath $ObjdumpPath -SkipSmoke
    }

    $missingLegal = Join-Path $temp 'missing-legal'
    New-Item -ItemType Directory -Path $missingLegal | Out-Null
    Copy-Item -Path (Join-Path $resolvedBundle '*') -Destination $missingLegal -Recurse
    [System.IO.File]::Delete((Join-Path $missingLegal 'NOTICE.txt'))
    Assert-ThrowsLike -Name 'missing upstream notice fails closed' -Pattern 'NOTICE\.txt' -Action {
        & $bundleCheck -BundlePath $missingLegal -ObjdumpPath $ObjdumpPath -SkipSmoke
    }

    $missingWriter = Join-Path $temp 'missing-writer'
    New-Item -ItemType Directory -Path $missingWriter | Out-Null
    Copy-Item -Path (Join-Path $resolvedBundle '*') -Destination $missingWriter -Recurse
    [System.IO.File]::Delete((Join-Path $missingWriter 'modules\coders\IM_MOD_RL_jxl_.dll'))
    Assert-ThrowsLike -Name 'missing dynamic JXL writer fails output smoke' -Pattern 'JXL|advertised\.jxl' -Action {
        & $bundleCheck -BundlePath $missingWriter -ObjdumpPath $ObjdumpPath
    }

    Assert-ThrowsLike -Name 'referenced DLL cannot be pruned' -Pattern 'imported by' -Action {
        & $pruneCheck -BundlePath $resolvedBundle -ObjdumpPath $ObjdumpPath -Candidate @('vcomp140.dll')
    }
} finally {
    if (Test-Path -LiteralPath $temp) {
        [System.IO.Directory]::Delete($temp, $true)
    }
}

if ($passed -ne 7) {
    throw "Magick packaging test accounting error: expected 7 passes, got $passed"
}
Write-Host "[magick-tests] PASS $passed/7" -ForegroundColor Green
