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

$expectedInstallerName = "SageThumbs2K-Setup-$Version.exe"
if ($installer.Name -cne $expectedInstallerName) {
    throw "installer filename mismatch: '$($installer.Name)' (expected '$expectedInstallerName')"
}
Assert-ReleasePeMetadata -Path $installer.FullName -Version $Version -Description 'SageThumbs 2K Setup'

$rustNames = @('sagethumbs2k.dll', 'SageThumbs2K.exe', 'st2k.exe')
$rustBytes = [int64]0
foreach ($name in $rustNames) {
    $path = Join-Path $stage.FullName $name
    Assert-ReleasePeMetadata -Path $path -Version $Version
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
        -Version $Version
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

$expectedExeArguments = @(
    '--release', '--locked',
    '-p', 'sagethumbs2k',
    # `hdr-capture` is APP-ONLY (see Cargo.toml): it links D3D11/DXGI, which the
    # shell DLL must never load. The production EXE recipe therefore carries it and
    # the DLL recipe below deliberately does not.
    '--features', 'webp-lossy,html-preview,hdr-capture'
)
$expectedDllArguments = @(
    '--release', '--locked',
    '-p', 'sagethumbs2k-dll',
    '--features', 'webp-lossy,dll-i18n-subset'
)
function Test-ExactArguments([string[]]$Actual, [string[]]$Expected) {
    if ($Actual.Count -ne $Expected.Count) { return $false }
    for ($i = 0; $i -lt $Expected.Count; $i++) {
        if ($Actual[$i] -cne $Expected[$i]) { return $false }
    }
    return $true
}

$rustcVerbose = @(& rustc -vV)
if ($LASTEXITCODE -ne 0) {
    throw 'rustc -vV failed while recording the release target'
}
$hostLine = $rustcVerbose | Where-Object { $_ -match '^host:\s*(\S+)\s*$' } | Select-Object -First 1
if (-not $hostLine -or $hostLine -notmatch '^host:\s*(\S+)\s*$') {
    throw 'could not determine rustc host target'
}
$rustTarget = $Matches[1]
$cargoVersion = (& cargo -V)
if ($LASTEXITCODE -ne 0 -or -not $cargoVersion) {
    throw 'cargo -V failed while recording the release toolchain'
}
$cargoVersion = $cargoVersion.Trim()
$rustFlags = [string]$env:RUSTFLAGS
$expectedRustTarget = 'x86_64-pc-windows-msvc'
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
if ($rustTarget -cne $expectedRustTarget) { $publishableReasons.Add("Rust target was $rustTarget") }
if ($rustFlags -cne $expectedRustFlags) { $publishableReasons.Add("RUSTFLAGS was '$rustFlags'") }
if (-not $ImageMagickBundled -or -not $magickPresent) { $publishableReasons.Add('full ImageMagick payload is absent') }
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
    build = [ordered]@{
        rustBuildPerformed = $RustBuildPerformed
        cargoLocked = $ExeCargoArguments -contains '--locked' -and $DllCargoArguments -contains '--locked'
        rustTarget = $rustTarget
        rustFlags = $rustFlags
        toolchain = [ordered]@{
            cargo = $cargoVersion
            rustcVerbose = @($rustcVerbose)
        }
        cargo = [ordered]@{
            executables = [ordered]@{
                package = 'sagethumbs2k'
                features = @('webp-lossy', 'html-preview')
                arguments = @($ExeCargoArguments)
            }
            shellExtension = [ordered]@{
                package = 'sagethumbs2k-dll'
                features = @('webp-lossy', 'dll-i18n-subset')
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
