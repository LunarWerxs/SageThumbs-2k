<#
  Build the SIGNED sparse MSIX that gives SageThumbs 2K package identity, so the
  Windows 11 *modern* context menu (IExplorerCommand) installs for normal users —
  no Developer Mode required.

  Signing uses a self-signed code-signing cert (CN=SageThumbs2K). That is FREE and
  needs no Microsoft / CA involvement: the installer trusts the matching public
  cert (machine TrustedPeople store — app packages only, NOT a root CA), then
  sideloads the signed package. The private key never leaves this machine's cert
  store; only the public .cer ships.

  Outputs into -OutDir (default: packaging\stage, where build-release.ps1 stages):
    SageThumbs2K.msix   the signed sparse package (manifest + assets; the DLL/EXE
                        live at the external location passed at install time)
    SageThumbs2K.cer    the public cert the installer adds to TrustedPeople

  The sparse package payload is JUST the manifest + Assets; the actual binaries
  stay unpackaged in {app} and are bound via -ExternalLocation at registration.
#>
[CmdletBinding()]
param(
    [string]$OutDir  = "$PSScriptRoot\stage",
    [string]$Subject = "CN=SageThumbs2K"
)
$ErrorActionPreference = 'Stop'
$pkgdir = $PSScriptRoot   # ...\packaging

# 1) Locate the Windows SDK tools (latest installed bin\<ver>\x64). ----------
$sdk = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Directory -ErrorAction SilentlyContinue |
    Where-Object { Test-Path "$($_.FullName)\x64\makeappx.exe" } |
    Sort-Object Name -Descending | Select-Object -First 1
if (-not $sdk) {
    throw "Windows SDK not found (makeappx.exe / signtool.exe). Install the Windows 10/11 SDK, or build with -NoModernMenu."
}
$makeappx = "$($sdk.FullName)\x64\makeappx.exe"
$signtool = "$($sdk.FullName)\x64\signtool.exe"
Write-Host "      SDK: $($sdk.Name)" -ForegroundColor DarkGray

# 2) Ensure a self-signed code-signing cert (10-year) in CurrentUser\My. ------
#    Reused across builds so the publisher (and thus update trust) stays stable.
$cert = Get-ChildItem Cert:\CurrentUser\My |
    Where-Object { $_.Subject -eq $Subject -and $_.HasPrivateKey -and $_.NotAfter -gt (Get-Date) } |
    Select-Object -First 1
if (-not $cert) {
    Write-Host "      generating self-signed code-signing cert ($Subject)" -ForegroundColor DarkGray
    $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject $Subject `
        -KeyUsage DigitalSignature -FriendlyName "SageThumbs2K self-signed (sparse package)" `
        -CertStoreLocation Cert:\CurrentUser\My -NotAfter (Get-Date).AddYears(10) `
        -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3")
}

New-Item -ItemType Directory $OutDir -Force | Out-Null
$cer  = Join-Path $OutDir "SageThumbs2K.cer"
$msix = Join-Path $OutDir "SageThumbs2K.msix"
Export-Certificate -Cert $cert -FilePath $cer -Force | Out-Null

# 3) Stage the package payload (manifest + assets only) and pack. -------------
$stage = Join-Path ([System.IO.Path]::GetTempPath()) "st2k_msix_stage"
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory $stage -Force | Out-Null
Copy-Item (Join-Path $pkgdir 'AppxManifest.xml') $stage -Force
Copy-Item (Join-Path $pkgdir 'Assets') $stage -Recurse -Force

# Patch the STAGED manifest's Identity Version from Cargo.toml (the single source of
# truth) so a release can never ship a sparse package still carrying the previous
# version — this was a manual, forgettable bump before. The checked-in manifest is
# left untouched; only the packed copy is rewritten.
$cargoVer = ([regex]::Match((Get-Content (Join-Path $pkgdir '..\Cargo.toml') -Raw), '(?m)^\s*version\s*=\s*"([^"]+)"')).Groups[1].Value
if ($cargoVer) {
    $mf = Join-Path $stage 'AppxManifest.xml'
    (Get-Content $mf -Raw) -replace '(<Identity\b[^>]*\bVersion=")[^"]+(")', "`${1}$cargoVer.0`${2}" |
        Set-Content $mf -Encoding utf8
    Write-Host "      manifest Identity Version -> $cargoVer.0 (from Cargo.toml)" -ForegroundColor DarkGray
}

& $makeappx pack /d $stage /p $msix /o /nv
if ($LASTEXITCODE) { throw "makeappx pack failed ($LASTEXITCODE)" }

# 4) Sign with the cert (matched by thumbprint, so it's unambiguous). ---------
& $signtool sign /fd SHA256 /sha1 $cert.Thumbprint $msix
if ($LASTEXITCODE) { throw "signtool sign failed ($LASTEXITCODE)" }

Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
Write-Host ("      signed sparse package: {0} ({1} bytes) + {2}" -f (Split-Path $msix -Leaf), (Get-Item $msix).Length, (Split-Path $cer -Leaf)) -ForegroundColor DarkGray
