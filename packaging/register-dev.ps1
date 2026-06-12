<#
  Phase-2 dev registration of the sparse identity package (NO signing, NO
  makeappx, NO admin — uses Developer Mode loose-manifest registration).

  Builds the DLL + stub EXE, generates placeholder logo assets if missing, then
  registers the package pointing at the build output as the external location.

      .\packaging\register-dev.ps1            # build + register
      .\packaging\register-dev.ps1 -Unregister

  After registering, restart File Explorer (the script does this) and right-click
  a .jpg/.png to look for "SageThumbs". Thumbnails for .tga/.dds/.qoi/etc. should
  also appear. A signed .msix for distribution is a separate (SDK) step.
#>
[CmdletBinding()]
param(
    [switch]$Unregister,
    [string]$ExternalLocation = (@("D:\st2k-target\release", "$PSScriptRoot\..\target\release") | Where-Object { Test-Path $_ } | Select-Object -First 1)
)

$ErrorActionPreference = 'Stop'
$pkgName = 'SageThumbs2K'
$root = Split-Path $PSScriptRoot -Parent
$manifest = Join-Path $PSScriptRoot 'AppxManifest.xml'
$assets = Join-Path $PSScriptRoot 'Assets'

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

# 2) Build the DLL + stub EXE into the external location.
Write-Host "Building release binaries..."
cargo build --release --manifest-path (Join-Path $root 'Cargo.toml')
$dll = Join-Path $ExternalLocation 'sagethumbs2k.dll'
$exe = Join-Path $ExternalLocation 'sagethumbs2k-app.exe'
if (-not (Test-Path $dll)) { throw "Missing $dll" }
if (-not (Test-Path $exe)) { throw "Missing $exe" }
Write-Host "External location: $ExternalLocation"

# 3) Register the loose manifest with the external location (Developer Mode).
Get-AppxPackage $pkgName | Remove-AppxPackage -ErrorAction SilentlyContinue
Add-AppxPackage -Register $manifest -ExternalLocation $ExternalLocation -ForceUpdateFromAnyVersion
Get-AppxPackage $pkgName | Format-List Name, PackageFullName, InstallLocation, Status

# 4) Restart Explorer so it loads the new package's shell extensions.
Stop-Process -Name explorer -Force -ErrorAction SilentlyContinue
Start-Process explorer.exe
Write-Host "Registered. Right-click a .jpg/.png for 'SageThumbs'; check .tga/.dds thumbnails."
