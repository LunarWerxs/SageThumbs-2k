<#
  Permanent machine-wide install of SageThumbs 2K. Run from an ELEVATED prompt
  (writes to Program Files + HKLM):

      .\scripts\install.ps1
      .\scripts\install.ps1 -Uninstall

  Copies the DLL, stub EXE, manifest and assets into Program Files (self-contained,
  no dependency on the build tree), then:
    - regsvr32 registers the thumbnail provider + classic context-menu handler (HKLM)
    - Add-AppxPackage registers the sparse package for the modern Win11 context menu
#>
[CmdletBinding()]
param(
    [switch]$Uninstall,
    [string]$BuildDir = (@('D:\st2k-target\release', "$PSScriptRoot\..\target\release") | Where-Object { Test-Path $_ } | Select-Object -First 1)
)
$ErrorActionPreference = 'Stop'
$prog = Join-Path $env:ProgramFiles 'SageThumbs2K'
$root = Split-Path $PSScriptRoot -Parent
$pkg = 'SageThumbs2K'

$shortcut = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'SageThumbs 2K Options.lnk'

if ($Uninstall) {
    if (Test-Path "$prog\sagethumbs2k.dll") { regsvr32 /s /u "$prog\sagethumbs2k.dll" }
    Get-AppxPackage $pkg | Remove-AppxPackage -ErrorAction SilentlyContinue
    if (Test-Path $shortcut) { Remove-Item $shortcut -Force -ErrorAction SilentlyContinue }
    if (Test-Path $prog) { Remove-Item $prog -Recurse -Force -ErrorAction SilentlyContinue }
    Write-Host "SageThumbs 2K uninstalled."
    return
}

New-Item -ItemType Directory -Path $prog -Force | Out-Null
Copy-Item "$BuildDir\sagethumbs2k.dll" $prog -Force
Copy-Item "$BuildDir\sagethumbs2k-app.exe" $prog -Force
# The CLI / MCP server (`st2k --mcp`). The dist installer ships it; the dev
# install used to omit it, leaving a live CLI check running stale code.
Copy-Item "$BuildDir\st2k.exe" $prog -Force
Copy-Item "$root\packaging\AppxManifest.xml" $prog -Force
Copy-Item "$root\packaging\Assets" $prog -Recurse -Force

# Thumbnails + classic context menu (machine-wide, HKLM)
regsvr32 /s "$prog\sagethumbs2k.dll"
# Modern Win11 context menu (sparse package, self-contained in the install dir)
Get-AppxPackage $pkg | Remove-AppxPackage -ErrorAction SilentlyContinue
Add-AppxPackage -Register "$prog\AppxManifest.xml" -ExternalLocation $prog -ForceUpdateFromAnyVersion

# Start Menu shortcut to the Options dialog.
$ws = New-Object -ComObject WScript.Shell
$sc = $ws.CreateShortcut($shortcut)
$sc.TargetPath = "$prog\sagethumbs2k-app.exe"
$sc.WorkingDirectory = $prog
$sc.Description = 'SageThumbs 2K Options'
$sc.Save()

Write-Host "Installed to $prog. Restart Explorer (or reboot) and clear the thumbnail cache to see changes."
Write-Host "Configure via Start menu > 'SageThumbs 2K Options'."
