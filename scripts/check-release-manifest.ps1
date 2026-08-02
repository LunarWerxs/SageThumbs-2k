<#
  Fail-closed publication gate for the manifest written by
  write-release-manifest.ps1. It verifies the exact source commit and inputs,
  required full-build flags, PE metadata, every staged file, the installer hash,
  and both release size ceilings.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$InstallerPath,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$StagePath,

    [Parameter(Mandatory)]
    [ValidatePattern('^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$')]
    [string]$ExpectedVersion,

    [Parameter(Mandatory)]
    [ValidatePattern('^[0-9a-fA-F]{40}$')]
    [string]$ExpectedCommitSha,

    # Keep x64 as the compatibility default for existing manifests and release
    # callers.  The selected architecture, not an installer filename, determines
    # the expected installer and Rust target.
    [ValidateSet('x64', 'arm64')]
    [string]$Architecture = 'x64',

    [string]$ManifestPath
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'release-manifest-lib.ps1')

$installer = Get-Item -LiteralPath $InstallerPath -ErrorAction Stop
$stage = Get-Item -LiteralPath $StagePath -ErrorAction Stop
if (-not $stage.PSIsContainer) {
    throw "release stage is not a directory: $StagePath"
}
if (-not $ManifestPath) {
    $ManifestPath = Join-Path $installer.DirectoryName "$($installer.BaseName).release.json"
}
if (-not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) {
    throw "release provenance manifest not found: $ManifestPath"
}

try {
    $manifest = Get-Content -LiteralPath $ManifestPath -Raw | ConvertFrom-Json
} catch {
    throw "release provenance manifest is not valid JSON: $ManifestPath`n$($_.Exception.Message)"
}
if ($null -eq $manifest) {
    throw "release provenance manifest is empty: $ManifestPath"
}

$targetSpec = switch ($Architecture) {
    'x64' {
        [pscustomobject]@{
            RustTarget = 'x86_64-pc-windows-msvc'
            InstallerName = "SageThumbs2K-Setup-$ExpectedVersion.exe"
        }
    }
    'arm64' {
        [pscustomobject]@{
            RustTarget = 'aarch64-pc-windows-msvc'
            InstallerName = "SageThumbs2K-Setup-$ExpectedVersion-arm64.exe"
        }
    }
    default { throw "unsupported release architecture: $Architecture" }
}

$schema = [int64](Get-ReleaseRequiredProperty -Object $manifest -Name 'schemaVersion')
if ($schema -ne 1) {
    throw "unsupported release manifest schemaVersion $schema (expected 1)"
}
if ([string](Get-ReleaseRequiredProperty -Object $manifest -Name 'product') -cne 'SageThumbs 2K') {
    throw 'release manifest has the wrong product'
}
if ([string](Get-ReleaseRequiredProperty -Object $manifest -Name 'version') -cne $ExpectedVersion) {
    throw "release manifest version does not match $ExpectedVersion"
}
# `architecture` was added after schema v1 shipped. Treat an omitted value as the
# original x64 contract so existing manifests retain their valid meaning; every new
# writer invocation records it explicitly.
$manifestArchitectureProperty = $manifest.PSObject.Properties['architecture']
$manifestArchitecture = if ($null -eq $manifestArchitectureProperty) {
    'x64'
} else {
    [string]$manifestArchitectureProperty.Value
}
if ($manifestArchitecture -notin @('x64', 'arm64')) {
    throw "release manifest has invalid architecture: '$manifestArchitecture'"
}
if ($manifestArchitecture -cne $Architecture) {
    throw "release manifest architecture '$manifestArchitecture' does not match expected '$Architecture'"
}

$manifestCommit = ([string](Get-ReleaseRequiredProperty -Object $manifest -Name 'commitSha')).ToLowerInvariant()
$expectedCommit = $ExpectedCommitSha.ToLowerInvariant()
if ($manifestCommit -cne $expectedCommit) {
    throw "release manifest was built from $manifestCommit, not validated commit $expectedCommit"
}

$createdUtcValue = Get-ReleaseRequiredProperty -Object $manifest -Name 'createdUtc'
$createdUtc = [DateTime]::MinValue
if ($createdUtcValue -is [DateTime]) {
    # PowerShell 7.6+ materializes ISO JSON dates as DateTime. Casting that value
    # back to string drops the trailing Z under some cultures, causing a second
    # parse to treat UTC as local time and falsely report a future timestamp.
    $createdUtc = ([DateTime]$createdUtcValue).ToUniversalTime()
    $createdUtcText = ([DateTime]$createdUtcValue).ToString('o')
} elseif ($createdUtcValue -is [string]) {
    $createdUtcText = [string]$createdUtcValue
    if (-not [DateTime]::TryParse(
            $createdUtcText,
            [Globalization.CultureInfo]::InvariantCulture,
            [Globalization.DateTimeStyles]::RoundtripKind,
            [ref]$createdUtc
        )) {
        throw "release manifest has invalid createdUtc: $createdUtcText"
    }
    $createdUtc = $createdUtc.ToUniversalTime()
} else {
    $createdUtcType = if ($null -eq $createdUtcValue) { 'null' } else { $createdUtcValue.GetType().FullName }
    throw "release manifest has invalid createdUtc type: $createdUtcType"
}
if ($createdUtc -gt [DateTime]::UtcNow.AddMinutes(5)) {
    throw "release manifest creation time is in the future: $createdUtcText"
}

if ((Get-ReleaseRequiredProperty -Object $manifest -Name 'sourceTreeClean') -isnot [bool] -or
    -not [bool]$manifest.sourceTreeClean) {
    throw 'release manifest was produced from a dirty source tree'
}
if ((Get-ReleaseRequiredProperty -Object $manifest -Name 'publishable') -isnot [bool] -or
    -not [bool]$manifest.publishable) {
    $reasons = @($manifest.publishableReasons) -join '; '
    throw "release manifest marks this artifact non-publishable: $reasons"
}
if (@(Get-ReleaseRequiredProperty -Object $manifest -Name 'publishableReasons').Count -ne 0) {
    throw 'release manifest is internally inconsistent: publishable artifact has rejection reasons'
}

$build = Get-ReleaseRequiredProperty -Object $manifest -Name 'build'
foreach ($flag in 'rustBuildPerformed', 'modernMenuBundled') {
    $value = Get-ReleaseRequiredProperty -Object $build -Name $flag
    if ($value -isnot [bool] -or -not [bool]$value) {
        throw "release manifest requires full-build flag '$flag'"
    }
}
$imageMagickBundled = Get-ReleaseRequiredProperty -Object $build -Name 'imageMagickBundled'
# Full/Compact is a payload choice on BOTH architectures now, so the manifest must
# agree with the STAGE, not with the architecture.
$magickStaged = Test-Path -LiteralPath (Join-Path $stage.FullName 'magick') -PathType Container
if ($imageMagickBundled -isnot [bool] -or [bool]$imageMagickBundled -ne $magickStaged) {
    $requiredImageMagick = if ($magickStaged) { 'true (Full)' } else { 'false (Compact)' }
    throw "release manifest ImageMagickBundled disagrees with the stage: requires ImageMagickBundled=$requiredImageMagick for this $Architecture stage"
}
if ((Get-ReleaseRequiredProperty -Object $build -Name 'cargoLocked') -isnot [bool] -or
    -not [bool]$build.cargoLocked) {
    throw 'release manifest was not produced by locked Cargo builds'
}
if ([string](Get-ReleaseRequiredProperty -Object $build -Name 'rustTarget') -cne $targetSpec.RustTarget) {
    throw "release manifest has wrong Rust target: $($build.rustTarget)"
}
if ([string](Get-ReleaseRequiredProperty -Object $build -Name 'rustFlags') -cne '-C target-feature=+crt-static') {
    throw "release manifest has wrong RUSTFLAGS: '$($build.rustFlags)'"
}
$toolchain = Get-ReleaseRequiredProperty -Object $build -Name 'toolchain'
$cargoVersion = [string](Get-ReleaseRequiredProperty -Object $toolchain -Name 'cargo')
if ($cargoVersion -notmatch '^cargo\s+\d+\.\d+\.\d+\b') {
    throw "release manifest has invalid Cargo toolchain provenance: '$cargoVersion'"
}
$rustcVerbose = @((Get-ReleaseRequiredProperty -Object $toolchain -Name 'rustcVerbose'))
if (-not ($rustcVerbose -match '^rustc\s+\d+\.\d+\.\d+') -or
    -not ($rustcVerbose -match '^host:\s*[A-Za-z0-9_-]+(?:-[A-Za-z0-9_-]+){2,}\s*$')) {
    throw 'release manifest has invalid rustc toolchain provenance'
}

function Assert-ExactStringArray([object]$ActualObject, [string[]]$Expected, [string]$Name) {
    $actual = @($ActualObject)
    if ($actual.Count -ne $Expected.Count) {
        throw "$Name count changed: expected $($Expected.Count), got $($actual.Count)"
    }
    for ($i = 0; $i -lt $Expected.Count; $i++) {
        if ([string]$actual[$i] -cne $Expected[$i]) {
            throw "$Name changed at argument $i`: expected '$($Expected[$i])', got '$($actual[$i])'"
        }
    }
}

$cargoBuild = Get-ReleaseRequiredProperty -Object $build -Name 'cargo'
$exeBuild = Get-ReleaseRequiredProperty -Object $cargoBuild -Name 'executables'
$dllBuild = Get-ReleaseRequiredProperty -Object $cargoBuild -Name 'shellExtension'
if ([string](Get-ReleaseRequiredProperty -Object $exeBuild -Name 'package') -cne 'sagethumbs2k') {
    throw 'release manifest has the wrong executable package'
}
if ([string](Get-ReleaseRequiredProperty -Object $dllBuild -Name 'package') -cne 'sagethumbs2k-dll') {
    throw 'release manifest has the wrong shell-extension package'
}
# These four expectations are DELIBERATELY a second, independent copy of the shipping
# build recipe: their whole job is to refuse a manifest whose build silently gained or
# lost a feature. So when the recipe in build-release.ps1 changes, it must be changed
# HERE and in write-release-manifest.ps1 too. A release that fails at this gate is this
# working as designed; fix the recipe copies, do not weaken the assertion.
Assert-ExactStringArray `
    -ActualObject (Get-ReleaseRequiredProperty -Object $exeBuild -Name 'features') `
    -Expected @('webp-lossy', 'html-preview', 'hdr-capture') `
    -Name 'executable feature set'
Assert-ExactStringArray `
    -ActualObject (Get-ReleaseRequiredProperty -Object $dllBuild -Name 'features') `
    -Expected @('webp-lossy', 'dll-i18n-subset') `
    -Name 'shell-extension feature set'
Assert-ExactStringArray `
    -ActualObject (Get-ReleaseRequiredProperty -Object $exeBuild -Name 'arguments') `
    -Expected @(Get-ReleaseCargoBuildArguments -Architecture $Architecture -Package sagethumbs2k -Features 'webp-lossy,html-preview,hdr-capture') `
    -Name 'executable Cargo arguments'
Assert-ExactStringArray `
    -ActualObject (Get-ReleaseRequiredProperty -Object $dllBuild -Name 'arguments') `
    -Expected @(Get-ReleaseCargoBuildArguments -Architecture $Architecture -Package sagethumbs2k-dll -Features 'webp-lossy,dll-i18n-subset') `
    -Name 'shell-extension Cargo arguments'

$head = (& git -C $root rev-parse HEAD)
if ($LASTEXITCODE -ne 0 -or -not $head) {
    throw 'could not resolve current source commit'
}
$head = $head.Trim().ToLowerInvariant()
if ($head -cne $expectedCommit) {
    throw "current HEAD $head is not validated commit $expectedCommit"
}
$status = @(& git -C $root status --porcelain --untracked-files=normal)
if ($LASTEXITCODE -ne 0) {
    throw 'could not inspect current source tree'
}
if ($status.Count -ne 0) {
    throw 'current source tree is dirty; refusing to validate a release artifact'
}

$inputs = @(Get-ReleaseRequiredProperty -Object $manifest -Name 'inputs')
$requiredInputPaths = @(Get-ReleaseRequiredInputPaths)
Assert-ReleaseRequiredInputsTracked -Root $root -RelativePaths $requiredInputPaths
if ($inputs.Count -ne $requiredInputPaths.Count) {
    throw "release manifest source-input count changed: expected $($requiredInputPaths.Count), got $($inputs.Count)"
}
$manifestInputPaths = @(
    $inputs | ForEach-Object {
        [string](Get-ReleaseRequiredProperty -Object $_ -Name 'path')
    }
)
for ($i = 0; $i -lt $requiredInputPaths.Count; $i++) {
    if ($manifestInputPaths[$i] -cne $requiredInputPaths[$i]) {
        throw "release manifest source input $i changed: expected '$($requiredInputPaths[$i])', got '$($manifestInputPaths[$i])'"
    }
}
foreach ($expectedRecord in $inputs) {
    $relative = [string](Get-ReleaseRequiredProperty -Object $expectedRecord -Name 'path')
    $path = Get-ReleasePathUnderRoot -Root $root -RelativePath $relative
    $actualRecord = Get-ReleaseFileRecord -Path $path -RelativeTo $root
    Assert-ReleaseRecordMatches -Expected $expectedRecord -Actual $actualRecord -Context "source input '$relative'"
}

# Reassert that Cargo still resolves the reviewed local exr patch and that its
# upstream/intentional-file provenance fingerprints match.
& (Join-Path $PSScriptRoot 'check-vendored-exr.ps1')

$expectedInstallerName = $targetSpec.InstallerName
if ($installer.Name -cne $expectedInstallerName) {
    throw "installer filename mismatch: '$($installer.Name)' (expected '$expectedInstallerName')"
}
Assert-ReleasePeMetadata -Path $installer.FullName -Version $ExpectedVersion -Description 'SageThumbs 2K Setup'
$actualInstaller = Get-ReleaseFileRecord -Path $installer.FullName -RelativeTo $installer.DirectoryName
$expectedInstaller = Get-ReleaseRequiredProperty -Object $manifest -Name 'installer'
Assert-ReleaseRecordMatches -Expected $expectedInstaller -Actual $actualInstaller -Context 'installer'

$expectedStage = Get-ReleaseRequiredProperty -Object $manifest -Name 'stage'
$expectedFiles = @(Get-ReleaseRequiredProperty -Object $expectedStage -Name 'files')
$actualFiles = @(Get-ReleaseStageInventory -StagePath $stage.FullName)
if ($expectedFiles.Count -ne $actualFiles.Count) {
    throw "release stage file count changed: expected $($expectedFiles.Count), got $($actualFiles.Count)"
}
for ($i = 0; $i -lt $expectedFiles.Count; $i++) {
    Assert-ReleaseRecordMatches `
        -Expected $expectedFiles[$i] `
        -Actual $actualFiles[$i] `
        -Context "staged file '$($expectedFiles[$i].path)'"
}

$rustBytes = [int64]0
foreach ($name in 'sagethumbs2k.dll', 'SageThumbs2K.exe', 'st2k.exe') {
    $path = Join-Path $stage.FullName $name
    Assert-ReleasePeMetadata -Path $path -Version $ExpectedVersion
    Assert-ReleasePeArchitecture -Path $path -Architecture $Architecture
    $rustBytes += [int64](Get-Item -LiteralPath $path).Length
}
if ($rustBytes -ne [int64](Get-ReleaseRequiredProperty -Object $expectedStage -Name 'rustPayloadBytes')) {
    throw 'staged Rust payload total does not match the release manifest'
}

$magickFiles = @($actualFiles | Where-Object { $_.path.StartsWith('magick/', [StringComparison]::OrdinalIgnoreCase) })
$magickBytes = [int64]0
foreach ($file in $magickFiles) { $magickBytes += [int64]$file.bytes }
if ($magickFiles.Count -ne [int64](Get-ReleaseRequiredProperty -Object $expectedStage -Name 'imageMagickFileCount') -or
    $magickBytes -ne [int64](Get-ReleaseRequiredProperty -Object $expectedStage -Name 'imageMagickBytes')) {
    throw 'ImageMagick payload total does not match the release manifest'
}
# Both architectures ship Full now, so the payload must be PRESENT for either unless the
# build was explicitly Compact. Keyed on what the manifest claims, not on the architecture.
if ($imageMagickBundled) {
    $magickExe = Join-Path $stage.FullName 'magick\magick.exe'
    Assert-ReleasePeFile -Path $magickExe
    if ($magickFiles.Count -eq 0) {
        throw 'full ImageMagick payload is absent'
    }
} elseif ($magickFiles.Count -ne 0 -or (Test-Path -LiteralPath (Join-Path $stage.FullName 'magick'))) {
    throw 'Compact stage must not contain ImageMagick files'
}

# Re-run the pinned-source identity/inventory gate and the staged dependency-
# closure + real decode smoke tests for the x64 Full payload. ARM64 Compact
# intentionally has no ImageMagick and cannot execute on this x64 verifier.
if ($Architecture -eq 'x64') {
    $magickPinPath = Join-Path $root 'packaging\imagemagick-source.json'
    $magickPin = Get-Content -LiteralPath $magickPinPath -Raw | ConvertFrom-Json
    $sourceDirectoryName = [string](Get-ReleaseRequiredProperty -Object $magickPin.identity -Name 'installDirectoryName')
    $magickSource = Join-Path $env:ProgramFiles $sourceDirectoryName
    & (Join-Path $PSScriptRoot 'check-magick-source.ps1') -SourcePath $magickSource -PinPath $magickPinPath
    & (Join-Path $PSScriptRoot 'check-magick-bundle.ps1') -BundlePath (Join-Path $stage.FullName 'magick')
    & (Join-Path $PSScriptRoot 'test-staged-regression.ps1') -StagePath $stage.FullName
}

foreach ($required in 'SageThumbs2K.msix', 'SageThumbs2K.cer') {
    $path = Join-Path $stage.FullName $required
    if (-not (Test-Path -LiteralPath $path -PathType Leaf) -or
        (Get-Item -LiteralPath $path).Length -eq 0) {
        throw "full modern-menu payload is missing: $path"
    }
}
Assert-ReleaseMsixPackage `
    -Path (Join-Path $stage.FullName 'SageThumbs2K.msix') `
    -CertificatePath (Join-Path $stage.FullName 'SageThumbs2K.cer') `
    -Version $ExpectedVersion `
    -ExpectedProcessorArchitecture $(if ($Architecture -eq 'arm64') { 'arm64' } else { 'neutral' })

$sizeCheck = Join-Path $PSScriptRoot 'check-release-size.ps1'
& $sizeCheck -InstallerPath $installer.FullName -StagePath $stage.FullName -Architecture $Architecture

Write-Host '[manifest] release provenance and artifact integrity passed.' -ForegroundColor Green
