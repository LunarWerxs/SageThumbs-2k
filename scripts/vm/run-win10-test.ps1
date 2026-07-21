<#
  run-win10-test.ps1 — build a REAL Windows 10 VM, unattended-install it, and run the
  clean-room install + decode test on it. ELEVATED (Hyper-V + PowerShell Direct).

      .\scripts\vm\run-win10-test.ps1 -Iso D:\isos\Win10_22H2_x64.iso

  Why this and not Windows Sandbox: the Sandbox ALWAYS mirrors the host OS (Win11 here),
  so it can't reproduce Windows 10. issue #5's reporter is on Win10 Home 22H2 -> this is it.

  Method: OFFLINE IMAGE APPLY (deterministic, no WinPE/Setup UI, no "press any key" boot
  prompt). Partition the VHDX (GPT: EFI + MSR + Windows), DISM-apply install.wim index 1
  ("Windows 10 Home"), bcdboot the EFI boot files, drop autounattend-win10.xml at
  Windows\Panther\unattend.xml (a location Windows ALWAYS reads, unlike a fixed data disk
  that Setup may skip). First boot runs specialize+oobe unattended -> local admin `vmadmin`
  auto-logs on -> the test is driven over PowerShell Direct (no guest networking needed):
  copy the installer + samples in, silent-install, doctor, thumbnail a .xcf + .zip, copy
  results back. Nothing here touches the host.
#>
param(
    [string]$Iso = 'D:\isos\Win10_22H2_x64.iso',
    [string]$Name = 'st2k-win10',
    [string]$VmRoot = 'D:\Hyper-V',
    [int]$MemoryGB = 6,
    [int]$DiskGB = 64,
    [string]$ResultDir = 'D:\isos\win10-test-results',
    [int]$BootTimeoutMin = 25,
    [switch]$Keep,
    [switch]$Resume
)
$ErrorActionPreference = 'Stop'
function Say($m) { Write-Host "[win10] $((Get-Date).ToString('HH:mm:ss')) $m" -ForegroundColor Cyan }

if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)) {
    Write-Host "Run elevated." -ForegroundColor Red; exit 1
}
if (-not (Test-Path $Iso)) { Write-Host "ISO not found: $Iso" -ForegroundColor Red; exit 1 }

$repo    = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
$corpus  = Join-Path (Split-Path $repo -Parent) 'test-corpus'
$installer = Get-ChildItem (Join-Path $repo 'dist') -Filter 'SageThumbs2K-Setup-*.exe' |
             Sort-Object LastWriteTime -Descending | Select-Object -First 1 -Exp FullName
$unattend = Join-Path $PSScriptRoot 'autounattend-win10.xml'
foreach ($p in @($installer, $unattend)) { if (-not (Test-Path $p)) { Write-Host "missing: $p" -ForegroundColor Red; exit 1 } }
New-Item -ItemType Directory -Force $VmRoot, $ResultDir | Out-Null

# ---- tear down any prior VM (the VHDX is kept when resuming) ----------------
if (Get-VM -Name $Name -EA SilentlyContinue) { Say "removing old VM"; Stop-VM $Name -TurnOff -Force -EA SilentlyContinue; Remove-VM $Name -Force -EA SilentlyContinue }
$osVhd = Join-Path $VmRoot "$Name.vhdx"
if (-not $Resume) { Remove-Item $osVhd -Force -EA SilentlyContinue }

# Assign a drive letter to a partition and PROVE it landed on THAT partition. Every branch
# below is a failure that actually happened while building this:
#   * DON'T use `diskpart assign letter=` for the ESP. diskpart flatly refuses a HIDDEN
#     system partition with the misleading "The specified drive letter is not free to be
#     assigned" -- for EVERY letter, so it reads like a host-wide drive-letter outage when
#     it is really just diskpart declining that one partition. `Add-PartitionAccessPath
#     -AssignDriveLetter` assigns it without complaint.
#   * A letter can already point at something ELSE, so `Test-Path X:\` returns TRUE while
#     you bcdboot into the wrong volume (that produced a bogus exit 193).
#   * A re-mounted VHDX gets its OLD letters back from MountedDevices, so check first.
# The partition's own AccessPaths is the only authoritative answer.
function Set-VerifiedLetter {
    param([int]$Disk, [int]$Part, [string[]]$Candidates)
    $cur = "$((Get-Partition -DiskNumber $Disk -PartitionNumber $Part -EA SilentlyContinue).DriveLetter)".Trim()
    if ($cur -and (Test-Path "${cur}:\")) { return $cur }
    foreach ($l in $Candidates) {
        try { Add-PartitionAccessPath -DiskNumber $Disk -PartitionNumber $Part -AccessPath "${l}:\" -EA Stop } catch { continue }
        Start-Sleep 1
        if (((Get-Partition -DiskNumber $Disk -PartitionNumber $Part -EA SilentlyContinue).AccessPaths -contains "${l}:\") -and (Test-Path "${l}:\")) { return $l }
    }
    try {
        Add-PartitionAccessPath -DiskNumber $Disk -PartitionNumber $Part -AssignDriveLetter -EA Stop
        Start-Sleep 2
        $l = "$((Get-Partition -DiskNumber $Disk -PartitionNumber $Part).DriveLetter)".Trim()
        if ($l -and (Test-Path "${l}:\")) { return $l }
    } catch { }
    return $null
}

# Seed the candidate list from letters that LOOK free. An elevated session does not see the
# interactive user's mapped network drives (this box maps O:/P:/R:), so `net use` is folded
# in too -- but Set-VerifiedLetter is what actually decides.
$used = @()
$used += (Get-Volume -EA SilentlyContinue | Where-Object DriveLetter | ForEach-Object { "$($_.DriveLetter)".ToUpper() })
$used += (Get-PSDrive -PSProvider FileSystem -EA SilentlyContinue | ForEach-Object { $_.Name.ToUpper() })
$used += ((net use) 2>&1 | Select-String -Pattern '\s([A-Z]):\s' -AllMatches |
          ForEach-Object { $_.Matches } | ForEach-Object { $_.Groups[1].Value.ToUpper() })
$used = $used | Select-Object -Unique
$cands = @('W','V','U','T','Q','N','M','L','K','J','G','Y','X','Z') | Where-Object { $_ -notin $used }
if ($cands.Count -lt 2) { Write-Host "no free drive letters available" -ForegroundColor Red; exit 1 }
Say "candidate drive letters: $($cands -join '') (excluded in-use: $($used -join ','))"
$isoMounted = $false

if ($Resume) {
    # -Resume reuses a VHDX that ALREADY has Windows applied and only redoes the cheap tail
    # (bcdboot + unattend + boot + test). The apply is the 10-15 min part; when it succeeded
    # and only bcdboot failed, re-running it from scratch is pure wasted wall-clock.
    if (-not (Test-Path $osVhd)) { Write-Host "[win10] -Resume needs an existing $osVhd" -ForegroundColor Red; exit 1 }
    Say "RESUME: reusing the already-applied $osVhd (skipping the DISM apply)"
    Dismount-VHD -Path $osVhd -EA SilentlyContinue
    $diskNum = (Mount-VHD -Path $osVhd -Passthru | Get-Disk).Number
    # Re-format the ESP: it holds nothing we need (bcdboot repopulates it) and this rules out
    # a half-initialised/RAW system partition left by an earlier aborted run.
    $espN = (Get-Partition -DiskNumber $diskNum | Where-Object { $_.GptType -eq '{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}' } | Select-Object -First 1).PartitionNumber
    if ($espN) { "select disk $diskNum`r`nselect partition $espN`r`nformat fs=fat32 quick label=System`r`nexit`r`n" |
                 Set-Content (Join-Path $env:TEMP "st2k-fmt-$PID.txt") -Encoding ascii
                 diskpart /s (Join-Path $env:TEMP "st2k-fmt-$PID.txt") | Out-Null
                 Remove-Item (Join-Path $env:TEMP "st2k-fmt-$PID.txt") -Force -EA SilentlyContinue }
} else {
    Say "creating $DiskGB GB VHDX + partitioning (GPT: EFI/MSR/Windows)"
    New-VHD -Path $osVhd -SizeBytes ($DiskGB * 1GB) -Dynamic | Out-Null
    $diskNum = (Mount-VHD -Path $osVhd -Passthru | Get-Disk).Number
    $dp = "select disk $diskNum`r`nclean`r`nconvert gpt`r`ncreate partition efi size=300`r`nformat fs=fat32 quick label=System`r`ncreate partition msr size=16`r`ncreate partition primary`r`nformat fs=ntfs quick label=Windows`r`nexit`r`n"
    $dpFile = Join-Path $env:TEMP "st2k-dp-$PID.txt"; $dp | Set-Content $dpFile -Encoding ascii
    diskpart /s $dpFile | Out-Null
    Remove-Item $dpFile -Force -EA SilentlyContinue
    Start-Sleep 3
}

# Locate the two partitions by GPT type (NOT by number: `convert gpt` auto-creates an extra
# MSR, so the layout is not the order the create statements imply).
$parts   = Get-Partition -DiskNumber $diskNum
$espPart = $parts | Where-Object { $_.GptType -eq '{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}' } | Select-Object -First 1
$winPart = $parts | Where-Object { $_.GptType -eq '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}' } |
           Sort-Object Size -Descending | Select-Object -First 1
if (-not $espPart -or -not $winPart) {
    Write-Host "[win10] could not find the ESP + Windows partitions on disk $diskNum" -ForegroundColor Red
    Dismount-VHD -Path $osVhd -EA SilentlyContinue; exit 1
}

$espL = Set-VerifiedLetter -Disk $diskNum -Part $espPart.PartitionNumber -Candidates $cands
$winL = if ($espL) { Set-VerifiedLetter -Disk $diskNum -Part $winPart.PartitionNumber -Candidates ($cands | Where-Object { $_ -ne $espL }) }
if (-not $espL -or -not $winL) {
    Write-Host "[win10] could not assign verified drive letters (ESP=$espL WIN=$winL)." -ForegroundColor Red
    Write-Host "        Check: Get-Disk for IsOffline/IsReadOnly, and 'mountvol' for a stale mapping." -ForegroundColor Yellow
    Dismount-VHD -Path $osVhd -EA SilentlyContinue
    exit 1
}
Say "drive letters VERIFIED: ESP=${espL}: ($([int]($espPart.Size/1MB)) MB) WIN=${winL}: ($([int]($winPart.Size/1GB)) GB)"

if (-not $Resume) {
    Say "mounting ISO + applying 'Windows 10 Home' (index 1) with DISM (the slow part, ~10-15 min)"
    $mount = Mount-DiskImage -ImagePath $Iso -PassThru
    $isoMounted = $true
    $isoLtr = ($mount | Get-Volume).DriveLetter
    $wim = "${isoLtr}:\sources\install.wim"
    Expand-WindowsImage -ImagePath $wim -Index 1 -ApplyPath "${winL}:\" | Out-Null
}
if (-not (Test-Path "${winL}:\Windows\System32\ntoskrnl.exe")) {
    Write-Host "[win10] no Windows install on the target partition (expected ${winL}:\Windows\System32\ntoskrnl.exe)" -ForegroundColor Red
    if ($isoMounted) { Dismount-DiskImage -ImagePath $Iso | Out-Null }
    Dismount-VHD -Path $osVhd -EA SilentlyContinue; exit 1
}

Say "writing UEFI boot files (bcdboot) + injecting the unattend answer file"
# Use the TARGET IMAGE's OWN bcdboot.exe, never the host's. On a Win11 24H2+ host with Secure
# Boot on and the 2023 PCA in the Secure Boot DB, the host's bfsvc decides it must service the
# "Ex" (2023-signed) boot binaries and looks for <win>\Boot\EFI_EX\bootmgfw_EX.efi -- a file
# Windows 10 never shipped. It then dies with:
#   BFSVC Error: Failed to validate boot manager checksum (...bootmgfw_EX.efi)! Error = 0xc1
#   Failure when attempting to copy boot files.            (bcdboot exit 193)
# There is no switch for it: BFSVC_USE_EX_BINS is NOT an environment variable (it still logs
# ":y" when you set it to 0 or unset it). Windows 10's own bcdboot has none of that logic and
# writes the boot files cleanly, so run that instead. Verified: "Boot files successfully created."
$bcdExe = "$env:WINDIR\System32\bcdboot.exe"
$stage  = Join-Path $env:TEMP "st2k-bcdboot-$PID"
Remove-Item $stage -Recurse -Force -EA SilentlyContinue
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item "${winL}:\Windows\System32\bcdboot.exe" (Join-Path $stage 'bcdboot.exe') -Force -EA SilentlyContinue
Copy-Item "${winL}:\Windows\System32\bfsvc.dll"   (Join-Path $stage 'bfsvc.dll')   -Force -EA SilentlyContinue
if (Test-Path (Join-Path $stage 'bcdboot.exe')) { $bcdExe = Join-Path $stage 'bcdboot.exe'; Say "using the image's own bcdboot" }

# NEVER swallow bcdboot's output: a silent failure here produced a VM that booted to
# "The boot loader did not load an operating system" with no clue why. Capture it, check the
# exit code, and prove bootmgfw.efi landed. On failure, re-run verbose so the log says WHY.
$bcdOut  = & $bcdExe "${winL}:\Windows" /s "${espL}:" /f UEFI 2>&1
$bcdExit = $LASTEXITCODE
Say "bcdboot: $($bcdOut -join ' ') (exit $bcdExit)"
if ($bcdExit -ne 0) { Say "bcdboot verbose retry:"; (& $bcdExe "${winL}:\Windows" /s "${espL}:" /f UEFI /v 2>&1) | ForEach-Object { Write-Host "    $_" } }
Remove-Item $stage -Recurse -Force -EA SilentlyContinue
if ($bcdExit -ne 0 -or -not (Test-Path "${espL}:\EFI\Microsoft\Boot\bootmgfw.efi")) {
    Write-Host "[win10] bcdboot FAILED - the disk would not be bootable. Aborting." -ForegroundColor Red
    if ($isoMounted) { Dismount-DiskImage -ImagePath $Iso | Out-Null }
    Dismount-VHD -Path $osVhd -EA SilentlyContinue; exit 1
}
New-Item -ItemType Directory -Force "${winL}:\Windows\Panther" | Out-Null
Copy-Item $unattend "${winL}:\Windows\Panther\unattend.xml" -Force

if ($isoMounted) { Dismount-DiskImage -ImagePath $Iso | Out-Null }
Dismount-VHD -Path $osVhd

# ---- create the Gen2 VM (networkless; PowerShell Direct drives it) ---------
Say "creating Gen2 VM ($MemoryGB GB RAM), booting the applied disk"
New-VM -Name $Name -Generation 2 -MemoryStartupBytes ($MemoryGB * 1GB) -VHDPath $osVhd | Out-Null
Set-VM -Name $Name -AutomaticCheckpointsEnabled $false -CheckpointType Disabled
Set-VMProcessor -VMName $Name -Count 4
# Secure Boot OFF on purpose: Windows 10 22H2's boot manager is 2011-CA-signed, and a modern
# host's Secure Boot policy can refuse it. This VM exists to exercise OUR shell extension on
# real Win10, not to validate Microsoft's boot chain -- so remove the variable.
Set-VMFirmware -VMName $Name -EnableSecureBoot Off
Start-VM -Name $Name

# ---- wait for first-boot OOBE to finish + auto-logon (PS Direct usable) -----
Say "first boot: specialize + OOBE (unattended)... waiting for PowerShell Direct"
$cred = New-Object System.Management.Automation.PSCredential('vmadmin', (ConvertTo-SecureString 'P@ssw0rd!23' -AsPlainText -Force))
$deadline = (Get-Date).AddMinutes($BootTimeoutMin)
$session = $null
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 20
    try { $session = New-PSSession -VMName $Name -Credential $cred -EA Stop; Say "guest UP (PowerShell Direct connected)"; break } catch { }
}
if (-not $session) {
    Write-Host "[win10] TIMEOUT: guest never came up over PowerShell Direct. VM left running: vmconnect localhost $Name" -ForegroundColor Red
    "TIMEOUT - guest never reachable" | Set-Content (Join-Path $ResultDir 'win10-report.txt')
    exit 1
}

# ---- drive the clean-room test in the guest --------------------------------
Say "copying installer + samples into the guest"
Invoke-Command -Session $session -ScriptBlock { New-Item -ItemType Directory -Force 'C:\st2ktest\out' | Out-Null }
Copy-Item -ToSession $session -Path $installer -Destination 'C:\st2ktest\Setup.exe' -Force
foreach ($f in @('sample-gimp3.xcf', 'archive-photos.zip')) {
    $src = Join-Path $corpus $f
    if (Test-Path $src) { Copy-Item -ToSession $session -Path $src -Destination "C:\st2ktest\$f" -Force }
}

Say "running silent install -> doctor -> thumbnail .xcf + .zip (on REAL Windows 10)"
$r = Invoke-Command -Session $session -ScriptBlock {
    $o = [ordered]@{}
    $o.os = (Get-CimInstance Win32_OperatingSystem).Caption + ' build ' + (Get-CimInstance Win32_OperatingSystem).BuildNumber
    $p = Start-Process 'C:\st2ktest\Setup.exe' -ArgumentList '/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART' -PassThru
    if ($p.WaitForExit(180000)) { $o.installExit = $p.ExitCode } else { $o.installExit = 'HUNG'; $p.Kill() }
    Start-Sleep 5
    $st = 'C:\Program Files\SageThumbs2K\st2k.exe'
    $o.installed = Test-Path $st
    if ($o.installed) {
        $o.version = (& $st --version) 2>&1
        $o.doctor  = (& $st doctor 2>&1 | Out-String)
        & $st thumbnail 'C:\st2ktest\sample-gimp3.xcf' 'C:\st2ktest\out\xcf.png' 256 2>&1 | Out-Null
        & $st thumbnail 'C:\st2ktest\archive-photos.zip' 'C:\st2ktest\out\zip.png' 256 2>&1 | Out-Null
        $o.xcfBytes = if (Test-Path 'C:\st2ktest\out\xcf.png') { (Get-Item 'C:\st2ktest\out\xcf.png').Length } else { 0 }
        $o.zipBytes = if (Test-Path 'C:\st2ktest\out\zip.png') { (Get-Item 'C:\st2ktest\out\zip.png').Length } else { 0 }
        $o.doctorXcf = (& $st doctor 'C:\st2ktest\sample-gimp3.xcf' 2>&1 | Select-String 'Decode this file') -join ''
    }
    $o
}

Copy-Item -FromSession $session -Path 'C:\st2ktest\out\xcf.png' -Destination (Join-Path $ResultDir 'win10-xcf.png') -Force -EA SilentlyContinue
Copy-Item -FromSession $session -Path 'C:\st2ktest\out\zip.png' -Destination (Join-Path $ResultDir 'win10-zip.png') -Force -EA SilentlyContinue
$rep = @(
    "OS:            $($r.os)"
    "install exit:  $($r.installExit)"
    "st2k present:  $($r.installed)"
    "version:       $($r.version)"
    "xcf.png bytes: $($r.xcfBytes)"
    "zip.png bytes: $($r.zipBytes)"
    "doctor <xcf>:  $($r.doctorXcf)"
    "---- doctor ----"
    $r.doctor
)
$rep | Set-Content (Join-Path $ResultDir 'win10-report.txt')
Remove-PSSession $session

$pass = $r.installed -and ($r.installExit -eq 0) -and ([int]$r.xcfBytes -gt 1000) -and ([int]$r.zipBytes -gt 1000)
Say ("VERDICT: " + $(if ($pass) { 'PASS - install + decode verified on REAL Windows 10' } else { 'FAIL - see win10-report.txt' }))
$rep | Select-Object -First 8 | ForEach-Object { Write-Host "  $_" }

# On FAILURE keep the applied VHDX even without -Keep, so a retry can use -Resume and skip
# the 10-15 min image apply. Only a PASS reclaims the 60+ GB.
if (-not $Keep) {
    Say "tearing down VM"
    Stop-VM $Name -TurnOff -Force -EA SilentlyContinue; Remove-VM $Name -Force -EA SilentlyContinue
    if ($pass) { Remove-Item $osVhd -Force -EA SilentlyContinue }
    else { Say "kept $osVhd for a '-Resume' retry" }
}
else { Say "VM kept: vmconnect localhost $Name" }
exit $(if ($pass) { 0 } else { 1 })
