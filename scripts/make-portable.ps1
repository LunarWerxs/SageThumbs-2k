<#
  make-portable.ps1 - build the NO-INSTALLER distribution: a plain .zip of the same binaries,
  plus Register/Unregister scripts.

  WHY THIS EXISTS (see docs/RELEASE-SECURITY.md for the full evidence):
  Antivirus engines flag the Inno Setup INSTALLER, never the software. Measured 2026-07-18:
  SageThumbs2K.exe 0/68, sagethumbs2k.dll 0/69, st2k.exe 0/69 - every shipped binary clean -
  while the installer that wraps them draws 2-3 heuristic hits. The detections target the
  packed self-extracting stub, which every EXE-installer format (Inno, NSIS, 7-Zip SFX,
  Squirrel) has by construction. A plain zip has no stub at all, so there is nothing for that
  detector class to fire on, and Windows' own Explorer does the extracting.

  This is an ADDITION, not a replacement: the installer remains the recommended path (it also
  installs the MSIX sparse package for the Windows 11 context menu, which a zip cannot do
  without the user trusting a certificate). The zip is the escape hatch for anyone whose AV
  blocks the installer, and for anyone who simply prefers no installer.

  Run after build-release.ps1 has staged files (it calls this automatically).
#>
[CmdletBinding()]
param(
    [string]$Stage = (Join-Path (Split-Path $PSScriptRoot -Parent) 'packaging\stage'),
    [string]$OutDir = (Join-Path (Split-Path $PSScriptRoot -Parent) 'dist'),
    [string]$Version
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
if (-not $Version) {
    $Version = ([regex]::Match((Get-Content "$root\Cargo.toml" -Raw), '(?m)^\s*version\s*=\s*"([^"]+)"')).Groups[1].Value
}
if (-not (Test-Path $Stage)) { throw "stage dir not found: $Stage (run build-release.ps1 first)" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$work = Join-Path ([IO.Path]::GetTempPath()) "st2k-portable-$PID"
if (Test-Path $work) { Remove-Item $work -Recurse -Force }
$pkg = Join-Path $work "SageThumbs2K-$Version"
New-Item -ItemType Directory -Force -Path $pkg | Out-Null

# Copy the payload, MINUS the installer-only bits. The .msix/.cer are deliberately excluded:
# registering a sparse package requires trusting its certificate, which is exactly the kind of
# step a portable user should not be asked to take. Portable therefore gets thumbnails + the
# CLASSIC right-click menu; the modern Win11 menu stays an installer feature.
Copy-Item "$Stage\*" $pkg -Recurse -Exclude '*.msix', '*.cer'

# ---- Register.ps1 (ships inside the zip) ------------------------------------------------
@'
<#
  Register SageThumbs 2K from this folder. Run as ADMINISTRATOR.

      Right-click Register.ps1 -> "Run with PowerShell"  (accept the elevation prompt)
   or from an elevated terminal:  powershell -ExecutionPolicy Bypass -File .\Register.ps1

  This registers the shell extension IN PLACE, so KEEP THIS FOLDER WHERE IT IS. Moving or
  deleting it after registering will leave Explorer pointing at a missing DLL - run
  Unregister.ps1 FIRST if you want to move or remove it.
#>
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$dll  = Join-Path $here 'sagethumbs2k.dll'
if (-not (Test-Path $dll)) { throw "sagethumbs2k.dll not found next to this script." }

$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
         ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) {
    Write-Host "Elevation required - relaunching as administrator..." -ForegroundColor Yellow
    Start-Process powershell -Verb RunAs -ArgumentList '-ExecutionPolicy','Bypass','-File',"`"$($MyInvocation.MyCommand.Path)`""
    return
}

Write-Host "Registering $dll ..." -ForegroundColor Cyan
regsvr32 /s $dll
if ($LASTEXITCODE) { throw "regsvr32 failed ($LASTEXITCODE)" }

# Explorer caches thumbnails aggressively; clear so new formats appear immediately.
Write-Host "Clearing the thumbnail cache and restarting Explorer..." -ForegroundColor Cyan
Get-Process explorer -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 800
Remove-Item "$env:LOCALAPPDATA\Microsoft\Windows\Explorer\thumbcache_*.db" -Force -ErrorAction SilentlyContinue
Start-Process explorer

Write-Host ""
Write-Host "Done. SageThumbs 2K is active for this folder location." -ForegroundColor Green
Write-Host "  * Thumbnails and the classic right-click menu are registered."
Write-Host "  * The Windows 11 'modern' context menu needs the installer (it requires a"
Write-Host "    signed sparse package), so it is not available in the portable build."
Write-Host "  * Settings:  .\SageThumbs2K.exe"
Write-Host "  * CLI:       .\st2k.exe --help"
Write-Host ""
Write-Host "To remove: run Unregister.ps1 (as administrator) BEFORE deleting this folder." -ForegroundColor Yellow
'@ | Set-Content (Join-Path $pkg 'Register.ps1') -Encoding UTF8

# ---- Unregister.ps1 ----------------------------------------------------------------------
@'
<#
  Unregister SageThumbs 2K. Run as ADMINISTRATOR, BEFORE deleting this folder.
#>
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$dll  = Join-Path $here 'sagethumbs2k.dll'

$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
         ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) {
    Start-Process powershell -Verb RunAs -ArgumentList '-ExecutionPolicy','Bypass','-File',"`"$($MyInvocation.MyCommand.Path)`""
    return
}

# Stop anything running from this folder first, or the DLL/EXE stays locked.
Get-Process SageThumbs2K -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -and $_.Path.StartsWith($here, 'OrdinalIgnoreCase') } |
    Stop-Process -Force -ErrorAction SilentlyContinue

if (Test-Path $dll) { regsvr32 /s /u $dll }
Get-Process explorer -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 800
Remove-Item "$env:LOCALAPPDATA\Microsoft\Windows\Explorer\thumbcache_*.db" -Force -ErrorAction SilentlyContinue
Start-Process explorer
Write-Host "Unregistered. This folder can now be deleted." -ForegroundColor Green
'@ | Set-Content (Join-Path $pkg 'Unregister.ps1') -Encoding UTF8

# ---- README for the zip ------------------------------------------------------------------
@"
SageThumbs 2K $Version - portable (no installer)
================================================

Why this exists
---------------
Antivirus engines sometimes flag the Inno Setup INSTALLER. They do not flag the program:
every binary in this zip scans clean on VirusTotal. The detections target the compressed
self-extracting installer stub, which is a property of installer formats in general, not of
this software. This zip has no installer, so there is nothing for those heuristics to hit.

Setup
-----
1. Extract this folder somewhere permanent - for example C:\Tools\SageThumbs2K.
   The shell extension is registered IN PLACE, so the folder must stay put.
2. Right-click Register.ps1 and choose "Run with PowerShell", then accept the
   administrator prompt. (Administrator is required: shell extensions register
   machine-wide, and Explorer must be restarted to pick them up.)
3. Explorer restarts automatically. Thumbnails appear straight away.

What you get
------------
  * Explorer thumbnails for every supported format
  * The classic right-click "SageThumbs 2K" menu
  * SageThumbs2K.exe - Settings, Quick preview, Convert
  * st2k.exe        - command line / AI tool  (st2k --help)

What you do NOT get
-------------------
The Windows 11 "modern" right-click menu. That requires a signed sparse package, which
cannot be registered from a plain zip without asking you to trust a certificate. Use the
regular installer if you want it. Everything else is identical.

Removing it
-----------
Run Unregister.ps1 as administrator BEFORE deleting the folder. Deleting the folder while
still registered leaves Explorer pointing at a DLL that no longer exists.

Docs, source and issues: https://github.com/LunarWerxs/SageThumbs-2k
"@ | Set-Content (Join-Path $pkg 'README-PORTABLE.txt') -Encoding UTF8

$zip = Join-Path $OutDir "SageThumbs2K-$Version-portable.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path $pkg -DestinationPath $zip -CompressionLevel Optimal
Remove-Item $work -Recurse -Force

$mb = [math]::Round((Get-Item $zip).Length / 1MB, 2)
Write-Host "  -> $zip  ($mb MB)" -ForegroundColor Green
