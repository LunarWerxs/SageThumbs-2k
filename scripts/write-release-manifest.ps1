<#
  Records the exact source inputs, staged payload, and installer produced by a
  release build. The resulting manifest is ignored with dist/, but release.ps1
  requires it and independently revalidates every hash before publishing.

  Incomplete/dev builds still receive a manifest, but publishable=false prevents
  -SkipBuild from turning them into a public release.
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
    [string]$Version,

    [Parameter(Mandatory)]
    [bool]$ImageMagickBundled,

    [Parameter(Mandatory)]
    [bool]$ModernMenuBundled,

    [Parameter(Mandatory)]
    [bool]$RustBuildPerformed,

    [Parameter(Mandatory)]
    [string[]]$ExeCargoArguments,

    [Parameter(Mandatory)]
    [string[]]$DllCargoArguments,

    # Artifact naming and Rust-target provenance are selected deliberately by the
    # release caller.  Do not infer an architecture from a filename: a stale
    # same-version installer must never change the target a manifest attests to.
    [ValidateSet('x64', 'arm64')]
    [string]$Architecture = 'x64',

    [string]$OutputPath
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'release-manifest-lib.ps1')

$installer = Get-Item -LiteralPath $InstallerPath -ErrorAction Stop
$stage = Get-Item -LiteralPath $StagePath -ErrorAction Stop
if (-not $stage.PSIsContainer) {
    throw "release stage is not a directory: $StagePath"
}
if (-not $OutputPath) {
    $OutputPath = Join-Path $installer.DirectoryName "$($installer.BaseName).release.json"
}

$targetSpec = switch ($Architecture) {
    'x64' {
        [pscustomobject]@{
            RustTarget = 'x86_64-pc-windows-msvc'
            InstallerName = "SageThumbs2K-Setup-$Version.exe"
        }
    }
    'arm64' {
        [pscustomobject]@{
            RustTarget = 'aarch64-pc-windows-msvc'
            InstallerName = "SageThumbs2K-Setup-$Version-arm64.exe"
        }
    }
    default { throw "unsupported release architecture: $Architecture" }
}
$expectedInstallerName = $targetSpec.InstallerName
if ($installer.Name -cne $expectedInstallerName) {
    throw "installer filename mismatch: '$($installer.Name)' (expected '$expectedInstallerName')"
}
Assert-ReleasePeMetadata -Path $installer.FullName -Version $Version -Description 'SageThumbs 2K Setup'

$rustNames = @('sagethumbs2k.dll', 'SageThumbs2K.exe', 'st2k.exe')
$rustBytes = [int64]0
foreach ($name in $rustNames) {
    $path = Join-Path $stage.FullName $name
    Assert-ReleasePeMetadata -Path $path -Version $Version
    Assert-ReleasePeArchitecture -Path $path -Architecture $Architecture
    $length = [int64](Get-Item -LiteralPath $path).Length
    if ($rustBytes -gt [int64]::MaxValue - $length) {
        throw 'staged Rust payload byte count overflowed Int64'
    }
    $rustBytes += $length
}

$magickExe = Join-Path $stage.FullName 'magick\magick.exe'
$magickPresent = Test-Path -LiteralPath $magickExe -PathType Leaf
if ($ImageMagickBundled) {
    if (-not $magickPresent) {
        throw "ImageMagick was declared bundled but magick.exe is missing: $magickExe"
    }
    Assert-ReleasePeFile -Path $magickExe
}
$magickDirectory = Join-Path $stage.FullName 'magick'
if ($Architecture -eq 'arm64' -and ($ImageMagickBundled -or (Test-Path -LiteralPath $magickDirectory))) {
    throw 'ARM64 Compact releases must not stage an ImageMagick payload'
}

$msixPath = Join-Path $stage.FullName 'SageThumbs2K.msix'
$cerPath = Join-Path $stage.FullName 'SageThumbs2K.cer'
$modernPresent = (Test-Path -LiteralPath $msixPath -PathType Leaf) -and
    (Test-Path -LiteralPath $cerPath -PathType Leaf)
if ($ModernMenuBundled -and -not $modernPresent) {
    throw 'modern menu was declared bundled but SageThumbs2K.msix or SageThumbs2K.cer is missing'
}
if ($ModernMenuBundled) {
    Assert-ReleaseMsixPackage `
        -Path $msixPath `
        -CertificatePath $cerPath `
        -Version $Version `
        -ExpectedProcessorArchitecture $(if ($Architecture -eq 'arm64') { 'arm64' } else { 'neutral' })
}

$gitHead = (& git -C $root rev-parse HEAD)
if ($LASTEXITCODE -ne 0 -or -not $gitHead) {
    throw 'could not resolve the release source commit'
}
$gitHead = $gitHead.Trim()
if ($gitHead -notmatch '^[0-9a-fA-F]{40}$') {
    throw "invalid release source commit: $gitHead"
}
$gitStatus = @(& git -C $root status --porcelain --untracked-files=normal)
if ($LASTEXITCODE -ne 0) {
    throw 'could not inspect the release source tree'
}
$sourceTreeClean = $gitStatus.Count -eq 0

# `hdr-capture` is APP-ONLY (see Cargo.toml): it links D3D11/DXGI, which the
# shell DLL must never load. These canonical helpers are shared with the build
# runner and checker so their exact ARM target ordering cannot drift.
$expectedExeArguments = @(Get-ReleaseCargoBuildArguments -Architecture $Architecture -Package sagethumbs2k -Features 'webp-lossy,html-preview,hdr-capture')
$expectedDllArguments = @(Get-ReleaseCargoBuildArguments -Architecture $Architecture -Package sagethumbs2k-dll -Features 'webp-lossy,dll-i18n-subset')
function Test-ExactArguments([string[]]$Actual, [string[]]$Expected) {
    if ($Actual.Count -ne $Expected.Count) { return $false }
    for ($i = 0; $i -lt $Expected.Count; $i++) {
        if ($Actual[$i] -cne $Expected[$i]) { return $false }
    }
    return $true
}

# DERIVE the recorded feature list from the recorded argument list. Never write the
# features out a second time by hand: two hand-maintained copies of one truth drift,
# and one prior manifest did: its recorded features omitted `hdr-capture` even
# though its own argument line included `webp-lossy,html-preview,hdr-capture`, because
# only the argument constant was updated when HDR capture was gated behind a feature.
# The release gate caught the contradiction and refused to publish, which is the
# system working; this makes the contradiction unrepresentable instead.
function Get-CargoFeatureList([string[]]$Arguments) {
    $i = [Array]::IndexOf($Arguments, '--features')
    if ($i -lt 0 -or $i + 1 -ge $Arguments.Count) { return @() }
    $value = [string]$Arguments[$i + 1]
    return @($value -split ',' | Where-Object { $_ })
}

$rustcVerbose = @(& rustc -vV)
if ($LASTEXITCODE -ne 0) {
    throw 'rustc -vV failed while recording the release target'
}
$hostLine = $rustcVerbose | Where-Object { $_ -match '^host:\s*(\S+)\s*$' } | Select-Object -First 1
if (-not $hostLine -or $hostLine -notmatch '^host:\s*(\S+)\s*$') {
    throw 'could not determine rustc host target'
}
$rustHost = $Matches[1]
$cargoVersion = (& cargo -V)
if ($LASTEXITCODE -ne 0 -or -not $cargoVersion) {
    throw 'cargo -V failed while recording the release toolchain'
}
$cargoVersion = $cargoVersion.Trim()
$rustFlags = [string]$env:RUSTFLAGS
$expectedRustTarget = $targetSpec.RustTarget
$expectedRustFlags = '-C target-feature=+crt-static'
$exeRecipeExact = Test-ExactArguments -Actual $ExeCargoArguments -Expected $expectedExeArguments
$dllRecipeExact = Test-ExactArguments -Actual $DllCargoArguments -Expected $expectedDllArguments

$inputPaths = @(Get-ReleaseRequiredInputPaths)
Assert-ReleaseRequiredInputsTracked -Root $root -RelativePaths $inputPaths
$inputs = @(
    foreach ($relative in $inputPaths) {
        $path = Get-ReleasePathUnderRoot -Root $root -RelativePath $relative
        Get-ReleaseFileRecord -Path $path -RelativeTo $root
    }
)

$stageFiles = @(Get-ReleaseStageInventory -StagePath $stage.FullName)
$magickFiles = @($stageFiles | Where-Object { $_.path.StartsWith('magick/', [StringComparison]::OrdinalIgnoreCase) })
$magickBytes = [int64]0
foreach ($file in $magickFiles) {
    $magickBytes += [int64]$file.bytes
}

$publishableReasons = [System.Collections.Generic.List[string]]::new()
if (-not $sourceTreeClean) { $publishableReasons.Add('source tree was dirty at build time') }
if (-not $RustBuildPerformed) { $publishableReasons.Add('Rust build was skipped') }
if (-not $exeRecipeExact) { $publishableReasons.Add('EXE Cargo build recipe was not production-exact') }
if (-not $dllRecipeExact) { $publishableReasons.Add('DLL Cargo build recipe was not production-exact') }
if ($rustHost -notmatch '^[A-Za-z0-9_-]+(?:-[A-Za-z0-9_-]+){2,}$') {
    $publishableReasons.Add("rustc host was '$rustHost'")
}
if ($rustFlags -cne $expectedRustFlags) { $publishableReasons.Add("RUSTFLAGS was '$rustFlags'") }
if ($Architecture -eq 'x64' -and (-not $ImageMagickBundled -or -not $magickPresent)) {
    $publishableReasons.Add('full ImageMagick payload is absent')
}
if (-not $ModernMenuBundled -or -not $modernPresent) { $publishableReasons.Add('modern-menu package is absent') }
$publishable = $publishableReasons.Count -eq 0

$manifest = [ordered]@{
    schemaVersion = 1
    product = 'SageThumbs 2K'
    version = $Version
    createdUtc = [DateTime]::UtcNow.ToString('o', [Globalization.CultureInfo]::InvariantCulture)
    commitSha = $gitHead.ToLowerInvariant()
    sourceTreeClean = $sourceTreeClean
    publishable = $publishable
    publishableReasons = @($publishableReasons)
    architecture = $Architecture
    build = [ordered]@{
        rustBuildPerformed = $RustBuildPerformed
        cargoLocked = $ExeCargoArguments -contains '--locked' -and $DllCargoArguments -contains '--locked'
        # This is the explicit Cargo target attested by the exact argument arrays;
        # it is intentionally allowed to differ from the machine rustc host.
        rustTarget = $expectedRustTarget
        rustHost = $rustHost
        rustFlags = $rustFlags
        toolchain = [ordered]@{
            cargo = $cargoVersion
            rustcVerbose = @($rustcVerbose)
        }
        cargo = [ordered]@{
            executables = [ordered]@{
                package = 'sagethumbs2k'
                features = @(Get-CargoFeatureList $ExeCargoArguments)
                arguments = @($ExeCargoArguments)
            }
            shellExtension = [ordered]@{
                package = 'sagethumbs2k-dll'
                features = @(Get-CargoFeatureList $DllCargoArguments)
                arguments = @($DllCargoArguments)
            }
        }
        imageMagickBundled = $ImageMagickBundled
        modernMenuBundled = $ModernMenuBundled
    }
    inputs = $inputs
    installer = Get-ReleaseFileRecord -Path $installer.FullName -RelativeTo $installer.DirectoryName
    stage = [ordered]@{
        rustPayloadBytes = $rustBytes
        imageMagickBytes = $magickBytes
        imageMagickFileCount = $magickFiles.Count
        files = $stageFiles
    }
}

$outputDirectory = Split-Path $OutputPath -Parent
if (-not $outputDirectory) {
    $outputDirectory = (Get-Location).Path
}
New-Item -ItemType Directory -Path $outputDirectory -Force | Out-Null
$temporary = "$OutputPath.tmp-$PID-$([guid]::NewGuid().ToString('N'))"
try {
    $manifest | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $temporary -Encoding utf8
    Move-Item -LiteralPath $temporary -Destination $OutputPath -Force
} finally {
    if (Test-Path -LiteralPath $temporary) {
        Remove-Item -LiteralPath $temporary -Force
    }
}

$color = if ($publishable) { 'Green' } else { 'Yellow' }
Write-Host "[manifest] $OutputPath" -ForegroundColor $color
if ($publishable) {
    Write-Host '[manifest] full release provenance recorded; artifact is publishable.' -ForegroundColor Green
} else {
    Write-Host "[manifest] development artifact only: $($publishableReasons -join '; ')" -ForegroundColor Yellow
}
