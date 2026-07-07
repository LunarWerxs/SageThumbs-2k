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
    # Clear the modern-menu marker (see install path below).
    Remove-ItemProperty 'HKLM:\SOFTWARE\SageThumbs2K' -Name 'ModernMenuActive' -ErrorAction SilentlyContinue
    # Remove the Start-menu shortcut (current + legacy "Options" name) and the
    # obsolete screenshot shortcuts (the screenshot tool is controlled via Settings now).
    foreach ($f in @('SageThumbs 2K.lnk', 'SageThumbs 2K Options.lnk',
                     'SageThumbs 2K Screenshot.lnk', 'SageThumbs 2K Screenshot Hotkey.lnk')) {
        $l = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) $f
        if (Test-Path $l) { Remove-Item $l -Force -ErrorAction SilentlyContinue }
    }
    # Turn the screenshot hotkey off: remove its autostart entry + stop the daemon.
    Remove-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'SageThumbs2KScreenshot' -ErrorAction SilentlyContinue
    Get-Process SageThumbs2K -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    if (Test-Path $prog) { Remove-Item $prog -Recurse -Force -ErrorAction SilentlyContinue }
    Write-Host "SageThumbs 2K uninstalled."
    return
}

New-Item -ItemType Directory -Path $prog -Force | Out-Null
Copy-Item "$BuildDir\sagethumbs2k.dll" $prog -Force
# The bin target is `SageThumbs2K`, so it builds as `SageThumbs2K.exe` directly.
Copy-Item "$BuildDir\SageThumbs2K.exe" $prog -Force
# The CLI / MCP server (`st2k --mcp`). The dist installer ships it; the dev
# install used to omit it, leaving a live CLI check running stale code.
Copy-Item "$BuildDir\st2k.exe" $prog -Force
Copy-Item "$root\packaging\AppxManifest.xml" $prog -Force
Copy-Item "$root\packaging\Assets" $prog -Recurse -Force
# Our hardened ImageMagick policy.xml next to the binaries. decode.rs points
# MAGICK_CONFIGURE_PATH here, so the policy applies even when we fall back to a
# system-installed magick (this dev/compact install bundles none of its own).
Copy-Item "$root\packaging\imagemagick-policy.xml" "$prog\policy.xml" -Force

# Thumbnails + classic context menu (machine-wide, HKLM). This classic IContextMenu
# handler serves the full owner-drawn preview + "SageThumbs 2K" submenu with Settings
# in "Show more options" (and the whole right-click menu on classic-menu machines).
regsvr32 /s "$prog\sagethumbs2k.dll"
# Modern Win11 menu: register the sparse package (Dev Mode, UNSIGNED loose -Register —
# the signed installer.iss path uses the packed .msix instead) so the packaged QUICK
# verbs (Convert into / Convert… / Resize / Rotate) appear on the compact Win11 menu.
# Then set the HKLM marker the classic handler reads (settings::modern_menu_active): with
# the package active, Windows bridges those quick verbs into "Show more options", so the
# classic handler omits ITS own quick-verb copies to avoid double-listing them. The full
# flyout + preview stay on the classic handler. (See packaging\AppxManifest.xml.)
Get-AppxPackage $pkg | Remove-AppxPackage -ErrorAction SilentlyContinue
Add-AppxPackage -Register "$prog\AppxManifest.xml" -ExternalLocation $prog -ForceUpdateFromAnyVersion
# Set the modern-menu marker the classic handler reads (settings::modern_menu_active) via
# reg.exe and tolerate failure: on a machine where this key already exists with a locked-down
# ACL, `New-Item -Force` throws "Requested registry access is not allowed" (it reopens/replaces
# the key), which would abort the whole install. The value is idempotent, so a failed write on
# a key that's already correct is harmless.
try { & reg.exe add 'HKLM\SOFTWARE\SageThumbs2K' /v ModernMenuActive /t REG_DWORD /d 1 /f 2>$null | Out-Null } catch {}

# Flag this box as a DEVELOPER test machine (HKCU DevMachine=1 → settings::is_dev_machine).
# ONLY the owner ever runs this dev-install script (real users get the Inno installer), so a
# dev install is exactly the machine whose build/install/test churn must NOT inflate the public
# analytics: with this set, every startup check-in carries &dev=1 and the analytics Worker drops
# it from the public counters (tallying it under dims event='dev' for auditability). Without this
# the owner's own launches were being counted as real users — the reason 0.7.2 showed 17 "installs".
# Idempotent; stays set across a dev -Uninstall so a rebuild+reinstall never re-pollutes.
try { & reg.exe add 'HKCU\Software\SageThumbs2K' /v DevMachine /t REG_DWORD /d 1 /f 2>$null | Out-Null } catch {}

# Start Menu shortcut to the Options dialog.
$ws = New-Object -ComObject WScript.Shell
$sc = $ws.CreateShortcut($shortcut)
$sc.TargetPath = "$prog\SageThumbs2K.exe"
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
