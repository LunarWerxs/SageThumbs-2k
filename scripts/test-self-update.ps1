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
# Elevation: the launched setup elevates via the `runas` verb. On GitHub-hosted runners and
# on dev boxes with silent admin consent this shows no prompt. It INSTALLS/UPGRADES the
# machine it runs on - that is the point (CI runners are disposable; the release gate
# doubles as the owner's own upgrade to the build being shipped).
param(
    # The built installer to update to (e.g. dist\SageThumbs2K-Setup-<ver>.exe).
    [Parameter(Mandatory)][string]$Setup,
    # The freshly built app exe that performs the update. Defaults to the x64 release build.
    [string]$App,
    [int]$TimeoutSec = 300
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

$beforeExe = (Get-Item -LiteralPath $installedExe).LastWriteTimeUtc
Write-Host "  [self-update] installed: $(Get-Ver3 $installedExe) -> expecting $expected via the app's own updater"

# The app-side pipeline. It exits as soon as the ELEVATED INSTALLER PROCESS is running
# (mirroring the production caller, which exits so the installer can replace it), so a zero
# exit here means verify + lock + launch all succeeded - the half that was broken for
# twenty releases. The polling below proves the other half.
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
        $item = Get-Item -LiteralPath $installedExe -ErrorAction Stop
        if ($item.LastWriteTimeUtc -gt $beforeExe -and (Get-Ver3 $installedExe) -eq $expected) {
            $landed = $true
            break
        }
    } catch {
        # Mid-replace the file can be transiently missing/locked; keep polling.
    }
}
if (-not $landed) {
    throw "Self-update did NOT land within ${TimeoutSec}s: $installedExe is $(Get-Ver3 $installedExe) (expected $expected). The launch succeeded, so the installer itself failed or stalled - check %TEMP% Inno logs and whether something held the install dir open."
}

# The DLL must land too - a "successful" upgrade that left the shell extension stale is the
# 2026-08-02 "still on the old version" bug shape. Name the likely holder in the failure.
if ((Get-Ver3 $installedDll) -ne $expected) {
    $holders = (tasklist /m sagethumbs2k.dll 2>$null | Out-String).Trim()
    throw "Installed exe updated but $installedDll is still $(Get-Ver3 $installedDll) (expected $expected) - a process is holding the old DLL mapped. tasklist /m says:`n$holders"
}

Write-Host "  [self-update] PASS - installed exe + dll are $expected, upgraded in place by the app's own updater." -ForegroundColor Green
exit 0
