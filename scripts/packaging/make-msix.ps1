<#
  Build the SIGNED sparse MSIX that gives SageThumbs 2K package identity, so the
  Windows 11 *modern* context menu (IExplorerCommand) installs for normal users —
  no Developer Mode required.

  Two signing modes:

  * -AzureSign (the RELEASE path, since 2026-09-09): the package is signed with the real
    publisher certificate through sign-release.ps1 (Azure Artifact Signing, ST2K_SIGN_* +
    the AZURE_* triple in the environment). An MSIX is only valid when its manifest
    Publisher EQUALS the signing certificate's subject, character for character, so the
    staged manifest is patched to -Subject, which the caller sets to that subject. NO .cer
    is emitted, and a stale one in -OutDir is removed: the chain is already trusted on every
    PC, so the installer has nothing to add to a certificate store any more.

  * default (development, tests, a machine without the account): a self-signed
    code-signing cert (CN=SageThumbs2K), FREE and needing no CA. The installer trusts the
    matching public cert (machine TrustedPeople store — app packages only, NOT a root CA),
    then sideloads the signed package. The private key never leaves this machine's cert
    store; only the public .cer ships.

  Outputs into -OutDir (default: scripts\packaging\stage, where build-release.ps1 stages):
    SageThumbs2K.msix   the signed sparse package (manifest + assets; the DLL/EXE
                        live at the external location passed at install time)
    SageThumbs2K.cer    self-signed mode only: the public cert the installer adds to
                        TrustedPeople

  The sparse package payload is JUST the manifest + Assets; the actual binaries
  stay unpackaged in {app} and are bound via -ExternalLocation at registration.
#>
[CmdletBinding()]
param(
    [string]$OutDir  = "$PSScriptRoot\stage",
    # Self-signed mode: the subject of the certificate to mint/reuse. -AzureSign mode: the
    # EXACT subject of the Azure certificate, which becomes the package's Publisher.
    [string]$Subject = "CN=SageThumbs2K",

    # The x64 release has always used a neutral sparse package. Keep that
    # identity for seamless updates; ARM64 needs an ARM64 identity because its
    # in-process shell extension cannot load in an x64 Explorer process.
    [ValidateSet('x64', 'arm64')]
    [string]$Architecture = 'x64',

    [switch]$AzureSign
)
$ErrorActionPreference = 'Stop'
$pkgdir = $PSScriptRoot   # ...\scripts\packaging

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

New-Item -ItemType Directory $OutDir -Force | Out-Null
$cer  = Join-Path $OutDir "SageThumbs2K.cer"
$msix = Join-Path $OutDir "SageThumbs2K.msix"

$cert = $null
if ($AzureSign) {
    if ($Subject -ceq 'CN=SageThumbs2K') {
        throw "-AzureSign needs -Subject set to the Azure certificate's subject; the self-signed default cannot be the Publisher of a chain-signed package"
    }
    # A leftover .cer from an earlier self-signed build would ride into the installer and be
    # imported into TrustedPeople for nothing. check-release-manifest.ps1 refuses that stage.
    Remove-Item -LiteralPath $cer -Force -ErrorAction SilentlyContinue
    Write-Host "      signing through Azure Artifact Signing as $Subject" -ForegroundColor DarkGray
} else {
    # 2) Ensure a self-signed code-signing cert (10-year) in CurrentUser\My. ------
    #    Reused across builds so the publisher (and thus update trust) stays stable. Among
    #    several candidates, one the machine already trusts (present in LocalMachine
    #    TrustedPeople, which the installer populates) comes first: the package verification
    #    then needs no temporary trust entry, which only an elevated shell can add.
    $trustedThumbprints = @(
        Get-ChildItem Cert:\LocalMachine\TrustedPeople -ErrorAction SilentlyContinue |
            ForEach-Object { $_.Thumbprint }
    )
    $cert = Get-ChildItem Cert:\CurrentUser\My |
        Where-Object { $_.Subject -eq $Subject -and $_.HasPrivateKey -and $_.NotAfter -gt (Get-Date) } |
        Sort-Object -Property @{ Expression = { $trustedThumbprints -contains $_.Thumbprint }; Descending = $true } |
        Select-Object -First 1
    if (-not $cert) {
        Write-Host "      generating self-signed code-signing cert ($Subject)" -ForegroundColor DarkGray
        $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject $Subject `
            -KeyUsage DigitalSignature -FriendlyName "SageThumbs2K self-signed (sparse package)" `
            -CertStoreLocation Cert:\CurrentUser\My -NotAfter (Get-Date).AddYears(10) `
            -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3")
    }
    Export-Certificate -Cert $cert -FilePath $cer -Force | Out-Null
}

# 3) Stage the package payload (manifest + assets only) and pack. -------------
# Process-ID-scoped, not a fixed "st2k_msix_stage" name: two concurrent make-msix.ps1 calls
# (e.g. the x64 and ARM64 test packages built back-to-back by test-msix-integrity.ps1, or two
# concurrent -Lint runs) would otherwise delete and overwrite the same staging directory out
# from under each other mid-build.
$stage = Join-Path ([System.IO.Path]::GetTempPath()) "st2k_msix_stage_$PID"
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory $stage -Force | Out-Null
Copy-Item (Join-Path $pkgdir 'AppxManifest.xml') $stage -Force
Copy-Item (Join-Path $pkgdir 'Assets') $stage -Recurse -Force

# Patch the STAGED manifest's Identity metadata from Cargo.toml, the requested
# external-binary architecture, and the signing subject. The checked-in manifest is a
# neutral dev template (Publisher CN=SageThumbs2K, what the unpackaged dev registration and
# the self-signed test package use); only the packed copy is rewritten.
$cargoVer = ([regex]::Match((Get-Content (Join-Path $pkgdir '..\..\Cargo.toml') -Raw), '(?m)^\s*version\s*=\s*"([^"]+)"')).Groups[1].Value
if ($cargoVer -notmatch '^\d+\.\d+\.\d+$') {
    throw "could not read an MSIX-compatible version from Cargo.toml: '$cargoVer'"
}
$mf = Join-Path $stage 'AppxManifest.xml'
$manifestArchitecture = if ($Architecture -eq 'arm64') { 'arm64' } else { 'neutral' }
$manifestText = Get-Content -LiteralPath $mf -Raw
$manifestText = $manifestText -replace '(<Identity\b[^>]*\bVersion=")[^"]+(")', "`${1}$cargoVer.0`${2}"
$manifestText = $manifestText -replace '(<Identity\b[^>]*\bProcessorArchitecture=")[^"]+(")', "`${1}$manifestArchitecture`${2}"
# The subject is spliced as a LITERAL: a `$` in it would be read as a replacement group.
$manifestText = [regex]::Replace($manifestText, '(<Identity\b[^>]*\bPublisher=")[^"]+(")', { param($m) $m.Groups[1].Value + $Subject + $m.Groups[2].Value })
if ($manifestText -notmatch [regex]::Escape("Version=`"$cargoVer.0`"")) {
    throw 'could not patch staged AppxManifest.xml Identity Version'
}
if ($manifestText -notmatch [regex]::Escape("ProcessorArchitecture=`"$manifestArchitecture`"")) {
    throw 'could not patch staged AppxManifest.xml ProcessorArchitecture'
}
if ($manifestText -notmatch [regex]::Escape("Publisher=`"$Subject`"")) {
    throw 'could not patch staged AppxManifest.xml Identity Publisher'
}
Set-Content -LiteralPath $mf -Value $manifestText -Encoding utf8
Write-Host "      manifest Identity -> $cargoVer.0 / $manifestArchitecture ($Architecture payload) / $Subject" -ForegroundColor DarkGray

& $makeappx pack /d $stage /p $msix /o /nv
if ($LASTEXITCODE) { throw "makeappx pack failed ($LASTEXITCODE)" }

# 4) Sign. -------------------------------------------------------------------
. (Join-Path $pkgdir '..\release-manifest-lib.ps1')
$expectedIdentityArchitecture = if ($Architecture -eq 'arm64') { 'arm64' } else { 'neutral' }
if ($AzureSign) {
    # signtool itself refuses an MSIX whose manifest Publisher differs from the certificate
    # subject (0x8007000B), so a wrong -Subject fails HERE, loudly, never on a user's PC.
    & (Join-Path $pkgdir 'sign-release.ps1') -Path $msix
    if ($LASTEXITCODE) { throw "Azure Artifact Signing of the sparse package failed ($LASTEXITCODE)" }
    Assert-ReleaseMsixPackage `
        -Path $msix `
        -Version $cargoVer `
        -ExpectedProcessorArchitecture $expectedIdentityArchitecture `
        -ExpectedPublisher $Subject `
        -SignToolPath $signtool
    Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host ("      chain-signed sparse package: {0} ({1} bytes), no .cer to trust" -f (Split-Path $msix -Leaf), (Get-Item $msix).Length) -ForegroundColor DarkGray
} else {
    # Matched by thumbprint, so it's unambiguous.
    & $signtool sign /fd SHA256 /sha1 $cert.Thumbprint $msix
    if ($LASTEXITCODE) { throw "signtool sign failed ($LASTEXITCODE)" }
    Assert-ReleaseMsixPackage `
        -Path $msix `
        -CertificatePath $cer `
        -Version $cargoVer `
        -ExpectedProcessorArchitecture $expectedIdentityArchitecture `
        -SignToolPath $signtool
    Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host ("      signed sparse package: {0} ({1} bytes) + {2}" -f (Split-Path $msix -Leaf), (Get-Item $msix).Length, (Split-Path $cer -Leaf)) -ForegroundColor DarkGray
}
