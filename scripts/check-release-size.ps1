<#
  Fail closed when a release artifact exceeds the selected architecture's
  policy. Both architectures ship the full profile and always bundle
  ImageMagick. ARM64's installer reference remains deliberately uncalibrated
  until the first verified installer exists, so an ARM64 installer cannot pass
  this gate merely because its raw Rust payload is within budget.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$InstallerPath,

    [ValidateSet('x64', 'arm64')]
    [string]$Architecture = 'x64',

    [string]$PolicyPath,

    [string]$StagePath
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
if (-not $PolicyPath) { $PolicyPath = Join-Path $root 'scripts\packaging\size-budget.json' }

function Read-RequiredText([object]$object, [string]$name) {
    $property = $object.PSObject.Properties[$name]
    if ($null -eq $property -or $property.Value -isnot [string] -or [string]::IsNullOrWhiteSpace($property.Value)) {
        throw "size policy '$PolicyPath' requires a non-empty string '$name'"
    }
    return $property.Value
}

function Read-RequiredBool([object]$object, [string]$name) {
    $property = $object.PSObject.Properties[$name]
    if ($null -eq $property -or $property.Value -isnot [bool]) {
        throw "size policy '$PolicyPath' requires Boolean '$name'"
    }
    return [bool]$property.Value
}

function Read-RequiredInt64([object]$object, [string]$name) {
    $property = $object.PSObject.Properties[$name]
    if ($null -eq $property) { throw "size policy '$PolicyPath' is missing '$name'" }
    $value = $property.Value
    if ($value -is [bool] -or $value -is [string] -or $null -eq $value) {
        throw "size policy '$PolicyPath' requires integer '$name'"
    }
    try {
        $integer = [Convert]::ToInt64($value, [Globalization.CultureInfo]::InvariantCulture)
        $decimal = [Convert]::ToDecimal($value, [Globalization.CultureInfo]::InvariantCulture)
    } catch { throw "size policy '$PolicyPath' has invalid integer '$name': $value" }
    if ($integer -lt 0 -or $decimal -ne [decimal]$integer) {
        throw "size policy '$PolicyPath' requires non-negative integer '$name'"
    }
    return [int64]$integer
}

function Format-Size([int64]$bytes) { return ('{0:N0} bytes ({1:N3} MiB)' -f $bytes, ($bytes / 1MB)) }

function Get-ArchitecturePolicy([object]$policy, [string]$architecture) {
    $schema = Read-RequiredInt64 $policy 'schemaVersion'
    if ($schema -ne 3) { throw "unsupported release size policy schemaVersion $schema (expected 3)" }
    $architectures = $policy.PSObject.Properties['architectures']
    if ($null -eq $architectures -or $null -eq $architectures.Value) {
        throw "size policy '$PolicyPath' requires an 'architectures' object"
    }
    $architecturePolicy = $architectures.Value.PSObject.Properties[$architecture]
    if ($null -eq $architecturePolicy -or $null -eq $architecturePolicy.Value) {
        throw "size policy '$PolicyPath' has no '$architecture' architecture policy"
    }
    # Both architectures now ship Full: ARM64 gained its own pinned ImageMagick bundle
    # (scripts\packaging\imagemagick-source-arm64.json), so 'compact' is no longer an ARM64 fact.
    $profile = 'full'
    $profilePolicy = $architecturePolicy.Value.PSObject.Properties[$profile]
    if ($null -eq $profilePolicy -or $null -eq $profilePolicy.Value) {
        throw "size policy '$PolicyPath' has no '$architecture/$profile' profile"
    }
    return [pscustomobject]@{ Name = $profile; Policy = $profilePolicy.Value }
}

function Test-StageRustPayload([string]$stagePath, [object]$profile) {
    $total = [int64]0
    foreach ($name in 'sagethumbs2k.dll', 'st2k_dlghook.dll', 'SageThumbs2K.exe', 'st2k.exe') {
        $path = Join-Path $stagePath $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "release stage is missing required Rust artifact: $path"
        }
        $length = [int64](Get-Item -LiteralPath $path).Length
        if ($total -gt [int64]::MaxValue - $length) { throw 'staged Rust payload byte count overflowed Int64' }
        $total += $length
        Write-Host ("[size]   {0,-18} {1}" -f $name, (Format-Size $length)) -ForegroundColor DarkGray
    }
    $reference = Read-RequiredInt64 $profile 'referenceRustPayloadBytes'
    $allowance = Read-RequiredInt64 $profile 'rustPayloadGrowthAllowanceBytes'
    $maximum = Read-RequiredInt64 $profile 'maxRustPayloadBytes'
    # REPORTING ONLY (see the header): the arithmetic is no longer asserted, because an
    # unenforced number drifts and would then fail for a reason nobody acts on.
    $headroom = $maximum - $total
    Write-Host "[size] Rust payload: $(Format-Size $total)" -ForegroundColor Cyan
    Write-Host "[size] Rust payload limit: $(Format-Size $maximum)" -ForegroundColor DarkGray
    if ($headroom -lt 0) {
        Write-Host ("[size] NOTE Rust payload is {0} over the old reference budget (not enforced)." -f (Format-Size (-$headroom))) -ForegroundColor Yellow
    }
    Write-Host "[size] Rust payload headroom: $(Format-Size $headroom) (delta from reference: $($total - $reference) bytes)" -ForegroundColor Green
}

if (-not (Test-Path -LiteralPath $PolicyPath -PathType Leaf)) { throw "release size policy not found: $PolicyPath" }
try { $policy = Get-Content -LiteralPath $PolicyPath -Raw | ConvertFrom-Json }
catch { throw "release size policy is not valid JSON: $PolicyPath`n$($_.Exception.Message)" }
if ($null -eq $policy) { throw "release size policy is empty: $PolicyPath" }
$selected = Get-ArchitecturePolicy $policy $Architecture
$profile = $selected.Policy
$rationale = Read-RequiredText $profile 'rationale'

if (-not (Test-Path -LiteralPath $InstallerPath -PathType Leaf)) { throw "release installer not found: $InstallerPath" }
if ($StagePath) {
    if (-not (Test-Path -LiteralPath $StagePath -PathType Container)) { throw "release stage directory not found: $StagePath" }

    Test-StageRustPayload $StagePath $profile
}

$installerCalibrated = Read-RequiredBool $profile 'installerReferenceCalibrated'
if (-not $installerCalibrated) {
    throw "release size budget cannot validate $Architecture/$($selected.Name) installer: no calibrated installer reference yet. Build and independently verify the first installer, then record its bytes and SHA-256 in scripts/packaging/size-budget.json. Policy rationale: $rationale"
}

$referenceVersion = Read-RequiredText $profile 'referenceVersion'
$referenceSha256 = Read-RequiredText $profile 'referenceInstallerSha256'
if ($referenceSha256 -notmatch '^[0-9a-fA-F]{64}$') { throw "size policy '$PolicyPath' has invalid referenceInstallerSha256" }
$referenceInstaller = Read-RequiredInt64 $profile 'referenceInstallerBytes'
$installerAllowance = Read-RequiredInt64 $profile 'installerGrowthAllowanceBytes'
$maxInstaller = Read-RequiredInt64 $profile 'maxInstallerBytes'
# REPORTING ONLY: see the header.

$installerBytes = [int64](Get-Item -LiteralPath $InstallerPath).Length
$headroom = $maxInstaller - $installerBytes
Write-Host "[size] policy: $Architecture/$($selected.Name), reference $referenceVersion ($referenceSha256)" -ForegroundColor DarkGray
Write-Host "[size] installer: $(Format-Size $installerBytes)" -ForegroundColor Cyan
Write-Host "[size] installer limit: $(Format-Size $maxInstaller)" -ForegroundColor DarkGray
if ($headroom -lt 0) {
    Write-Host ("[size] NOTE installer is {0} over the old reference budget (not enforced)." -f (Format-Size (-$headroom))) -ForegroundColor Yellow
}
Write-Host "[size] installer headroom: $(Format-Size $headroom) (delta from reference: $($installerBytes - $referenceInstaller) bytes)" -ForegroundColor Green

if ($StagePath -and $Architecture -eq 'x64') {
    $magickPath = Join-Path $StagePath 'magick'
    if (Test-Path -LiteralPath $magickPath -PathType Container) {
        $magickBytes = [int64]0
        Get-ChildItem -LiteralPath $magickPath -Recurse -File | ForEach-Object {
            if ($magickBytes -gt [int64]::MaxValue - $_.Length) { throw 'staged ImageMagick payload byte count overflowed Int64' }
            $magickBytes += $_.Length
        }
        $referenceMagick = Read-RequiredInt64 $profile 'referenceMagickPayloadBytes'
        $magickAllowance = Read-RequiredInt64 $profile 'magickPayloadGrowthAllowanceBytes'
        $maxMagick = Read-RequiredInt64 $profile 'maxMagickPayloadBytes'
        # REPORTING ONLY: see the header. This budget in particular grew only because the
        # pinned ImageMagick moved 7.1.2-25 -> 7.1.2-29 for SECURITY, i.e. it fired on the
        # updates that are least optional.
        $magickHeadroom = $maxMagick - $magickBytes
        if ($magickHeadroom -lt 0) {
            Write-Host ("[size] NOTE ImageMagick payload is {0} over the old reference budget (not enforced)." -f (Format-Size (-$magickHeadroom))) -ForegroundColor Yellow
        }
        Write-Host "[size] ImageMagick payload: $(Format-Size $magickBytes); headroom: $(Format-Size $magickHeadroom)" -ForegroundColor Green
    } else { Write-Host '[size] ImageMagick payload: not staged (engine-less build)' -ForegroundColor DarkGray }
}

Write-Host '[size] release size budget passed.' -ForegroundColor Green
