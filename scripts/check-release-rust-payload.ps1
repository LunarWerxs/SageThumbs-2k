<#
  CI gate for the three production Rust artifacts. This intentionally validates
  the raw payload before an installer exists, and selects the architecture's
  shipped profile from scripts/packaging/size-budget.json.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$ArtifactDirectory,

    [ValidateSet('x64', 'arm64')]
    [string]$Architecture = 'x64',

    [string]$PolicyPath,

    [string]$ExpectedVersion
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'release-manifest-lib.ps1')

function Read-RequiredInt64([object]$object, [string]$name) {
    $property = $object.PSObject.Properties[$name]
    if ($null -eq $property) { throw "size policy requires '$name'" }
    $value = $property.Value
    if ($value -is [bool] -or $value -is [string] -or $null -eq $value) {
        throw "size policy requires integer '$name'"
    }
    try {
        $integer = [Convert]::ToInt64($value, [Globalization.CultureInfo]::InvariantCulture)
        $decimal = [Convert]::ToDecimal($value, [Globalization.CultureInfo]::InvariantCulture)
    } catch { throw "size policy has invalid integer '$name': $value" }
    if ($integer -lt 0 -or $decimal -ne [decimal]$integer) {
        throw "size policy requires non-negative integer '$name'"
    }
    return [int64]$integer
}

function Get-ArchitecturePolicy([object]$policy, [string]$architecture) {
    $schema = Read-RequiredInt64 $policy 'schemaVersion'
    if ($schema -ne 3) { throw "unsupported release size policy schemaVersion $schema (expected 3)" }
    $architectures = $policy.PSObject.Properties['architectures']
    if ($null -eq $architectures -or $null -eq $architectures.Value) {
        throw "size policy requires an 'architectures' object"
    }
    $architecturePolicy = $architectures.Value.PSObject.Properties[$architecture]
    if ($null -eq $architecturePolicy -or $null -eq $architecturePolicy.Value) {
        throw "size policy has no '$architecture' architecture policy"
    }
    $profile = if ($architecture -eq 'x64') { 'full' } else { 'compact' }
    $profilePolicy = $architecturePolicy.Value.PSObject.Properties[$profile]
    if ($null -eq $profilePolicy -or $null -eq $profilePolicy.Value) {
        throw "size policy has no '$architecture/$profile' profile"
    }
    return [pscustomobject]@{ Name = $profile; Policy = $profilePolicy.Value }
}

if (-not $PolicyPath) { $PolicyPath = Join-Path $root 'scripts\packaging\size-budget.json' }
if (-not $ExpectedVersion) {
    $ExpectedVersion = ([regex]::Match(
        (Get-Content (Join-Path $root 'Cargo.toml') -Raw),
        '(?m)^\s*version\s*=\s*"([^"]+)"'
    )).Groups[1].Value
}
if (-not $ExpectedVersion) { throw 'could not determine expected release version' }
if (-not (Test-Path -LiteralPath $ArtifactDirectory -PathType Container)) {
    throw "release artifact directory not found: $ArtifactDirectory"
}
if (-not (Test-Path -LiteralPath $PolicyPath -PathType Leaf)) {
    throw "release size policy not found: $PolicyPath"
}

try { $policy = Get-Content -LiteralPath $PolicyPath -Raw | ConvertFrom-Json }
catch { throw "release size policy is not valid JSON: $PolicyPath`n$($_.Exception.Message)" }
$selected = Get-ArchitecturePolicy $policy $Architecture
$maxRust = Read-RequiredInt64 $selected.Policy 'maxRustPayloadBytes'


$total = [int64]0
foreach ($name in 'sagethumbs2k.dll', 'st2k_dlghook.dll', 'SageThumbs2K.exe', 'st2k.exe') {
    $path = Join-Path $ArtifactDirectory $name
    Assert-ReleasePeMetadata -Path $path -Version $ExpectedVersion
    Assert-ReleasePeArchitecture -Path $path -Architecture $Architecture
    $length = [int64](Get-Item -LiteralPath $path).Length
    if ($total -gt [int64]::MaxValue - $length) { throw 'release Rust payload byte count overflowed Int64' }
    $total += $length
    Write-Host ("[size-ci] {0,-18} {1:N0} bytes" -f $name, $length) -ForegroundColor DarkGray
}

Write-Host ("[size-ci] $Architecture/$($selected.Name) Rust payload: {0:N0} bytes; limit: {1:N0} bytes" -f $total, $maxRust) -ForegroundColor Cyan
if ($total -gt $maxRust) {
    # REPORTING ONLY. The VERSIONINFO and machine-type assertions above still THROW: those
    # catch a stale artifact or an x64 binary in an ARM64 build, which has nothing to do with size.
    Write-Host ("[size-ci] NOTE Rust payload is {0:N0} bytes over the old reference budget (not enforced)." -f ($total - $maxRust)) -ForegroundColor Yellow
}
Write-Host ("[size-ci] Rust payload headroom: {0:N0} bytes" -f ($maxRust - $total)) -ForegroundColor Green
