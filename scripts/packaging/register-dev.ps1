<#
  Phase-2 dev registration of the sparse identity package (NO signing, NO
  makeappx, NO admin — uses Developer Mode loose-manifest registration).

  Builds the DLL + stub EXE, generates placeholder logo assets if missing, then
  registers the package pointing at the build output as the external location.

      .\scripts\packaging\register-dev.ps1            # build + register
      .\scripts\packaging\register-dev.ps1 -Architecture arm64
      .\scripts\packaging\register-dev.ps1 -Unregister

  After registering, restart File Explorer (the script does this) and right-click
  a .jpg/.png to look for "SageThumbs". Thumbnails for .tga/.dds/.qoi/etc. should
  also appear. A signed .msix for distribution is a separate (SDK) step.
#>
[CmdletBinding()]
param(
    [switch]$Unregister,
    [ValidateSet('x64', 'arm64')]
    [string]$Architecture = 'x64',
    [string]$ExternalLocation,
    # Read-only guard for CI/local script tests: validates selected artifacts
    # and exits before Cargo, package registration, or Explorer changes.
    [switch]$ValidateOnly
)

$ErrorActionPreference = 'Stop'
$pkgName = 'SageThumbs2K'
$root = Split-Path $PSScriptRoot -Parent
$manifest = Join-Path $PSScriptRoot 'AppxManifest.xml'
$assets = Join-Path $PSScriptRoot 'Assets'

function Get-ArchitectureSpec([string]$SelectedArchitecture) {
    switch ($SelectedArchitecture) {
        'x64' {
            # Preserve `target\release` for the established x64 developer flow.
            return [pscustomobject]@{
                Name = 'x64'; RustTarget = 'x86_64-pc-windows-msvc';
                BuildSubdirectory = 'release'; PeMachine = [uint16]0x8664
            }
        }
        'arm64' {
            return [pscustomobject]@{
                Name = 'arm64'; RustTarget = 'aarch64-pc-windows-msvc';
                BuildSubdirectory = 'aarch64-pc-windows-msvc\release'; PeMachine = [uint16]0xaa64
            }
        }
        default { throw "Unsupported architecture: $SelectedArchitecture" }
    }
}

function Get-PeMachine([string]$Path) {
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        if ($stream.Length -lt 0x40) { throw "Not a PE file (too short): $Path" }
        $head = [byte[]]::new(0x40)
        if ($stream.Read($head, 0, $head.Length) -ne $head.Length -or $head[0] -ne 0x4d -or $head[1] -ne 0x5a) {
            throw "Not a PE file: $Path"
        }
        $peOffset = [BitConverter]::ToInt32($head, 0x3c)
        if ($peOffset -lt 0 -or $peOffset + 6 -gt $stream.Length) { throw "Invalid PE header offset: $Path" }
        $null = $stream.Seek($peOffset, [IO.SeekOrigin]::Begin)
        $coff = [byte[]]::new(6)
        if ($stream.Read($coff, 0, $coff.Length) -ne $coff.Length -or
            $coff[0] -ne 0x50 -or $coff[1] -ne 0x45 -or $coff[2] -ne 0 -or $coff[3] -ne 0) {
            throw "Invalid PE signature: $Path"
        }
        return [BitConverter]::ToUInt16($coff, 4)
    } finally {
        $stream.Dispose()
    }
}

function Assert-PeArchitecture([string]$Path, [pscustomobject]$Spec) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "Missing $Path" }
    $machine = Get-PeMachine $Path
    if ($machine -ne $Spec.PeMachine) {
        throw ('Architecture mismatch: expected {0} ({1}), but {2} is PE machine 0x{3:X4}' -f
            $Spec.Name, $Spec.RustTarget, $Path, $machine)
    }
}

function Assert-NativeArm64Host([pscustomobject]$Spec) {
    if ($Spec.Name -ne 'arm64') { return }
    if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne
        [System.Runtime.InteropServices.Architecture]::Arm64) {
        throw 'ARM64 registration requires a native ARM64 Windows host; use -ValidateOnly for cross-host artifact checks.'
    }
}

function New-RegistrationManifest([pscustomobject]$Spec, [string]$BuildLocation) {
    # Registration files live below the selected target's output directory. This
    # keeps x64 and ARM64 developer registrations and their architecture-specific
    # manifests from overwriting each other or the tracked template.
    $registrationRoot = Join-Path $BuildLocation ("dev-registration-" + $Spec.Name)
    New-Item -ItemType Directory -Path $registrationRoot -Force | Out-Null
    $registrationManifest = Join-Path $registrationRoot 'AppxManifest.xml'
    Copy-Item -LiteralPath $manifest -Destination $registrationManifest -Force
    Copy-Item -LiteralPath $assets -Destination (Join-Path $registrationRoot 'Assets') -Recurse -Force

    $manifestText = Get-Content -LiteralPath $registrationManifest -Raw
    $manifestArchitecture = if ($Spec.Name -eq 'arm64') { 'arm64' } else { 'neutral' }
    $manifestText = $manifestText -replace '(<Identity\b[^>]*\bProcessorArchitecture=")[^"]+("[^>]*>)', "`${1}$manifestArchitecture`${2}"
    if ($manifestText -notmatch [regex]::Escape("ProcessorArchitecture=`"$manifestArchitecture`"")) {
        throw "Could not set $($Spec.Name) package identity in $registrationManifest"
    }
    Set-Content -LiteralPath $registrationManifest -Value $manifestText -Encoding utf8
    return $registrationManifest
}

$spec = Get-ArchitectureSpec $Architecture
if (-not $ExternalLocation) {
    $ExternalLocation = Join-Path (& "$PSScriptRoot\..\scripts\_targetdir.ps1") $spec.BuildSubdirectory
}

if ($ValidateOnly) {
    foreach ($artifact in @('sagethumbs2k.dll', 'SageThumbs2K.exe', 'st2k.exe')) {
        Assert-PeArchitecture (Join-Path $ExternalLocation $artifact) $spec
    }
    [pscustomobject]@{ Architecture = $spec.Name; RustTarget = $spec.RustTarget; ExternalLocation = $ExternalLocation }
    return
}

# An ARM64 sparse package hosts an in-process ARM64 shell extension. Do not let
# an x64 developer machine remove/register it or restart Explorer; cross-host
# artifact validation remains available through the early -ValidateOnly path.
Assert-NativeArm64Host $spec

if ($Unregister) {
    Get-AppxPackage $pkgName | Remove-AppxPackage -ErrorAction SilentlyContinue
    Write-Host "Unregistered $pkgName."
    Stop-Process -Name explorer -Force -ErrorAction SilentlyContinue
    return
}

# 1) Placeholder logo assets (the manifest requires them to resolve).
if (-not (Test-Path $assets)) { New-Item -ItemType Directory -Path $assets | Out-Null }
Add-Type -AssemblyName System.Drawing
function New-Logo([string]$path, [int]$w, [int]$h) {
    if (Test-Path $path) { return }
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::FromArgb(40, 90, 170))
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
}
New-Logo (Join-Path $assets 'StoreLogo.png')        50  50
New-Logo (Join-Path $assets 'Square44x44Logo.png')  44  44
New-Logo (Join-Path $assets 'Square150x150Logo.png') 150 150

# 2) Build the DLL + stub EXE into the selected architecture's isolated output.
Write-Host "Building $($spec.Name) release binaries..."
$cargoArgs = @('build', '--release', '--locked', '--manifest-path', (Join-Path $root 'Cargo.toml'))
if ($spec.Name -eq 'arm64') { $cargoArgs += @('--target', $spec.RustTarget) }
& cargo @cargoArgs
if ($LASTEXITCODE) { throw "cargo build failed for $($spec.Name)" }
# The cargo bin target is `SageThumbs2K`, so it builds as `SageThumbs2K.exe` directly —
# the name the manifest's Executable= and the DLL's spawn code expect. No rename needed.
$dll = Join-Path $ExternalLocation 'sagethumbs2k.dll'
$exe = Join-Path $ExternalLocation 'SageThumbs2K.exe'
$cli = Join-Path $ExternalLocation 'st2k.exe'
foreach ($artifact in @($dll, $exe, $cli)) { Assert-PeArchitecture $artifact $spec }
Write-Host "External location ($($spec.RustTarget)): $ExternalLocation"
$registrationManifest = New-RegistrationManifest $spec $ExternalLocation

# 3) Register the loose manifest with the external location (Developer Mode).
Get-AppxPackage $pkgName | Remove-AppxPackage -ErrorAction SilentlyContinue
Add-AppxPackage -Register $registrationManifest -ExternalLocation $ExternalLocation -ForceUpdateFromAnyVersion
Get-AppxPackage $pkgName | Format-List Name, PackageFullName, InstallLocation, Status

# 4) Restart Explorer so it loads the new package's shell extensions.
Stop-Process -Name explorer -Force -ErrorAction SilentlyContinue
Start-Process explorer.exe
Write-Host "Registered. Right-click a .jpg/.png for 'SageThumbs'; check .tga/.dds thumbnails."
