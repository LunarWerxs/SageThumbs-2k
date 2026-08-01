<#
.SYNOPSIS
  Reports whether the bundled zlib and libpng DLLs have newer upstream releases.

.DESCRIPTION
  This is an advisory maintenance check, deliberately separate from release gates. It reads
  the staged DLL version resources, then fetches only the official zlib.net and libpng.org
  release pages. A newer release returns exit code 1; it is not a build failure. Network
  failures return 3 so callers can distinguish an unavailable check from an out-of-date DLL.

.EXAMPLE
  pwsh scripts/check-magick-dependency-freshness.ps1

.EXAMPLE
  pwsh scripts/check-magick-dependency-freshness.ps1 -ZlibPagePath test/fixtures/zlib.html -LibpngPagePath test/fixtures/libpng.html
#>
[CmdletBinding()]
param(
    [string]$BundlePath = (Join-Path (Split-Path $PSScriptRoot -Parent) 'packaging\stage\x64\magick'),

    # Local fixture paths are intentionally supported for deterministic offline tests.
    [string]$ZlibPagePath,
    [string]$LibpngPagePath,

    # Reserved for deterministic/offline tests. Supply both or neither; normal use always
    # reads FileVersion resources from the staged DLLs above.
    [string]$BundledZlibVersion,
    [string]$BundledLibpngVersion,

    [switch]$PassThru,
    [switch]$LoadOnly
)

$ErrorActionPreference = 'Stop'

$script:ZlibUrl = 'https://zlib.net/'
$script:LibpngUrl = 'https://libpng.org/pub/png/libpng.html'

function ConvertTo-DependencyVersion {
    param(
        [Parameter(Mandatory)][string]$Value,
        [Parameter(Mandatory)][string]$Context
    )

    $match = [regex]::Match($Value, '^(?<core>0|[1-9]\d*(?:\.(?:0|[1-9]\d*)){2})(?:-(?<pre>[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$')
    if (-not $match.Success) {
        throw "$Context is not a semantic version: '$Value'"
    }

    [pscustomobject]@{
        Raw = $Value
        Core = [int64[]]@($match.Groups['core'].Value.Split('.') | ForEach-Object { [int64]$_ })
        PreRelease = $match.Groups['pre'].Value
    }
}

function Compare-DependencyVersion {
    param(
        [Parameter(Mandatory)]$Left,
        [Parameter(Mandatory)]$Right
    )

    for ($i = 0; $i -lt 3; $i++) {
        if ($Left.Core[$i] -lt $Right.Core[$i]) { return -1 }
        if ($Left.Core[$i] -gt $Right.Core[$i]) { return 1 }
    }
    if ([string]::IsNullOrEmpty($Left.PreRelease) -and -not [string]::IsNullOrEmpty($Right.PreRelease)) { return 1 }
    if (-not [string]::IsNullOrEmpty($Left.PreRelease) -and [string]::IsNullOrEmpty($Right.PreRelease)) { return -1 }
    if ([string]::IsNullOrEmpty($Left.PreRelease)) { return 0 }

    $leftParts = $Left.PreRelease.Split('.')
    $rightParts = $Right.PreRelease.Split('.')
    $count = [Math]::Min($leftParts.Count, $rightParts.Count)
    for ($i = 0; $i -lt $count; $i++) {
        if ($leftParts[$i] -ceq $rightParts[$i]) { continue }
        $leftNumeric = $leftParts[$i] -match '^\d+$'
        $rightNumeric = $rightParts[$i] -match '^\d+$'
        if ($leftNumeric -and $rightNumeric) {
            $numericComparison = ([int64]$leftParts[$i]).CompareTo([int64]$rightParts[$i])
            if ($numericComparison -ne 0) { return $numericComparison }
        } elseif ($leftNumeric) {
            return -1
        } elseif ($rightNumeric) {
            return 1
        } else {
            $textComparison = [string]::CompareOrdinal($leftParts[$i], $rightParts[$i])
            if ($textComparison -ne 0) { return [Math]::Sign($textComparison) }
        }
    }
    return $leftParts.Count.CompareTo($rightParts.Count)
}

function Get-HighestDependencyVersion {
    param(
        [Parameter(Mandatory)][AllowEmptyCollection()][string[]]$Candidates,
        [Parameter(Mandatory)][string]$Name
    )

    if ($Candidates.Count -eq 0) { throw "Could not find a $Name release version on its official page" }
    $highest = ConvertTo-DependencyVersion -Value $Candidates[0] -Context "$Name release"
    foreach ($candidate in $Candidates | Select-Object -Skip 1) {
        $parsed = ConvertTo-DependencyVersion -Value $candidate -Context "$Name release"
        if ((Compare-DependencyVersion -Left $parsed -Right $highest) -gt 0) { $highest = $parsed }
    }
    return $highest
}

function Get-UpstreamDependencyVersion {
    param(
        [Parameter(Mandatory)][ValidateSet('zlib', 'libpng')][string]$Name,
        [Parameter(Mandatory)][string]$Html
    )

    # Match the published library name next to its version; deliberately do not scrape
    # arbitrary x.y.z strings from HTML, which would turn old changelog entries into false alerts.
    $pattern = if ($Name -eq 'zlib') {
        '(?i)\bzlib(?:\s+(?:version|release))?[\s:=-]+v?(\d+\.\d+\.\d+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?)\b'
    } else {
        '(?i)\blibpng(?:\s+(?:version|release))?[\s:=-]+v?(\d+\.\d+\.\d+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?)\b'
    }
    $candidates = @([regex]::Matches($Html, $pattern) | ForEach-Object { $_.Groups[1].Value })
    return Get-HighestDependencyVersion -Candidates $candidates -Name $Name
}

function Get-DependencyPage {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string]$OfficialUrl,
        [string]$FixturePath
    )

    if ($FixturePath) {
        if (-not (Test-Path -LiteralPath $FixturePath -PathType Leaf)) {
            throw "Fixture page for $Name does not exist: $FixturePath"
        }
        return Get-Content -LiteralPath $FixturePath -Raw
    }
    try {
        return (Invoke-WebRequest -Uri $OfficialUrl -MaximumRedirection 3 -TimeoutSec 20 -UseBasicParsing).Content
    } catch {
        $exception = [System.Exception]::new("Could not fetch official $Name release page ($OfficialUrl): $($_.Exception.Message)", $_.Exception)
        $exception.Data['DependencyFreshnessNetworkFailure'] = $true
        throw $exception
    }
}

function Get-BundledDependencyVersion {
    param(
        [Parameter(Mandatory)][string]$BundleRoot,
        [Parameter(Mandatory)][string]$DllName,
        [Parameter(Mandatory)][string]$Name
    )

    $path = Join-Path $BundleRoot $DllName
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Bundled $Name DLL is missing: $path"
    }
    $fileVersion = (Get-Item -LiteralPath $path).VersionInfo.FileVersion
    $match = [regex]::Match([string]$fileVersion, '\b(\d+\.\d+\.\d+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?)\b')
    if (-not $match.Success) { throw "Bundled $Name DLL has no parseable semantic FileVersion: '$fileVersion'" }
    return ConvertTo-DependencyVersion -Value $match.Groups[1].Value -Context "bundled $Name DLL"
}

if ($LoadOnly) { return }

try {
    $hasBundledVersionOverrides = -not [string]::IsNullOrWhiteSpace($BundledZlibVersion) -or
        -not [string]::IsNullOrWhiteSpace($BundledLibpngVersion)
    if ($hasBundledVersionOverrides -and (
            [string]::IsNullOrWhiteSpace($BundledZlibVersion) -or
            [string]::IsNullOrWhiteSpace($BundledLibpngVersion)
        )) {
        throw 'BundledZlibVersion and BundledLibpngVersion must be supplied together for deterministic testing'
    }
    $resolvedBundle = if ($hasBundledVersionOverrides) { $null } else { (Resolve-Path -LiteralPath $BundlePath).Path }
    $dependencies = @(
        [pscustomobject]@{ Name = 'zlib'; Dll = 'CORE_RL_zlib_.dll'; Url = $script:ZlibUrl; Fixture = $ZlibPagePath; BundledVersion = $BundledZlibVersion },
        [pscustomobject]@{ Name = 'libpng'; Dll = 'CORE_RL_png_.dll'; Url = $script:LibpngUrl; Fixture = $LibpngPagePath; BundledVersion = $BundledLibpngVersion }
    )
    $results = foreach ($dependency in $dependencies) {
        $bundled = if ($hasBundledVersionOverrides) {
            ConvertTo-DependencyVersion -Value $dependency.BundledVersion -Context "test bundled $($dependency.Name) version"
        } else {
            Get-BundledDependencyVersion -BundleRoot $resolvedBundle -DllName $dependency.Dll -Name $dependency.Name
        }
        $html = Get-DependencyPage -Name $dependency.Name -OfficialUrl $dependency.Url -FixturePath $dependency.Fixture
        $upstream = Get-UpstreamDependencyVersion -Name $dependency.Name -Html $html
        [pscustomobject]@{
            Name = $dependency.Name
            Dll = $dependency.Dll
            BundledVersion = $bundled.Raw
            UpstreamVersion = $upstream.Raw
            IsCurrent = (Compare-DependencyVersion -Left $bundled -Right $upstream) -ge 0
        }
    }
} catch {
    if ($_.Exception.Data['DependencyFreshnessNetworkFailure']) {
        Write-Error "[magick-freshness] NETWORK UNAVAILABLE: $($_.Exception.Message)"
        exit 3
    }
    Write-Error "[magick-freshness] CHECK FAILED: $($_.Exception.Message)"
    exit 2
}

$outdated = @($results | Where-Object { -not $_.IsCurrent })
foreach ($result in $results) {
    $state = if ($result.IsCurrent) { 'current' } else { 'newer upstream release available' }
    Write-Host "[magick-freshness] $($result.Name): bundled $($result.BundledVersion), upstream $($result.UpstreamVersion) ($state)"
}
if ($PassThru) { $results }
if ($outdated.Count -gt 0) { exit 1 }
exit 0
