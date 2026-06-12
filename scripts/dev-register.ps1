<#
  Phase-1 dev loop for the SageThumbs 2K thumbnail provider.

  Registers the freshly built DLL with regsvr32, clears the thumbnail cache,
  and restarts Explorer so the new build is exercised.

  Run from an ELEVATED PowerShell (regsvr32 writes HKCR/HKLM):
      .\scripts\dev-register.ps1            # register + refresh
      .\scripts\dev-register.ps1 -Unregister
#>
[CmdletBinding()]
param(
    [switch]$Unregister,
    [switch]$Debug,   # set HKCU\Software\SageThumbs2K\Debug=1 for verbose logging
    # Artifacts live in a space-free target dir (see .cargo/config.toml).
    [string]$Dll = (@("$PSScriptRoot\..\target\debug\sagethumbs2k.dll", "D:\st2k-target\debug\sagethumbs2k.dll") | Where-Object { Test-Path $_ } | Select-Object -First 1)
)

$ErrorActionPreference = 'Stop'
$Dll = (Resolve-Path $Dll).Path
Write-Host "DLL: $Dll"

if ($Debug) {
    New-Item -Path 'HKCU:\Software\SageThumbs2K' -Force | Out-Null
    New-ItemProperty -Path 'HKCU:\Software\SageThumbs2K' -Name 'Debug' -Value 1 -PropertyType DWord -Force | Out-Null
    Write-Host "Verbose logging ON -> $env:LOCALAPPDATA\SageThumbs2K.log"
}

# Explorer + the COM surrogate file-lock the loaded DLL; release them first.
Write-Host "Stopping explorer.exe and COM surrogate (dllhost.exe)..."
taskkill /f /im explorer.exe 2>$null | Out-Null
taskkill /f /im dllhost.exe  2>$null | Out-Null
Start-Sleep -Milliseconds 500

if ($Unregister) {
    Write-Host "Unregistering..."
    regsvr32 /s /u $Dll
} else {
    Write-Host "Registering..."
    regsvr32 /s $Dll
}

# Clear the per-user thumbnail cache so providers are re-invoked.
Write-Host "Clearing thumbnail cache..."
$cache = "$env:LOCALAPPDATA\Microsoft\Windows\Explorer"
Get-ChildItem "$cache\thumbcache_*.db" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue

Write-Host "Restarting explorer.exe..."
Start-Process explorer.exe

Write-Host "Done. Open a folder of images to test. Log: $env:LOCALAPPDATA\SageThumbs2K.log"
