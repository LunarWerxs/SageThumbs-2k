<#
  run-win10-lifecycle.ps1 - the two LIVE installer proofs the 2026-09-19 release audit left
  open, on a real Windows 10 VM with two accounts. ELEVATED (Hyper-V + PowerShell Direct).

  F11: standard user A + administrator B. A fresh install elevated as B, while A owns the
       console session, must write the per-user shell state (the folder verb + the
       TypeOverlay suppression) to A's HKCU, not B's; the uninstall must clear A's, not B's.
       The fix: the installer's runasoriginaluser [Run] steps and the [Code]
       RunAsOriginalUser (schtasks /RU "<console user>" /NP) target the interactive console
       user, asked of the WTS API, never the elevating account.

  F13: a locked-DLL upgrade. The installed DLL is held open during an upgrade from 3.1.0 to
       3.1.1, so the swap is deferred to the next reboot (restartreplace). StaleAfterInstall
       then fires and the installer registers a SYSTEM ONSTART task "SageThumbs2K-Reregister".
       After a reboot and a STANDARD user sign-in (no admin), that task must have
       re-registered the DLL machine-wide, and a following clean install/uninstall must
       remove the task.

  Reuses run-win10-test.ps1's proven partition/apply/bcdboot machinery. -Resume reuses an
  applied VHDX and skips the ~12 min DISM apply.
#>
param(
    [string]$Iso = 'D:\isos\Win10_22H2_x64.iso',
    [string]$Name = 'st2k-win10-life',
    [string]$VmRoot = 'D:\Hyper-V',
    [int]$MemoryGB = 6,
    [int]$DiskGB = 64,
    [string]$ResultDir = 'D:\isos\win10-lifecycle-results',
    [int]$BootTimeoutMin = 25,
    [string]$Installer,     # the version under test (3.1.1)
    [string]$OldInstaller,  # the previous release (3.1.0), for the F13 upgrade
    [string]$Ver,           # the version under test, e.g. 3.1.1
    [string]$Unattend,      # autounattend-win10.xml
    [switch]$Keep,
    [switch]$Resume
)
$ErrorActionPreference = 'Stop'
function Say($m) { Write-Host "[life] $((Get-Date).ToString('HH:mm:ss')) $m" -ForegroundColor Cyan }
$adminPw = 'P@ssw0rd!23'
$stdPw   = 'Std!Pass456'

if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)) {
    Write-Host 'Run elevated.' -ForegroundColor Red; exit 1
}
if (-not (Test-Path $Iso)) { Write-Host "ISO not found: $Iso" -ForegroundColor Red; exit 1 }

# Defaults come from the repo: the version under test is Cargo.toml's, its installer is the
# one build-release.ps1 just put in dist\, the previous release is the newest lower-versioned
# installer in dist\, and the unattend file sits beside this script. Every one can be
# overridden explicitly (the way the 2026-09-19 proof was launched from a scratch copy).
$repo = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent   # app\
$ver = if ($Ver) { $Ver } else {
    ([regex]::Match((Get-Content -LiteralPath (Join-Path $repo 'Cargo.toml') -Raw), '(?m)^\s*version\s*=\s*"([^"]+)"')).Groups[1].Value
}
if (-not $ver) { Write-Host 'could not read the version from Cargo.toml (pass -Ver)' -ForegroundColor Red; exit 1 }
if (-not $Installer) { $Installer = Join-Path $repo "dist\SageThumbs2K-Setup-$ver.exe" }
if (-not $OldInstaller) {
    $older = Get-ChildItem (Join-Path $repo 'dist') -Filter 'SageThumbs2K-Setup-*.exe' -ErrorAction SilentlyContinue |
             Where-Object { $_.Name -match '^SageThumbs2K-Setup-(\d+\.\d+\.\d+)\.exe$' -and [version]$Matches[1] -lt [version]$ver } |
             Sort-Object { [version]([regex]::Match($_.Name, '(\d+\.\d+\.\d+)').Value) } -Descending | Select-Object -First 1
    if ($older) { $OldInstaller = $older.FullName }
}
if (-not $Unattend) { $Unattend = Join-Path $PSScriptRoot 'autounattend-win10.xml' }
foreach ($p in @($Installer, $OldInstaller, $Unattend)) {
    if (-not $p -or -not (Test-Path -LiteralPath $p)) { Write-Host "missing input: $p" -ForegroundColor Red; exit 1 }
}
$unattend = $Unattend
New-Item -ItemType Directory -Force $VmRoot, $ResultDir | Out-Null
Say "under test:  $Installer"
Say "previous:    $OldInstaller"

# ---------- partition / apply / bcdboot machinery (from run-win10-test.ps1) ----------
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

if (Get-VM -Name $Name -EA SilentlyContinue) { Say 'removing old VM'; Stop-VM $Name -TurnOff -Force -EA SilentlyContinue; Remove-VM $Name -Force -EA SilentlyContinue }
$osVhd = Join-Path $VmRoot "$Name.vhdx"
if (-not $Resume) { Remove-Item $osVhd -Force -EA SilentlyContinue }

$used = @()
$used += (Get-Volume -EA SilentlyContinue | Where-Object DriveLetter | ForEach-Object { "$($_.DriveLetter)".ToUpper() })
$used += (Get-PSDrive -PSProvider FileSystem -EA SilentlyContinue | ForEach-Object { $_.Name.ToUpper() })
$used = $used | Select-Object -Unique
$cands = @('W','V','U','T','Q','N','M','L','K','J','G','Y','X','Z') | Where-Object { $_ -notin $used }
$isoMounted = $false

if ($Resume) {
    if (-not (Test-Path $osVhd)) { Write-Host "[life] -Resume needs an existing $osVhd" -ForegroundColor Red; exit 1 }
    Say "RESUME: reusing $osVhd"
    Dismount-VHD -Path $osVhd -EA SilentlyContinue
    $diskNum = (Mount-VHD -Path $osVhd -Passthru | Get-Disk).Number
    $espN = (Get-Partition -DiskNumber $diskNum | Where-Object { $_.GptType -eq '{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}' } | Select-Object -First 1).PartitionNumber
    if ($espN) {
        "select disk $diskNum`r`nselect partition $espN`r`nformat fs=fat32 quick label=System`r`nexit`r`n" | Set-Content (Join-Path $env:TEMP "life-fmt-$PID.txt") -Encoding ascii
        diskpart /s (Join-Path $env:TEMP "life-fmt-$PID.txt") | Out-Null
        Remove-Item (Join-Path $env:TEMP "life-fmt-$PID.txt") -Force -EA SilentlyContinue
    }
} else {
    Say "creating $DiskGB GB VHDX + partitioning"
    New-VHD -Path $osVhd -SizeBytes ($DiskGB * 1GB) -Dynamic | Out-Null
    $diskNum = (Mount-VHD -Path $osVhd -Passthru | Get-Disk).Number
    $dp = "select disk $diskNum`r`nclean`r`nconvert gpt`r`ncreate partition efi size=300`r`nformat fs=fat32 quick label=System`r`ncreate partition msr size=16`r`ncreate partition primary`r`nformat fs=ntfs quick label=Windows`r`nexit`r`n"
    $dpFile = Join-Path $env:TEMP "life-dp-$PID.txt"; $dp | Set-Content $dpFile -Encoding ascii
    diskpart /s $dpFile | Out-Null; Remove-Item $dpFile -Force -EA SilentlyContinue; Start-Sleep 3
}

$parts   = Get-Partition -DiskNumber $diskNum
$espPart = $parts | Where-Object { $_.GptType -eq '{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}' } | Select-Object -First 1
$winPart = $parts | Where-Object { $_.GptType -eq '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}' } | Sort-Object Size -Descending | Select-Object -First 1
$espL = Set-VerifiedLetter -Disk $diskNum -Part $espPart.PartitionNumber -Candidates $cands
$winL = if ($espL) { Set-VerifiedLetter -Disk $diskNum -Part $winPart.PartitionNumber -Candidates ($cands | Where-Object { $_ -ne $espL }) }
if (-not $espL -or -not $winL) { Write-Host "[life] drive-letter assignment failed (ESP=$espL WIN=$winL)" -ForegroundColor Red; Dismount-VHD -Path $osVhd -EA SilentlyContinue; exit 1 }
Say "drive letters: ESP=${espL}: WIN=${winL}:"

if (-not $Resume) {
    Say 'applying Windows 10 Home (index 1) with DISM (~10-15 min)'
    $mount = Mount-DiskImage -ImagePath $Iso -PassThru; $isoMounted = $true
    $isoLtr = ($mount | Get-Volume).DriveLetter
    Expand-WindowsImage -ImagePath "${isoLtr}:\sources\install.wim" -Index 1 -ApplyPath "${winL}:\" | Out-Null
}
if (-not (Test-Path "${winL}:\Windows\System32\ntoskrnl.exe")) { Write-Host '[life] no Windows on target' -ForegroundColor Red; if ($isoMounted) { Dismount-DiskImage -ImagePath $Iso | Out-Null }; Dismount-VHD -Path $osVhd -EA SilentlyContinue; exit 1 }

Say 'bcdboot + unattend'
$bcdExe = "$env:WINDIR\System32\bcdboot.exe"
$stage  = Join-Path $env:TEMP "life-bcdboot-$PID"
Remove-Item $stage -Recurse -Force -EA SilentlyContinue; New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item "${winL}:\Windows\System32\bcdboot.exe" (Join-Path $stage 'bcdboot.exe') -Force -EA SilentlyContinue
Copy-Item "${winL}:\Windows\System32\bfsvc.dll"   (Join-Path $stage 'bfsvc.dll')   -Force -EA SilentlyContinue
if (Test-Path (Join-Path $stage 'bcdboot.exe')) { $bcdExe = Join-Path $stage 'bcdboot.exe' }
$bcdOut = & $bcdExe "${winL}:\Windows" /s "${espL}:" /f UEFI 2>&1
$bcdExit = $LASTEXITCODE
Say "bcdboot: $($bcdOut -join ' ') (exit $bcdExit)"
Remove-Item $stage -Recurse -Force -EA SilentlyContinue
if ($bcdExit -ne 0 -or -not (Test-Path "${espL}:\EFI\Microsoft\Boot\bootmgfw.efi")) { Write-Host '[life] bcdboot FAILED' -ForegroundColor Red; if ($isoMounted) { Dismount-DiskImage -ImagePath $Iso | Out-Null }; Dismount-VHD -Path $osVhd -EA SilentlyContinue; exit 1 }
New-Item -ItemType Directory -Force "${winL}:\Windows\Panther" | Out-Null
Copy-Item $unattend "${winL}:\Windows\Panther\unattend.xml" -Force
if ($isoMounted) { Dismount-DiskImage -ImagePath $Iso | Out-Null }
Dismount-VHD -Path $osVhd

# ---------- boot the VM ----------
Say "creating Gen2 VM ($MemoryGB GB), booting"
New-VM -Name $Name -Generation 2 -MemoryStartupBytes ($MemoryGB * 1GB) -VHDPath $osVhd | Out-Null
Set-VM -Name $Name -AutomaticCheckpointsEnabled $false -CheckpointType Disabled
Set-VMProcessor -VMName $Name -Count 4
Set-VMFirmware -VMName $Name -EnableSecureBoot Off
Start-VM -Name $Name

$adminCred = New-Object System.Management.Automation.PSCredential('vmadmin', (ConvertTo-SecureString $adminPw -AsPlainText -Force))
function Wait-Session([System.Management.Automation.PSCredential]$Cred, [int]$Min = 20) {
    $deadline = (Get-Date).AddMinutes($Min); $s = $null
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Seconds 15
        try { $s = New-PSSession -VMName $Name -Credential $Cred -EA Stop; break } catch { }
    }
    return $s
}
Say 'first boot: waiting for PowerShell Direct (vmadmin)'
$session = Wait-Session $adminCred $BootTimeoutMin
if (-not $session) { Write-Host '[life] TIMEOUT: guest never came up' -ForegroundColor Red; exit 1 }
Say 'guest UP'

# ---------- copy installers in ----------
Invoke-Command -Session $session -ScriptBlock { New-Item -ItemType Directory -Force 'C:\st2ktest' | Out-Null }
Copy-Item -ToSession $session -Path $Installer    -Destination 'C:\st2ktest\Setup-new.exe' -Force
Copy-Item -ToSession $session -Path $OldInstaller -Destination 'C:\st2ktest\Setup-old.exe' -Force

# ---------- create standard user A ----------
Say 'creating standard user A (stduser)'
Invoke-Command -Session $session -ArgumentList $stdPw -ScriptBlock {
    param($pw)
    $sec = ConvertTo-SecureString $pw -AsPlainText -Force
    if (-not (Get-LocalUser -Name 'stduser' -EA SilentlyContinue)) {
        New-LocalUser -Name 'stduser' -Password $sec -FullName 'Standard A' -Description 'audit F11/F13' -PasswordNeverExpires -AccountNeverExpires | Out-Null
        Add-LocalGroupMember -Group 'Users' -Member 'stduser'
    }
    # Force the profile to be created now so its NTUSER.DAT exists for HKU checks.
    $sid = (Get-LocalUser 'stduser').SID.Value
    New-Item -ItemType Directory -Force 'C:\st2ktest\out' | Out-Null
    [pscustomobject]@{ stdSid = $sid; adminSid = (Get-LocalUser 'vmadmin').SID.Value }
} | Tee-Object -Variable sids | Out-Null
$stdSid   = $sids.stdSid
$adminSid = $sids.adminSid
Say "SIDs: A(stduser)=$stdSid  B(vmadmin)=$adminSid"

$results = [ordered]@{ under_test = (Split-Path $Installer -Leaf); previous = (Split-Path $OldInstaller -Leaf); std_sid = $stdSid; admin_sid = $adminSid }

# A guest-side helper: is the per-user folder verb present in a given hive file?
$probeUserShell = {
    param($SidOrHive, $IsLiveSid)
    # Returns $true if Software\Classes\Directory\shell\SageThumbs2K.Prebuild exists in that hive.
    if ($IsLiveSid) {
        return (Test-Path "Registry::HKEY_USERS\$SidOrHive\Software\Classes\Directory\shell\SageThumbs2K.Prebuild")
    } else {
        $tmp = 'HKU\st2ktmp'
        reg load $tmp $SidOrHive *> $null
        try {
            return (Test-Path 'Registry::HKEY_USERS\st2ktmp\Software\Classes\Directory\shell\SageThumbs2K.Prebuild')
        } finally {
            [gc]::Collect(); Start-Sleep 1; reg unload $tmp *> $null
        }
    }
}

# ============================================================
# Switch auto-logon to standard user A, reboot, so A owns the console session.
# ============================================================
function Set-AutoLogon([string]$User, [string]$Pass) {
    Invoke-Command -Session $session -ArgumentList $User, $Pass -ScriptBlock {
        param($u, $p)
        $k = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
        Set-ItemProperty $k 'AutoAdminLogon' '1'
        Set-ItemProperty $k 'DefaultUserName' $u
        Set-ItemProperty $k 'DefaultPassword' $p
        Set-ItemProperty $k 'DefaultDomainName' $env:COMPUTERNAME
        Remove-ItemProperty $k 'AutoLogonCount' -EA SilentlyContinue
    }
}
function Reboot-Guest([System.Management.Automation.PSCredential]$AsCred, [string]$Who) {
    Invoke-Command -Session $session -ScriptBlock { Restart-Computer -Force } -EA SilentlyContinue
    if ($session) { Remove-PSSession $session -EA SilentlyContinue }
    Start-Sleep 20
    Say "reboot: waiting for $Who over PowerShell Direct"
    $script:session = Wait-Session $AsCred 15
    if (-not $script:session) { throw "guest never came back as $Who" }
}
function ConsoleUserInGuest() {
    Invoke-Command -Session $session -ScriptBlock {
        # The interactive console user as DOMAIN\name. Windows 10 HOME has no query.exe /
        # qwinsta (Terminal Services tools), so ask CIM, which every edition answers.
        (Get-CimInstance Win32_ComputerSystem).UserName
    }
}

Say 'F11: switch auto-logon to standard user A, reboot'
Set-AutoLogon 'stduser' $stdPw
Reboot-Guest $adminCred 'vmadmin (via PS Direct after A auto-logon)'
Start-Sleep 25  # let A's interactive console session settle
$console = ConsoleUserInGuest
$results.f11_console_user = $console
Say "console user after reboot: $console (expect stduser)"

# ---------- F11 install: elevated as B (this PSDirect admin session), console = A ----------
Say 'F11: fresh install of the version under test (elevated as vmadmin B; console is A)'
$f11 = Invoke-Command -Session $session -ArgumentList $stdSid, $adminSid -ScriptBlock {
    param($aSid, $bSid)
    $o = [ordered]@{}
    $p = Start-Process 'C:\st2ktest\Setup-new.exe' -ArgumentList '/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART' -PassThru -Wait
    $o.installExit = $p.ExitCode
    Start-Sleep 6
    $verbKey = 'Software\Classes\Directory\shell\SageThumbs2K.Prebuild'
    $o.aHasVerb_afterInstall = Test-Path "Registry::HKEY_USERS\$aSid\$verbKey"
    # B's hive: loaded if signed in; else load NTUSER.DAT from its profile.
    $bLive = Test-Path "Registry::HKEY_USERS\$bSid"
    if ($bLive) {
        $o.bHasVerb_afterInstall = Test-Path "Registry::HKEY_USERS\$bSid\$verbKey"
    } else {
        reg load 'HKU\st2kB' 'C:\Users\vmadmin\NTUSER.DAT' *> $null
        $o.bHasVerb_afterInstall = Test-Path "Registry::HKEY_USERS\st2kB\$verbKey"
        [gc]::Collect(); Start-Sleep 1; reg unload 'HKU\st2kB' *> $null
    }
    $o.installed = Test-Path 'C:\Program Files\SageThumbs2K\st2k.exe'
    $o
}
$results.f11_install = $f11
Say "F11 install: exit=$($f11.installExit) A_verb=$($f11.aHasVerb_afterInstall) B_verb=$($f11.bHasVerb_afterInstall)"

# ---------- F11 uninstall: elevated as B, console = A ----------
Say 'F11: uninstall (elevated as vmadmin B; console is A)'
$f11u = Invoke-Command -Session $session -ArgumentList $stdSid -ScriptBlock {
    param($aSid)
    $o = [ordered]@{}
    $unins = 'C:\Program Files\SageThumbs2K\unins000.exe'
    $o.uninsExists = Test-Path $unins
    if ($o.uninsExists) {
        $p = Start-Process $unins -ArgumentList '/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART' -PassThru -Wait
        $o.uninsExit = $p.ExitCode
        Start-Sleep 6
    }
    $verbKey = 'Software\Classes\Directory\shell\SageThumbs2K.Prebuild'
    $o.aHasVerb_afterUninstall = Test-Path "Registry::HKEY_USERS\$aSid\$verbKey"
    $o.stillInstalled = Test-Path 'C:\Program Files\SageThumbs2K\st2k.exe'
    $o
}
$results.f11_uninstall = $f11u
Say "F11 uninstall: A_verb_after=$($f11u.aHasVerb_afterUninstall) stillInstalled=$($f11u.stillInstalled)"

# ============================================================
# F13: locked-DLL upgrade -> SYSTEM ONSTART task -> reboot -> standard sign-in
# ============================================================
Say 'F13: install the PREVIOUS version (3.1.0) as the base'
$f13base = Invoke-Command -Session $session -ScriptBlock {
    $p = Start-Process 'C:\st2ktest\Setup-old.exe' -ArgumentList '/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART' -PassThru -Wait
    $dll = 'C:\Program Files\SageThumbs2K\sagethumbs2k.dll'
    [pscustomobject]@{ exit = $p.ExitCode; dllVer = (Get-Item $dll -EA SilentlyContinue).VersionInfo.FileVersion }
}
$results.f13_base_install = $f13base
Say "F13 base (3.1.0): exit=$($f13base.exit) dllVer=$($f13base.dllVer)"

Say 'F13: LOCK the installed DLL, then upgrade to the version under test'
$f13up = Invoke-Command -Session $session -ScriptBlock {
    $o = [ordered]@{}
    $dll = 'C:\Program Files\SageThumbs2K\sagethumbs2k.dll'
    # Hold the DLL open in a background process so the upgrade cannot replace it. NOT a
    # LoadLibrary: Windows lets a mapped image be RENAMED, and the installer's own
    # SwapAsideInUseDll renames a loaded DLL aside and drops the new one in place, so the
    # stale path never fires for a merely loaded DLL. A handle opened WITHOUT delete sharing
    # (what an antivirus scan or an indexer holds) refuses the rename with a sharing
    # violation; Inno then falls back to restartreplace, the on-disk DLL stays the OLD
    # version, and StaleAfterInstall fires - the case F13 is about. A script FILE, not an
    # inline -Command: nested quotes do not survive Start-Process's own quoting.
    $hold = @'
$fs = [System.IO.File]::Open('C:\Program Files\SageThumbs2K\sagethumbs2k.dll', 'Open', 'Read', 'Read')
"held without delete sharing, length=$($fs.Length)" | Set-Content C:\st2ktest\hold.txt
Start-Sleep 300
'@
    Set-Content -Path 'C:\st2ktest\hold.ps1' -Value $hold -Encoding ascii
    $holder = Start-Process powershell -PassThru -WindowStyle Hidden -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','C:\st2ktest\hold.ps1'
    Start-Sleep 5
    $o.holderLoaded = (Get-Content 'C:\st2ktest\hold.txt' -Raw -EA SilentlyContinue)
    $o.holderPid = $holder.Id
    # /NOCLOSEAPPLICATIONS: a silent Setup otherwise closes whatever holds its files through
    # the Restart Manager (it terminated the holder above in two earlier runs and swapped the
    # DLL in place, so the stale path never fired). An admin deploying with that switch, or a
    # holder the Restart Manager cannot close, is exactly the machine F13 is about. /LOG keeps
    # Inno's own account of the in-use file and the deferred replace.
    $p = Start-Process 'C:\st2ktest\Setup-new.exe' -ArgumentList '/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART','/NOCLOSEAPPLICATIONS','/LOG=C:\st2ktest\upgrade.log' -PassThru -Wait
    $o.upgradeExit = $p.ExitCode
    $o.holderStillAlive = [bool](Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -match 'hold\.ps1' })
    $o.innoLog = ((Get-Content 'C:\st2ktest\upgrade.log' -EA SilentlyContinue | Select-String -Pattern 'in use|RestartReplace|Restart Manager|deferred|Reregister' | Select-Object -First 8 | ForEach-Object { $_.Line.Trim() }) -join ' || ')
    Start-Sleep 6
    # The SYSTEM ONSTART task must exist now (StaleAfterInstall fired).
    $q = (schtasks /Query /TN 'SageThumbs2K-Reregister' /FO LIST /V 2>&1 | Out-String)
    $o.taskPresentAfterUpgrade = ($LASTEXITCODE -eq 0)
    $qc = ($q -replace '\s+', ' ')
    $o.taskQuery = $qc.Substring(0, [Math]::Min(400, $qc.Length))
    $m = [regex]::Match($q, 'Run As User:\s*(\S+)')
    $o.taskRunAs = if ($m.Success) { $m.Groups[1].Value } else { '' }
    $o.dllVerOnDiskBeforeReboot = (Get-Item $dll -EA SilentlyContinue).VersionInfo.FileVersion
    # What the installer itself thought: its own log names a deferred swap.
    $o.pendingRenames = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager' -Name PendingFileRenameOperations -EA SilentlyContinue).PendingFileRenameOperations -join ' | '
    $o.runOnceReregister = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\RunOnce' -Name SageThumbs2KReregister -EA SilentlyContinue).SageThumbs2KReregister
    # Release the lock so the pending rename can swap on reboot.
    Stop-Process -Id $holder.Id -Force -EA SilentlyContinue
    $o
}
$results.f13_upgrade = $f13up
Say "F13 upgrade: exit=$($f13up.upgradeExit) task=$($f13up.taskPresentAfterUpgrade) runAs=$($f13up.taskRunAs) dllBeforeReboot=$($f13up.dllVerOnDiskBeforeReboot) holderAlive=$($f13up.holderStillAlive) pending=$($f13up.pendingRenames)"
Say "F13 inno log: $($f13up.innoLog)"

Say 'F13: reboot with STANDARD user A auto-logon (no admin sign-in)'
Set-AutoLogon 'stduser' $stdPw
Reboot-Guest $adminCred 'vmadmin (via PS Direct; A auto-logs on at the console)'
Start-Sleep 30  # let the SYSTEM ONSTART task run and A sign in
$f13post = Invoke-Command -Session $session -ScriptBlock {
    $o = [ordered]@{}
    $dll = 'C:\Program Files\SageThumbs2K\sagethumbs2k.dll'
    $o.dllVerAfterReboot = (Get-Item $dll -EA SilentlyContinue).VersionInfo.FileVersion
    $o.consoleUser = (Get-CimInstance Win32_ComputerSystem).UserName
    # The SYSTEM ONSTART task should still be present until a clean install/uninstall.
    schtasks /Query /TN 'SageThumbs2K-Reregister' /FO LIST 2>&1 | Out-Null
    $o.taskStillPresent = ($LASTEXITCODE -eq 0)
    # Was the DLL actually registered machine-wide? Probe the shellex thumbnail handler on a
    # known class the installer registers. Use st2k doctor as the authoritative self-check.
    $st = 'C:\Program Files\SageThumbs2K\st2k.exe'
    $o.doctor = if (Test-Path $st) { (& $st doctor 2>&1 | Out-String) } else { 'st2k.exe missing' }
    $o
}
$results.f13_after_reboot = $f13post
Say "F13 after reboot: dll=$($f13post.dllVerAfterReboot) consoleUser=$($f13post.consoleUser) taskStillPresent=$($f13post.taskStillPresent)"

Say 'F13: clean reinstall of the version under test (DLL not locked) must remove the task'
$f13clean = Invoke-Command -Session $session -ScriptBlock {
    $p = Start-Process 'C:\st2ktest\Setup-new.exe' -ArgumentList '/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART' -PassThru -Wait
    Start-Sleep 6
    schtasks /Query /TN 'SageThumbs2K-Reregister' /FO LIST 2>&1 | Out-Null
    [pscustomobject]@{ reinstallExit = $p.ExitCode; taskPresentAfterCleanReinstall = ($LASTEXITCODE -eq 0) }
}
$results.f13_clean_reinstall = $f13clean
Say "F13 clean reinstall: task_present=$($f13clean.taskPresentAfterCleanReinstall) (expect False)"

# ---------- verdict ----------
$f11pass = ($f11.installExit -eq 0) -and $f11.aHasVerb_afterInstall -and (-not $f11.bHasVerb_afterInstall) -and (-not $f11u.aHasVerb_afterUninstall)
$f13pass = ($f13up.upgradeExit -eq 0) -and $f13up.taskPresentAfterUpgrade -and `
           ($f13post.dllVerAfterReboot -and [version]$f13post.dllVerAfterReboot -ge [version]$ver) -and `
           (-not $f13clean.taskPresentAfterCleanReinstall)
$results.f11_verdict = if ($f11pass) { 'PASS' } else { 'FAIL' }
$results.f13_verdict = if ($f13pass) { 'PASS' } else { 'FAIL' }
$results | ConvertTo-Json -Depth 6 | Set-Content (Join-Path $ResultDir 'lifecycle-results.json')
Get-Content (Join-Path $ResultDir 'lifecycle-results.json') | Write-Host
Say "F11 = $($results.f11_verdict);  F13 = $($results.f13_verdict)"

if ($session) { Remove-PSSession $session -EA SilentlyContinue }
if (-not $Keep) {
    Say 'tearing down VM'
    Stop-VM $Name -TurnOff -Force -EA SilentlyContinue; Remove-VM $Name -Force -EA SilentlyContinue
    if ($f11pass -and $f13pass) { Remove-Item $osVhd -Force -EA SilentlyContinue } else { Say "kept $osVhd for a -Resume retry" }
} else { Say "VM kept: vmconnect localhost $Name" }
exit $(if ($f11pass -and $f13pass) { 0 } else { 1 })
