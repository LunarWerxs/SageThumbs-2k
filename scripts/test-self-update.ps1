# Drive the REAL one-click self-update pipeline end to end against a built installer, and
# fail unless the upgrade actually lands on disk.
#
# Why this exists: 1.3.3..=1.10.0 shipped an updater whose own write-mode file lock made
# Windows refuse to launch every downloaded installer (SE_ERR_SHARE), reported to users as
# "installing an update needs an administrator". Twenty releases, 100% failure rate, zero
# test coverage - because every existing test checked pieces and nothing ever ran the real
# pipeline against a real executable. This script is that missing test. The updater must
# NEVER ship broken again (owner directive, 2026-08-10); it gates CI on every push
# (self-update-smoke job) and the release ritual on the exact artifact being published
# (release.ps1 [4d/6]).
#
# What it does:
#   1. If SageThumbs 2K is not installed, silent-install $Setup as the baseline.
#   2. Run `$App --update-selftest $Setup` - the app-side verify -> locked-temp-copy ->
#      elevated-silent-launch pipeline, through the same functions the About-card updater
#      calls (only the network download is substituted).
#   3. Poll until the INSTALLED exe is replaced and its version matches $App's, i.e. the
#      in-place upgrade genuinely completed. Throw on timeout or mismatch.
#
# -Hold (issue #60, needs an elevated shell): before step 2, hold install files the way the
# reporter's PC did. A SYSTEM process in session 0 maps the shell extension DLL and a second
# binary (a holder Restart Manager cannot close: under 3.6.0 setup answered its own
# Abort/Retry/Ignore with Abort and rolled the update back without a word), and a handle
# WITHOUT delete sharing pins a third file (a scanner's grip: not even renameable). The upgrade
# must land anyway, and every holder must still be alive afterwards: an update that works by
# killing the user's programs is not a fix. It also starts from what that failed 3.6.0 update
# LEFT behind (reproduced 2026-10-06): the shell extension renamed to `.old<N>`, still mapped,
# and its own name empty, because the rollback never put it back. Baseline and upgrade are the
# same version here, so "the DLL is version X afterwards" cannot tell a replaced DLL from an
# untouched one; "the DLL exists afterwards" can, and it is what the user needs.
#
# Elevation: the launched setup elevates via the `runas` verb. On GitHub-hosted runners and
# on dev boxes with silent admin consent this shows no prompt. It INSTALLS/UPGRADES the
# machine it runs on - that is the point (CI runners are disposable; the release gate
# doubles as the owner's own upgrade to the build being shipped).
param(
    # The built installer to update to (e.g. dist\SageThumbs2K-Setup-<ver>.exe).
    [Parameter(Mandatory)][string]$Setup,
    # The freshly built app exe that performs the update. Defaults to the x64 release build.
    [string]$App,
    [int]$TimeoutSec = 300,
    [switch]$Hold
)
$ErrorActionPreference = 'Stop'
if (-not $App) {
    $App = Join-Path (Join-Path (& "$PSScriptRoot\_targetdir.ps1") 'release') 'SageThumbs2K.exe'
}
$Setup = (Resolve-Path -LiteralPath $Setup).Path
if (-not (Test-Path -LiteralPath $App -PathType Leaf)) { throw "App exe not found: $App" }

$installDir = Join-Path $env:ProgramFiles 'SageThumbs2K'
$installedExe = Join-Path $installDir 'SageThumbs2K.exe'
$installedDll = Join-Path $installDir 'sagethumbs2k.dll'

# First three version components only: Windows stores four and Inno writes X.Y.Z.0.
function Get-Ver3([string]$path) {
    $v = (Get-Item -LiteralPath $path).VersionInfo
    '{0}.{1}.{2}' -f $v.FileMajorPart, $v.FileMinorPart, $v.FileBuildPart
}
$expected = Get-Ver3 $App

if (-not (Test-Path -LiteralPath $installedExe -PathType Leaf)) {
    Write-Host "  [self-update] baseline: fresh silent install of $(Split-Path -Leaf $Setup)"
    $p = Start-Process -FilePath $Setup -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -Wait -PassThru
    if ($p.ExitCode) { throw "Baseline install failed (setup exit $($p.ExitCode))." }
    if (-not (Test-Path -LiteralPath $installedExe -PathType Leaf)) {
        throw "Baseline install finished but $installedExe does not exist."
    }
}

Write-Host "  [self-update] installed: $(Get-Ver3 $installedExe) -> expecting $expected via the app's own updater"

# The app's updater hands setup this log path (update/attempt.rs); its tail is what a failure
# below prints, so a red run says what setup itself saw.
$setupLog = Join-Path $env:LOCALAPPDATA 'SageThumbs2K-setup.log'
function Get-SetupLogTail {
    if (Test-Path -LiteralPath $setupLog) { (Get-Content -LiteralPath $setupLog -Tail 40) -join "`n" } else { '(no setup log)' }
}

$holdTask = "st2k-selfupdate-hold-$PID"
$holdDir = Join-Path ([IO.Path]::GetTempPath()) $holdTask
$holdScript = Join-Path $holdDir 'hold.ps1'
function Get-Holders {
    @(Get-CimInstance Win32_Process -Filter "Name='powershell.exe'" |
        Where-Object { $_.CommandLine -and $_.CommandLine.Contains($holdScript) })
}
function Stop-Holders {
    schtasks.exe /End /TN $holdTask 2>$null | Out-Null
    schtasks.exe /Delete /TN $holdTask /F 2>$null | Out-Null
    # Only the holders THIS script started: matched by its own per-run script path.
    Get-Holders | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    Remove-Item -LiteralPath $holdDir -Recurse -Force -ErrorAction SilentlyContinue
}
if ($Hold) {
    $firstPresent = { param($names) $names | ForEach-Object { Join-Path $installDir $_ } |
        Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1 }
    $mapped2 = & $firstPresent @('CORE_RL_MagickCore_.dll', 'st2k_dlghook.dll', 'st2k.exe')
    $pinned = & $firstPresent @('modules\coders\IM_MOD_RL_png_.dll', 'st2k.exe')
    New-Item -ItemType Directory -Force -Path $holdDir | Out-Null
    $ready = Join-Path $holdDir 'ready.txt'
    # The paths ride IN the script, not on its command line: schtasks refuses a /TR over 261
    # characters, and four quoted install paths are well past that (the first CI run of -Hold
    # died on exactly this).
    $lit = { param($s) "'" + $s.Replace("'", "''") + "'" }
    $paths = "`$Mapped1 = $(& $lit $installedDll)`n`$Mapped2 = $(& $lit $mapped2)`n" +
        "`$Pinned = $(& $lit $pinned)`n`$Ready = $(& $lit $ready)`n"
    # LOAD_LIBRARY_AS_IMAGE_RESOURCE maps each file as an IMAGE (what blocks an overwrite, as a
    # real load does) without running any of its code in a SYSTEM process.
    Set-Content -LiteralPath $holdScript -Encoding utf8 -Value ($paths + @'
Add-Type -Namespace St2kHold -Name K -MemberDefinition '[DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] public static extern IntPtr LoadLibraryExW(string p, IntPtr h, uint f);'
$a = [St2kHold.K]::LoadLibraryExW($Mapped1, [IntPtr]::Zero, 0x20)
$b = [St2kHold.K]::LoadLibraryExW($Mapped2, [IntPtr]::Zero, 0x20)
$fs = [System.IO.File]::Open($Pinned, 'Open', 'Read', 'Read')
"mapped=$($a -ne [IntPtr]::Zero),$($b -ne [IntPtr]::Zero) pinned=$($fs.Length)" | Set-Content -LiteralPath $Ready
Start-Sleep -Seconds 900
'@)
    $tr = "powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$holdScript`""
    $made = schtasks.exe /Create /TN $holdTask /RU SYSTEM /SC ONCE /ST 23:59 /F /TR $tr 2>&1
    if ($LASTEXITCODE) { throw "-Hold could not create the SYSTEM holder task (it needs an elevated shell): $made" }
    schtasks.exe /Run /TN $holdTask | Out-Null
    $until = (Get-Date).AddSeconds(30)
    while (-not (Test-Path -LiteralPath $ready) -and (Get-Date) -lt $until) { Start-Sleep -Milliseconds 500 }
    if (-not (Test-Path -LiteralPath $ready)) { Stop-Holders; throw 'The SYSTEM holder never took hold of the install files.' }
    $readyText = (Get-Content -LiteralPath $ready -Raw).Trim()
    if ($readyText -notmatch 'mapped=True,True') { Stop-Holders; throw "The SYSTEM holder could not map both files: $readyText" }
    Write-Host "  [self-update] -Hold: SYSTEM maps $(Split-Path -Leaf $installedDll) + $(Split-Path -Leaf $mapped2), pins $(Split-Path -Leaf $pinned) without delete sharing ($readyText)"
}

# Completion signal: Inno stamps every installed PAYLOAD file with its SOURCE timestamp,
# so exe/dll mtimes never change on a same-version reinstall - which is exactly what CI
# does (baseline and upgrade are the same built setup), and exactly how this check failed
# on its first CI run. unins000.dat is different: Inno REWRITES the uninstall log with the
# wall clock at the end of every install, same-version included, so "its mtime moved past
# the moment we launched the updater" is the honest "the install ran to completion" signal.
$uninsDat = Join-Path $installDir 'unins000.dat'
$t0 = (Get-Date).ToUniversalTime()

# The app-side pipeline. It exits as soon as the ELEVATED INSTALLER PROCESS is running
# (mirroring the production caller, which exits so the installer can replace it), so a zero
# exit here means verify + lock + launch all succeeded - the half that was broken for
# twenty releases. The polling below proves the other half.
$orphan = $null
try {
    if ($Hold) {
        $orphan = 0..20 | ForEach-Object { "$installedDll.old$_" } |
            Where-Object { -not (Test-Path -LiteralPath $_) } | Select-Object -First 1
        Rename-Item -LiteralPath $installedDll -NewName (Split-Path -Leaf $orphan)
        Write-Host "  [self-update] -Hold: left the shell extension at $(Split-Path -Leaf $orphan), its own name empty (a failed 3.6.0 update's leftovers)"
    }
    $p = Start-Process -FilePath $App -ArgumentList '--update-selftest', "`"$Setup`"" -Wait -PassThru
    if ($p.ExitCode) {
        throw "--update-selftest exited $($p.ExitCode): the updater could not verify, lock, or LAUNCH the installer. See %LOCALAPPDATA%\SageThumbs2K.log (update-selftest lines)."
    }
    Write-Host "  [self-update] elevated installer launched; waiting for the upgrade to land..."

    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    $landed = $false
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Seconds 3
        try {
            $unins = Get-Item -LiteralPath $uninsDat -ErrorAction Stop
            if ($unins.LastWriteTimeUtc -gt $t0 -and (Get-Ver3 $installedExe) -eq $expected) {
                $landed = $true
                break
            }
        } catch {
            # Mid-replace the files can be transiently missing/locked; keep polling.
        }
    }
    if (-not $landed) {
        throw "Self-update did NOT land within ${TimeoutSec}s: $installedExe is $(Get-Ver3 $installedExe) (expected $expected), uninstall log fresh: $((Test-Path $uninsDat) -and (Get-Item $uninsDat).LastWriteTimeUtc -gt $t0). The launch succeeded, so the installer itself failed or stalled. Setup's log ($setupLog) ends:`n$(Get-SetupLogTail)"
    }

    # The DLL must land too - a "successful" upgrade that left the shell extension stale is the
    # 2026-08-02 "still on the old version" bug shape. Name the likely holder in the failure.
    if (-not (Test-Path -LiteralPath $installedDll -PathType Leaf)) {
        throw "The update finished but $installedDll is MISSING: thumbnails are off on this machine. Setup's log ends:`n$(Get-SetupLogTail)"
    }
    if ((Get-Ver3 $installedDll) -ne $expected) {
        $holders = (tasklist /m sagethumbs2k.dll 2>$null | Out-String).Trim()
        throw "Installed exe updated but $installedDll is still $(Get-Ver3 $installedDll) (expected $expected) - a process is holding the old DLL mapped. tasklist /m says:`n$holders"
    }

    if ($Hold) {
        if (-not (Get-Holders)) {
            throw "The upgrade landed by KILLING the SYSTEM holder: setup must park held files, never close their holders. Setup's log ends:`n$(Get-SetupLogTail)"
        }
        Write-Host '  [self-update] -Hold: upgrade landed with every holder still running' -ForegroundColor Green
    }
} finally {
    if ($Hold) { Stop-Holders }
    # Whatever happened above, never leave this machine without its shell extension.
    if ($orphan -and -not (Test-Path -LiteralPath $installedDll) -and (Test-Path -LiteralPath $orphan)) {
        Move-Item -LiteralPath $orphan -Destination $installedDll
        Write-Host "  [self-update] restored $installedDll from $(Split-Path -Leaf $orphan)" -ForegroundColor Yellow
    }
}

Write-Host "  [self-update] PASS - installed exe + dll are $expected, upgraded in place by the app's own updater." -ForegroundColor Green
exit 0
