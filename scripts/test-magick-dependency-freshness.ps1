<#
.SYNOPSIS
  Offline parser and version-ordering tests for check-magick-dependency-freshness.ps1.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$check = Join-Path $PSScriptRoot 'check-magick-dependency-freshness.ps1'
. $check -LoadOnly

$passed = 0
function Assert-Equal {
    param([string]$Name, $Actual, $Expected)
    if ($Actual -cne $Expected) { throw "${Name}: expected '$Expected', got '$Actual'" }
    $script:passed++
    Write-Host "  PASS $Name" -ForegroundColor Green
}
function Assert-ThrowsLike {
    param([string]$Name, [scriptblock]$Action, [string]$Pattern)
    try {
        & $Action
        throw "Expected failure did not occur: $Name"
    } catch {
        if ($_.Exception.Message -notmatch $Pattern) { throw "${Name}: expected /$Pattern/, got '$($_.Exception.Message)'" }
        $script:passed++
        Write-Host "  PASS $Name" -ForegroundColor Green
    }
}

$zlibFixture = @'
<html><body><h1>zlib 1.3.2</h1><p>Older zlib 1.2.13 remains documented here.</p></body></html>
'@
$pngFixture = @'
<html><body><p>The latest public release is libpng version 1.6.58.</p><p>libpng 1.6.40 is old.</p></body></html>
'@

Assert-Equal -Name 'zlib parser selects named latest version' -Actual (Get-UpstreamDependencyVersion -Name zlib -Html $zlibFixture).Raw -Expected '1.3.2'
Assert-Equal -Name 'libpng parser selects named latest version' -Actual (Get-UpstreamDependencyVersion -Name libpng -Html $pngFixture).Raw -Expected '1.6.58'
Assert-Equal -Name 'stable release ranks above prerelease' -Actual (Compare-DependencyVersion (ConvertTo-DependencyVersion '1.6.58' 'test') (ConvertTo-DependencyVersion '1.6.58-rc.1' 'test')) -Expected 1
Assert-Equal -Name 'numeric prerelease identifiers compare numerically' -Actual (Compare-DependencyVersion (ConvertTo-DependencyVersion '1.6.58-rc.10' 'test') (ConvertTo-DependencyVersion '1.6.58-rc.2' 'test')) -Expected 1
Assert-Equal -Name 'higher patch ranks newer' -Actual (Compare-DependencyVersion (ConvertTo-DependencyVersion '1.3.3' 'test') (ConvertTo-DependencyVersion '1.3.2' 'test')) -Expected 1
Assert-ThrowsLike -Name 'unnamed version is rejected' -Pattern 'Could not find' -Action { Get-UpstreamDependencyVersion -Name zlib -Html '<p>latest is 1.3.2</p>' }
Assert-ThrowsLike -Name 'invalid semver is rejected' -Pattern 'not a semantic version' -Action { ConvertTo-DependencyVersion -Value '1.3' -Context test }

$temp = Join-Path ([System.IO.Path]::GetTempPath()) ("st2k-magick-freshness-tests-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $temp | Out-Null
try {
    $zlibPath = Join-Path $temp 'zlib.html'
    $pngPath = Join-Path $temp 'libpng.html'
    Set-Content -LiteralPath $zlibPath -Value $zlibFixture -Encoding utf8NoBOM
    Set-Content -LiteralPath $pngPath -Value $pngFixture -Encoding utf8NoBOM

    # A nonexistent bundle proves this suite does not depend on ignored staged artifacts.
    & $check -BundlePath (Join-Path $temp 'missing-magick-bundle') -ZlibPagePath $zlibPath -LibpngPagePath $pngPath `
        -BundledZlibVersion '1.3.2' -BundledLibpngVersion '1.6.58'
    Assert-Equal -Name 'offline current check needs no staged bundle and exits zero' -Actual $LASTEXITCODE -Expected 0

    Set-Content -LiteralPath $zlibPath -Value '<p>zlib 1.3.3</p>' -Encoding utf8NoBOM
    & $check -BundlePath (Join-Path $temp 'missing-magick-bundle') -ZlibPagePath $zlibPath -LibpngPagePath $pngPath `
        -BundledZlibVersion '1.3.2' -BundledLibpngVersion '1.6.58'
    Assert-Equal -Name 'offline newer release exits one' -Actual $LASTEXITCODE -Expected 1
} finally {
    if (Test-Path -LiteralPath $temp) { [System.IO.Directory]::Delete($temp, $true) }
}

if ($passed -ne 9) { throw "Test accounting error: expected 9 passes, got $passed" }
Write-Host "[magick-freshness-tests] PASS $passed/9" -ForegroundColor Green

# EXPLICIT, and load-bearing. The last thing this suite does is assert the checker exits
# ONE for a newer upstream release, which leaves $LASTEXITCODE = 1. Run via `pwsh -File`
# that is harmless, because the process exit code is the script's own. But CI runs these
# with `shell: pwsh`, which invokes the script IN-SESSION and then does
# `exit $LASTEXITCODE` - so the leaked 1 became the step's exit code and turned a fully
# passing suite red. Never end this file on a bare native/script call.
exit 0
