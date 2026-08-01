<#
  test-sandbox.ps1 — one-command CLEAN-ROOM test of the installer in Windows Sandbox.

      pwsh scripts\vm\test-sandbox.ps1                 # current native-arch installer
      pwsh scripts\vm\test-sandbox.ps1 -Installer <path\to\Setup.exe>

  Windows Sandbox is a disposable, throwaway Windows instance (a feature of Win10/11
  Pro). It boots in seconds, shares nothing with your real machine, and is wiped the
  moment you close it. This is the fast way to answer "does the installer work on a
  machine that has never seen SageThumbs, with default Defender on?" — a genuine
  first-run / SmartScreen / AV smoke test, no VM image to manage.

  IMPORTANT LIMITATION: Windows Sandbox is built from the HOST OS, so on this Win11
  box the Sandbox is ALSO Win11. It does NOT reproduce Windows 10-specific behavior
  (the modern-menu-on-Win10 or N/KN-edition class of bugs). For a real Windows 10
  target, use new-win10-vm.ps1 (Hyper-V + a Win10 ISO). See README.md.

  What this sets up inside the Sandbox:
   * the folder holding the installer, mapped READ-ONLY to the Sandbox desktop
   * the test-corpus (sample files to right-click and eyeball), mapped read-only
   * Explorer opened on the installer so you can run it and immediately test
#>
param([string]$Installer)

$ErrorActionPreference = 'Stop'
if (-not (Test-Path "$env:WINDIR\System32\WindowsSandbox.exe")) {
    Write-Host "Windows Sandbox is not installed. Enable it (elevated):" -ForegroundColor Red
    Write-Host "  Enable-WindowsOptionalFeature -Online -FeatureName 'Containers-DisposableClientVM' -All"
    exit 1
}

$root   = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent   # project root
$dist   = Join-Path $root 'dist'
$corpus = Join-Path (Split-Path $root -Parent) 'test-corpus'
function Get-NativeReleaseArchitecture {
    switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
        'X64' { return 'x64' }
        'Arm64' { return 'arm64' }
        default { throw "Windows Sandbox host architecture is unsupported: $([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture)" }
    }
}
$ver = ([regex]::Match((Get-Content -LiteralPath (Join-Path $root 'Cargo.toml') -Raw), '(?m)^\s*version\s*=\s*"([^"]+)"')).Groups[1].Value
if (-not $ver) { throw 'could not read version from Cargo.toml' }
$arch = Get-NativeReleaseArchitecture
$expectedInstallerName = if ($arch -eq 'arm64') {
    "SageThumbs2K-Setup-$ver-arm64.exe"
} else {
    "SageThumbs2K-Setup-$ver.exe"
}

if (-not $Installer) {
    $Installer = Join-Path $dist $expectedInstallerName
}
if (-not $Installer -or -not (Test-Path -LiteralPath $Installer -PathType Leaf)) {
    Write-Host "Missing expected $arch installer: $expectedInstallerName. Run scripts\build-release.ps1 -Architecture $arch first." -ForegroundColor Red
    exit 1
}
$Installer = (Get-Item -LiteralPath $Installer -ErrorAction Stop).FullName
$installerName = Split-Path -Path $Installer -Leaf
if ($installerName -cne $expectedInstallerName) {
    throw "installer '$installerName' does not match native $arch artifact '$expectedInstallerName'"
}

# Map an isolated one-file directory. dist\ may hold two architectures and old
# versions, none of which should be selectable during this test.
$installerMapRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("st2k-sandbox-installer-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force $installerMapRoot | Out-Null
Copy-Item -LiteralPath $Installer -Destination (Join-Path $installerMapRoot $installerName) -Force
$maps = @("    <MappedFolder><HostFolder>$installerMapRoot</HostFolder><SandboxFolder>C:\Users\WDAGUtilityAccount\Desktop\installer</SandboxFolder><ReadOnly>true</ReadOnly></MappedFolder>")
if (Test-Path $corpus) {
    $maps += "    <MappedFolder><HostFolder>$corpus</HostFolder><SandboxFolder>C:\Users\WDAGUtilityAccount\Desktop\test-corpus</SandboxFolder><ReadOnly>true</ReadOnly></MappedFolder>"
}
$mapsXml = $maps -join "`r`n"

# On logon: open Explorer on the installer AND on the corpus so you can install then
# immediately right-click a .psd/.cbz/.zip to see thumbnails. A note explains the flow.
$note = "SageThumbs 2K clean-room test:`r`n" +
        "1. Run installer\$installerName (accept the SmartScreen prompt - that IS the reputation test).`r`n" +
        "2. Open the test-corpus folder, switch to Large icons, look for thumbnails.`r`n" +
        "3. Try: C:\Program Files\SageThumbs2K\st2k.exe doctor  (in a Command Prompt).`r`n" +
        "Close the Sandbox to discard everything."

$wsb = @"
<Configuration>
  <MappedFolders>
$mapsXml
  </MappedFolders>
  <LogonCommand>
    <Command>cmd.exe /c "echo $note> C:\Users\WDAGUtilityAccount\Desktop\READ-ME.txt &amp; start explorer C:\Users\WDAGUtilityAccount\Desktop\installer &amp; start notepad C:\Users\WDAGUtilityAccount\Desktop\READ-ME.txt"</Command>
  </LogonCommand>
  <MemoryInMB>8192</MemoryInMB>
  <Networking>Enable</Networking>
</Configuration>
"@

$out = Join-Path $installerMapRoot 'sandbox.wsb'
$wsb | Set-Content $out -Encoding UTF8
Write-Host "Launching Windows Sandbox with $installerName ..." -ForegroundColor Cyan
Write-Host "(clean Win11 instance; everything is discarded on close)"
try {
    $sandbox = Start-Process "$env:WINDIR\System32\WindowsSandbox.exe" -ArgumentList $out -PassThru
    # The isolated map must remain available while the guest is open. Waiting here
    # lets us remove only this GUID-owned directory after the user closes Sandbox.
    Wait-Process -Id $sandbox.Id
} finally {
    Remove-Item -LiteralPath $installerMapRoot -Recurse -Force -ErrorAction SilentlyContinue
}
