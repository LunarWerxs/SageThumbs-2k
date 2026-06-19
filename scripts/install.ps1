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
    [string]$BuildDir = (Join-Path (& "$PSScriptRoot\_targetdir.ps1") 'release')
)
$ErrorActionPreference = 'Stop'
$prog = Join-Path $env:ProgramFiles 'SageThumbs2K'
$root = Split-Path $PSScriptRoot -Parent
$pkg = 'SageThumbs2K'

$shortcut = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'SageThumbs 2K.lnk'

if ($Uninstall) {
    if (Test-Path "$prog\sagethumbs2k.dll") { regsvr32 /s /u "$prog\sagethumbs2k.dll" }
    Get-AppxPackage $pkg | Remove-AppxPackage -ErrorAction SilentlyContinue
    # Remove the Start-menu shortcut (current + legacy "Options" name) and the
    # obsolete screenshot shortcuts (the screenshot tool is controlled via Settings now).
    foreach ($f in @('SageThumbs 2K.lnk', 'SageThumbs 2K Options.lnk',
                     'SageThumbs 2K Screenshot.lnk', 'SageThumbs 2K Screenshot Hotkey.lnk')) {
        $l = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) $f
        if (Test-Path $l) { Remove-Item $l -Force -ErrorAction SilentlyContinue }
    }
    # Turn the screenshot hotkey off: remove its autostart entry + stop the daemon.
    Remove-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'SageThumbs2KScreenshot' -ErrorAction SilentlyContinue
    Get-Process sagethumbs2k-app -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
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
# Our hardened ImageMagick policy.xml next to the binaries. decode.rs points
# MAGICK_CONFIGURE_PATH here, so the policy applies even when we fall back to a
# system-installed magick (this dev/compact install bundles none of its own).
Copy-Item "$root\packaging\imagemagick-policy.xml" "$prog\policy.xml" -Force

# Thumbnails + classic context menu (machine-wide, HKLM). This single classic
# IContextMenu handler now serves the WHOLE menu (owner-drawn preview + quick
# verbs + full "SageThumbs 2K" submenu with Settings) in "Show more options".
regsvr32 /s "$prog\sagethumbs2k.dll"
# Classic-menu-only: we no longer register the sparse package. A packaged
# `windows.fileExplorerContextMenus` verb gets bridged into "Show more options"
# and would double-list "SageThumbs 2K" next to our classic handler (the "off on
# its own" duplicate). Remove any package a prior install/signed-installer left
# behind so only the classic handler remains. (See packaging\AppxManifest.xml.)
Get-AppxPackage $pkg | Remove-AppxPackage -ErrorAction SilentlyContinue

# Start Menu shortcut to the Options dialog.
$ws = New-Object -ComObject WScript.Shell
$sc = $ws.CreateShortcut($shortcut)
$sc.TargetPath = "$prog\sagethumbs2k-app.exe"
$sc.WorkingDirectory = $prog
$sc.Description = 'SageThumbs 2K'
$sc.Save()

# No screenshot Start-menu shortcuts: the capture tool + Ctrl+PrtScn hotkey are
# controlled from the Settings dialog now, so they don't clutter the Start menu.
# (Clear any left over from older installs.)
foreach ($f in @('SageThumbs 2K Options.lnk', 'SageThumbs 2K Screenshot.lnk', 'SageThumbs 2K Screenshot Hotkey.lnk')) {
    $l = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) $f
    if (Test-Path $l) { Remove-Item $l -Force -ErrorAction SilentlyContinue }
}

Write-Host "Installed to $prog. Restart Explorer (or reboot) and clear the thumbnail cache to see changes."
Write-Host "Configure via Start menu > 'SageThumbs 2K' (enable screenshots in Settings)."
