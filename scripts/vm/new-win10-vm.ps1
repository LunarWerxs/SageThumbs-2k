<#
  new-win10-vm.ps1 — create a real Windows 10 Hyper-V VM for testing (ELEVATED).

  Windows Sandbox mirrors the HOST OS (Win11 here), so it CANNOT reproduce Windows
  10-specific bugs. This makes an actual Win10 guest — the environment issue #5's
  reporter is on (Win10 Home 22H2, build 19045).

      # elevated PowerShell:
      .\scripts\vm\new-win10-vm.ps1 -Iso D:\isos\Win10_22H2_English_x64.iso
      .\scripts\vm\new-win10-vm.ps1 -Iso <iso> -Name st2k-win10 -MemoryGB 6 -DiskGB 64

  GET A WIN10 ISO (pick one):
   * Media Creation Tool -> "Create installation media" -> ISO
     https://www.microsoft.com/software-download/windows10   (matches a real user's box)
   * Win10 Enterprise 90-day EVAL ISO (no key needed, expires in 90 days)
     https://www.microsoft.com/evalcenter/evaluate-windows-10-enterprise

  After it boots: install Win10 normally, then inside the guest map/copy the built
  installer in (or use an internal network share) and run the same clean-room test
  as the Sandbox. The VM is a Gen2 (UEFI) VM on the Hyper-V "Default Switch" (NAT,
  so the guest has internet without touching your LAN config).
#>
param(
    [Parameter(Mandatory=$true)][string]$Iso,
    [string]$Name = 'st2k-win10',
    [int]$MemoryGB = 6,
    [int]$DiskGB = 64,
    [string]$VmRoot = 'D:\Hyper-V'
)

$ErrorActionPreference = 'Stop'
if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
        ).IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)) {
    Write-Host "Run this from an ELEVATED PowerShell (Hyper-V management needs admin)." -ForegroundColor Red
    exit 1
}
if (-not (Get-Command New-VM -EA SilentlyContinue)) {
    Write-Host "Hyper-V PowerShell module not found. Enable Hyper-V (elevated, then reboot):" -ForegroundColor Red
    Write-Host "  Enable-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V-All -All"
    exit 1
}
if (-not (Test-Path $Iso)) { Write-Host "ISO not found: $Iso" -ForegroundColor Red; exit 1 }
if (Get-VM -Name $Name -EA SilentlyContinue) {
    Write-Host "A VM named '$Name' already exists. Remove it first or pass -Name." -ForegroundColor Red
    exit 1
}

New-Item -ItemType Directory -Force $VmRoot | Out-Null
$vhd = Join-Path $VmRoot "$Name.vhdx"

# The Default Switch gives the guest NAT internet with zero switch setup.
$switch = (Get-VMSwitch -EA SilentlyContinue | Where-Object Name -eq 'Default Switch')
if (-not $switch) { Write-Host "No 'Default Switch' found; the VM will start network-less." -ForegroundColor Yellow }

Write-Host "Creating Gen2 VM '$Name' ($MemoryGB GB RAM, $DiskGB GB disk)..." -ForegroundColor Cyan
New-VM -Name $Name -Generation 2 -MemoryStartupBytes ($MemoryGB * 1GB) `
    -NewVHDPath $vhd -NewVHDSizeBytes ($DiskGB * 1GB) `
    -SwitchName $(if ($switch) { 'Default Switch' } else { $null }) | Out-Null

Set-VM -Name $Name -DynamicMemory -MemoryMinimumBytes 2GB -MemoryMaximumBytes ($MemoryGB * 1GB) `
    -AutomaticCheckpointsEnabled $false
Set-VMProcessor -VMName $Name -Count ([math]::Min(4, (Get-CimInstance Win32_ComputerSystem).NumberOfLogicalProcessors))
Add-VMDvdDrive -VMName $Name -Path $Iso
# Boot from the DVD the first time.
$dvd = Get-VMDvdDrive -VMName $Name
Set-VMFirmware -VMName $Name -FirstBootDevice $dvd
# Win10 needs Secure Boot with the MS UEFI CA template.
Set-VMFirmware -VMName $Name -EnableSecureBoot On -SecureBootTemplate 'MicrosoftWindows'

Write-Host "Created. Starting + opening the console..." -ForegroundColor Green
Start-VM -Name $Name
vmconnect.exe localhost $Name
Write-Host @"

Next:
  * Install Windows 10 in the console window (any edition; skip the product key).
  * To get the built installer INTO the guest: copy dist\SageThumbs2K-Setup-*.exe onto
    a checkpoint-safe path, or use an internal share. Then run the same test as the
    Sandbox (install -> right-click a sample -> 'st2k.exe doctor').
  * Snapshot a clean post-install state:  Checkpoint-VM -Name $Name -SnapshotName clean
  * Tear down when done:                  Stop-VM $Name -Force; Remove-VM $Name -Force; Remove-Item '$vhd'
"@
