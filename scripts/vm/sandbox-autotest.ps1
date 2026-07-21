<#
  sandbox-autotest.ps1 — AUTOMATED clean-room test in Windows Sandbox with read-back.

      pwsh scripts\vm\sandbox-autotest.ps1                 # newest dist installer
      pwsh scripts\vm\sandbox-autotest.ps1 -Installer <path>

  Unlike test-sandbox.ps1 (interactive), this drives the whole test unattended and
  reports a verdict on the HOST:
    1. boots a throwaway Windows Sandbox,
    2. SILENT-installs the built installer,
    3. runs `st2k doctor`, thumbnails a modern GIMP .xcf and an image .zip,
    4. writes every result to a host folder mapped writable,
    5. the host polls for a DONE sentinel, reads the results, prints PASS/FAIL,
    6. closes the Sandbox (everything discarded).

  Proves, on a machine that has never seen SageThumbs: the installer runs silently and
  registers cleanly, and the decode pipeline (incl. the new native XCF decoder + archive
  thumbnails) actually produces images. NOTE: the Sandbox mirrors the host OS, so this is
  a clean WIN11 test, not Win10 (use new-win10-vm.ps1 for real Win10).
#>
param([string]$Installer, [int]$TimeoutSec = 360)

$ErrorActionPreference = 'Stop'
if (-not (Test-Path "$env:WINDIR\System32\WindowsSandbox.exe")) {
    Write-Host "Windows Sandbox is not installed (enable 'Containers-DisposableClientVM')." -ForegroundColor Red
    exit 1
}

$root   = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
$dist   = Join-Path $root 'dist'
$corpus = Join-Path (Split-Path $root -Parent) 'test-corpus'
if (-not $Installer) {
    $Installer = Get-ChildItem $dist -Filter 'SageThumbs2K-Setup-*.exe' -EA SilentlyContinue |
                 Sort-Object LastWriteTime -Descending | Select-Object -First 1 -Exp FullName
}
if (-not $Installer -or -not (Test-Path $Installer)) {
    Write-Host "No installer found; run scripts\build-release.ps1 first." -ForegroundColor Red; exit 1
}
$installerDir  = Split-Path $Installer -Parent
$installerName = Split-Path $Installer -Leaf

# Fresh host-side scratch: a payload folder (the in-guest test script), a writable
# results folder the guest writes back into, and an ISOLATED installer folder holding
# ONLY the chosen installer. Isolation matters: dist\ accumulates old installers, and
# the in-guest glob would otherwise grab the FIRST by name (an ancient build) — which is
# exactly what silently made this test hang. One file in, one file globbed.
$work    = Join-Path $env:TEMP ("st2k-autotest-" + (Get-Random))
$payload = Join-Path $work 'payload'
$results = Join-Path $work 'results'
$instDir = Join-Path $work 'installer'
New-Item -ItemType Directory -Force $payload, $results, $instDir | Out-Null
Copy-Item $Installer (Join-Path $instDir $installerName) -Force

# The in-guest test. Runs as WDAGUtilityAccount (admin), so the installer's
# requireAdministrator manifest is satisfied and regsvr32 can write HKLM.
@'
$ErrorActionPreference = "Continue"
$res = "C:\Users\WDAGUtilityAccount\Desktop\results"
$cor = "C:\Users\WDAGUtilityAccount\Desktop\corpus"
# A boot marker FIRST, so the host can tell "sandbox never booted" (empty results) from
# "a step hung" (marker present). Windows Sandbox occasionally fails to boot when a prior
# instance is still tearing down; the host side waits for that before launching.
"booted" | Out-File "$res\stage.txt"
$inst = Get-ChildItem "C:\Users\WDAGUtilityAccount\Desktop\installer\SageThumbs2K-Setup-*.exe" | Select-Object -First 1
try {
    "installing" | Out-File -Append "$res\stage.txt"
    Start-Process $inst.FullName -ArgumentList "/VERYSILENT","/SUPPRESSMSGBOXES","/NORESTART" -Wait
    "installed" | Out-File -Append "$res\stage.txt"
} catch { "install-exception: $_" | Out-File "$res\install-error.txt" }
Start-Sleep -Seconds 6
$st2k = "C:\Program Files\SageThumbs2K\st2k.exe"
if (Test-Path $st2k) {
    & $st2k doctor 2>&1 | Out-File "$res\doctor.txt"
    $xcf = Get-ChildItem "$cor\sample-gimp3.xcf" -EA SilentlyContinue
    if ($xcf) {
        & $st2k thumbnail $xcf.FullName "$res\xcf.png" 256 2>&1 | Out-File "$res\xcf-log.txt"
        & $st2k doctor $xcf.FullName 2>&1 | Out-File "$res\doctor-xcf.txt"
    }
    $zip = Get-ChildItem "$cor\archive-photos.zip" -EA SilentlyContinue
    if ($zip) { & $st2k thumbnail $zip.FullName "$res\zip.png" 256 2>&1 | Out-File "$res\zip-log.txt" }
} else {
    "st2k.exe not found - install did not land" | Out-File "$res\install-error.txt"
}
"done" | Out-File "$res\DONE.txt"
'@ | Set-Content (Join-Path $payload 'runtest.ps1') -Encoding UTF8

$maps = @(
    "    <MappedFolder><HostFolder>$instDir</HostFolder><SandboxFolder>C:\Users\WDAGUtilityAccount\Desktop\installer</SandboxFolder><ReadOnly>true</ReadOnly></MappedFolder>"
    "    <MappedFolder><HostFolder>$payload</HostFolder><SandboxFolder>C:\Users\WDAGUtilityAccount\Desktop\payload</SandboxFolder><ReadOnly>true</ReadOnly></MappedFolder>"
    "    <MappedFolder><HostFolder>$results</HostFolder><SandboxFolder>C:\Users\WDAGUtilityAccount\Desktop\results</SandboxFolder><ReadOnly>false</ReadOnly></MappedFolder>"
)
if (Test-Path $corpus) {
    $maps += "    <MappedFolder><HostFolder>$corpus</HostFolder><SandboxFolder>C:\Users\WDAGUtilityAccount\Desktop\corpus</SandboxFolder><ReadOnly>true</ReadOnly></MappedFolder>"
}
$wsb = @"
<Configuration>
  <MappedFolders>
$($maps -join "`r`n")
  </MappedFolders>
  <LogonCommand>
    <Command>powershell.exe -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\Users\WDAGUtilityAccount\Desktop\payload\runtest.ps1</Command>
  </LogonCommand>
  <MemoryInMB>8192</MemoryInMB>
  <Networking>Enable</Networking>
</Configuration>
"@
$wsbPath = Join-Path $work 'autotest.wsb'
$wsb | Set-Content $wsbPath -Encoding UTF8

# Only ONE Windows Sandbox can run at a time, and launching while a prior instance is
# still tearing down silently no-ops (the #1 cause of "booted nothing" flakes here). Make
# sure the field is clear first: stop any lingering Sandbox and wait for its VM memory
# process to actually exit before we launch.
Get-Process WindowsSandbox, WindowsSandboxClient, WindowsSandboxRemoteSession -EA SilentlyContinue | Stop-Process -Force -EA SilentlyContinue
$clear = (Get-Date).AddSeconds(30)
while ((Get-Process WindowsSandbox*, vmmem* -EA SilentlyContinue) -and (Get-Date) -lt $clear) { Start-Sleep -Seconds 2 }

Write-Host "[autotest] launching Sandbox with $installerName (silent install + probe)..." -ForegroundColor Cyan
Start-Process "$env:WINDIR\System32\WindowsSandbox.exe" -ArgumentList $wsbPath

$done  = Join-Path $results 'DONE.txt'
$stage = Join-Path $results 'stage.txt'
$booted = $false
$deadline = (Get-Date).AddSeconds($TimeoutSec)
while (-not (Test-Path $done) -and (Get-Date) -lt $deadline) {
    if (-not $booted -and (Test-Path $stage)) { $booted = $true; Write-Host "[autotest] sandbox booted, running..." -ForegroundColor DarkGray }
    Start-Sleep -Seconds 5
}
if (-not $booted -and -not (Test-Path $done)) {
    Write-Host "[autotest] NOTE: no boot marker — Windows Sandbox did not start (a known intermittent Sandbox issue). Re-run once." -ForegroundColor Yellow
}

$pass = $true
if (-not (Test-Path $done)) {
    Write-Host "[autotest] TIMEOUT after ${TimeoutSec}s - no DONE sentinel." -ForegroundColor Red
    $pass = $false
} else {
    Start-Sleep -Seconds 2  # let the last writes flush across the mapped share
    Write-Host "`n===== doctor.txt (verdict lines) =====" -ForegroundColor Cyan
    if (Test-Path "$results\doctor.txt") {
        Get-Content "$results\doctor.txt" | Select-String -Pattern 'Verdict','problem','No blocking','FAIL','loads OK','Hooked by' | ForEach-Object { Write-Host "  $_" }
    } else { Write-Host "  (no doctor.txt)"; $pass = $false }

    if (Test-Path "$results\install-error.txt") {
        Write-Host "  INSTALL ERROR:" -ForegroundColor Red; Get-Content "$results\install-error.txt" | ForEach-Object { Write-Host "    $_" }; $pass = $false
    }

    function Check($name, $path, $minBytes) {
        $sz = if (Test-Path $path) { (Get-Item $path).Length } else { 0 }
        if ($sz -ge $minBytes) { Write-Host ("  OK   {0}: {1} bytes" -f $name, $sz) -ForegroundColor Green }
        else { Write-Host ("  FAIL {0}: {1} bytes (expected >= {2})" -f $name, $sz, $minBytes) -ForegroundColor Red; $script:pass = $false }
    }
    Write-Host "`n===== generated thumbnails =====" -ForegroundColor Cyan
    Check 'modern GIMP .xcf' "$results\xcf.png" 1000
    Check 'image .zip'       "$results\zip.png" 1000
    Write-Host "`n===== doctor <that.xcf> (decode probe) =====" -ForegroundColor Cyan
    if (Test-Path "$results\doctor-xcf.txt") { Get-Content "$results\doctor-xcf.txt" | Select-String -Pattern 'Decode this file' | ForEach-Object { Write-Host "  $_" } }
}

# Copy the produced thumbnails out before we discard the Sandbox, so they can be eyeballed.
$keep = Join-Path ([System.IO.Path]::GetTempPath()) 'st2k-autotest-results'
New-Item -ItemType Directory -Force $keep | Out-Null
Get-ChildItem $results -File -EA SilentlyContinue | Copy-Item -Destination $keep -Force
Write-Host "`n[autotest] results copied to $keep" -ForegroundColor DarkGray

Write-Host "[autotest] closing Sandbox..." -ForegroundColor Cyan
Get-Process WindowsSandbox, WindowsSandboxClient, WindowsSandboxRemoteSession -EA SilentlyContinue | Stop-Process -Force -EA SilentlyContinue

Write-Host ("`n[autotest] {0}" -f $(if ($pass) { 'PASS - clean-room install + decode verified' } else { 'FAIL - see above' })) -ForegroundColor $(if ($pass) { 'Green' } else { 'Red' })
exit $(if ($pass) { 0 } else { 1 })
