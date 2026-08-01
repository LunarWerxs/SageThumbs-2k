<#
  Permanent machine-wide install of SageThumbs 2K. Run from an ELEVATED prompt
  (writes to Program Files + HKLM):

      .\scripts\install.ps1
      .\scripts\install.ps1 -Architecture arm64
      .\scripts\install.ps1 -Uninstall

  Copies the DLL, stub EXE, manifest and assets into Program Files (self-contained,
  no dependency on the build tree), then:
    - regsvr32 registers the thumbnail provider + classic context-menu handler (HKLM)
    - Add-AppxPackage registers the sparse package for the modern Win11 context menu
#>
[CmdletBinding()]
param(
    [switch]$Uninstall,
    [ValidateSet('x64', 'arm64')]
    [string]$Architecture = 'x64',
    [string]$BuildDir,
    # Read-only guard for CI/local script tests: validates the chosen artifacts
    # and exits before Program Files, registry, package, or Explorer changes.
    [switch]$ValidateOnly
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$pkg = 'SageThumbs2K'

function Get-ArchitectureSpec([string]$SelectedArchitecture) {
    switch ($SelectedArchitecture) {
        'x64' {
            # Preserve the original developer build location and installed path.
            return [pscustomobject]@{
                Name = 'x64'; RustTarget = 'x86_64-pc-windows-msvc';
                BuildSubdirectory = 'release'; InstallDirectoryName = 'SageThumbs2K';
                PeMachine = [uint16]0x8664
            }
        }
        'arm64' {
            return [pscustomobject]@{
                Name = 'arm64'; RustTarget = 'aarch64-pc-windows-msvc';
                BuildSubdirectory = 'aarch64-pc-windows-msvc\release'; InstallDirectoryName = 'SageThumbs2K-arm64';
                PeMachine = [uint16]0xaa64
            }
        }
        default { throw "Unsupported architecture: $SelectedArchitecture" }
    }
}

function Get-PeMachine([string]$Path) {
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        if ($stream.Length -lt 0x40) { throw "Not a PE file (too short): $Path" }
        $head = [byte[]]::new(0x40)
        if ($stream.Read($head, 0, $head.Length) -ne $head.Length -or $head[0] -ne 0x4d -or $head[1] -ne 0x5a) {
            throw "Not a PE file: $Path"
        }
        $peOffset = [BitConverter]::ToInt32($head, 0x3c)
        if ($peOffset -lt 0 -or $peOffset + 6 -gt $stream.Length) { throw "Invalid PE header offset: $Path" }
        $null = $stream.Seek($peOffset, [IO.SeekOrigin]::Begin)
        $coff = [byte[]]::new(6)
        if ($stream.Read($coff, 0, $coff.Length) -ne $coff.Length -or
            $coff[0] -ne 0x50 -or $coff[1] -ne 0x45 -or $coff[2] -ne 0 -or $coff[3] -ne 0) {
            throw "Invalid PE signature: $Path"
        }
        return [BitConverter]::ToUInt16($coff, 4)
    } finally {
        $stream.Dispose()
    }
}

function Assert-PeArchitecture([string]$Path, [pscustomobject]$Spec) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "Missing $Path" }
    $machine = Get-PeMachine $Path
    if ($machine -ne $Spec.PeMachine) {
        throw ('Architecture mismatch: expected {0} ({1}), but {2} is PE machine 0x{3:X4}' -f
            $Spec.Name, $Spec.RustTarget, $Path, $machine)
    }
}

function Assert-NativeArm64Host([pscustomobject]$Spec) {
    if ($Spec.Name -ne 'arm64') { return }
    if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne
        [System.Runtime.InteropServices.Architecture]::Arm64) {
        throw 'ARM64 install requires a native ARM64 Windows host; use -ValidateOnly for cross-host artifact checks.'
    }
}

function Invoke-Regsvr32([string]$DllPath, [switch]$Unregister) {
    # regsvr32 is a GUI-subsystem executable. Direct `& regsvr32.exe ...`
    # invocation can leave $LASTEXITCODE unset even though registration ran,
    # so wait for the process explicitly and return its real exit code.
    $arguments = @('/s')
    if ($Unregister) { $arguments += '/u' }
    $arguments += "`"$DllPath`""
    $registrar = Join-Path $env:SystemRoot 'System32\regsvr32.exe'
    $process = Start-Process -FilePath $registrar -ArgumentList $arguments `
        -WindowStyle Hidden -Wait -PassThru
    return $process.ExitCode
}

$spec = Get-ArchitectureSpec $Architecture
if (-not $BuildDir) {
    $BuildDir = Join-Path (& "$PSScriptRoot\_targetdir.ps1") $spec.BuildSubdirectory
}
$prog = Join-Path $env:ProgramFiles $spec.InstallDirectoryName
$shortcut = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'SageThumbs 2K.lnk'

if ($ValidateOnly) {
    foreach ($artifact in @('sagethumbs2k.dll', 'SageThumbs2K.exe', 'st2k.exe')) {
        Assert-PeArchitecture (Join-Path $BuildDir $artifact) $spec
    }
    [pscustomobject]@{ Architecture = $spec.Name; RustTarget = $spec.RustTarget; BuildDir = $BuildDir }
    return
}

# An ARM64 COM server cannot be registered into x64 Windows. Reject before any
# Program Files, registry, package, or Explorer mutation; -ValidateOnly above
# intentionally remains usable from an x64 cross-build host.
Assert-NativeArm64Host $spec

if ($Uninstall) {
    if (Test-Path "$prog\sagethumbs2k.dll") {
        $registrationExit = Invoke-Regsvr32 "$prog\sagethumbs2k.dll" -Unregister
        if ($registrationExit -ne 0) { throw "regsvr32 unregister failed ($registrationExit): $prog\sagethumbs2k.dll" }
    }
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
foreach ($artifact in @('sagethumbs2k.dll', 'SageThumbs2K.exe', 'st2k.exe')) {
    Assert-PeArchitecture (Join-Path $BuildDir $artifact) $spec
}
Copy-Item "$BuildDir\sagethumbs2k.dll" $prog -Force
# The bin target is `SageThumbs2K`, so it builds as `SageThumbs2K.exe` directly.
Copy-Item "$BuildDir\SageThumbs2K.exe" $prog -Force
# The CLI / MCP server (`st2k --mcp`). The dist installer ships it; the dev
# install used to omit it, leaving a live CLI check running stale code.
Copy-Item "$BuildDir\st2k.exe" $prog -Force
Copy-Item "$root\packaging\AppxManifest.xml" $prog -Force
Copy-Item "$root\packaging\Assets" $prog -Recurse -Force
# The legacy x64 loose package stays neutral for update compatibility. ARM64
# Explorer can load only an ARM64 in-process extension, so its dev manifest must
# declare arm64. Patch the copied manifest, never the tracked template.
if ($spec.Name -eq 'arm64') {
    $installedManifest = Join-Path $prog 'AppxManifest.xml'
    $manifestText = Get-Content -LiteralPath $installedManifest -Raw
    $manifestText = $manifestText -replace '(<Identity\b[^>]*\bProcessorArchitecture=")[^"]+("[^>]*>)', '${1}arm64${2}'
    if ($manifestText -notmatch '<Identity\b[^>]*\bProcessorArchitecture="arm64"') {
        throw "Could not set ARM64 package identity in $installedManifest"
    }
    Set-Content -LiteralPath $installedManifest -Value $manifestText -Encoding utf8
}
# Our hardened ImageMagick policy.xml next to the binaries. decode.rs points
# MAGICK_CONFIGURE_PATH here, so the policy applies even when we fall back to a
# system-installed magick (this dev/compact install bundles none of its own).
Copy-Item "$root\packaging\imagemagick-policy.xml" "$prog\policy.xml" -Force

# Thumbnails + classic context menu (machine-wide, HKLM). This classic IContextMenu
# handler serves the full owner-drawn preview + "SageThumbs 2K" submenu with Settings
# in "Show more options" (and the whole right-click menu on classic-menu machines).
$registrationExit = Invoke-Regsvr32 "$prog\sagethumbs2k.dll"
if ($registrationExit -ne 0) { throw "regsvr32 registration failed ($registrationExit): $prog\sagethumbs2k.dll" }
# Modern Win11 menu: register the sparse package (Dev Mode, UNSIGNED loose -Register —
# the signed installer.iss path uses the packed .msix instead) so the packaged QUICK
# verbs (Convert into / Convert… / Resize / Rotate) appear on the compact Win11 menu.
# NOTE: on a classic-menu-default machine those packaged verbs DON'T appear (packaged verbs
# live only in the modern compact flyout) — the classic handler shows its OWN quick verbs
# unconditionally now. (See packaging\AppxManifest.xml.)
Get-AppxPackage $pkg | Remove-AppxPackage -ErrorAction SilentlyContinue
Add-AppxPackage -Register "$prog\AppxManifest.xml" -ExternalLocation $prog -ForceUpdateFromAnyVersion
# Clean up the now-inert ModernMenuActive marker (<= 1.3.0 wrote it; nothing reads it since the
# classic handler stopped suppressing its quick verbs). Delete tolerantly so a dev box left with
# the old value doesn't keep it; a missing value is a harmless no-op.
try { & reg.exe delete 'HKLM\SOFTWARE\SageThumbs2K' /v ModernMenuActive /f 2>$null | Out-Null } catch {}

# Flag this box as a DEVELOPER test machine (HKCU DevMachine=1 → settings::is_dev_machine).
# ONLY the owner ever runs this dev-install script (real users get the Inno installer), so this
# marks the machine whose build/install/test churn should be tagged with &dev=1 on requests.
# Idempotent; stays set across a dev -Uninstall.
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
