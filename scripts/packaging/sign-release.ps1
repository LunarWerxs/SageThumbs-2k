<#
  Sign release artifacts with Azure Artifact Signing (formerly Trusted Signing), or say
  exactly why nothing was signed. The owner reopened code signing on 2026-09-01 (issue #30,
  docs/RELEASE-SECURITY.md); this is the pipeline half, written so that the day the Azure
  account exists, signing is three environment variables away and no script changes.

  CONFIGURATION - all three are required for a real signature, none is a secret:
    ST2K_SIGN_ENDPOINT   the account's Account URI, e.g. https://eus.codesigning.azure.net/
                         (it MUST be the region the account lives in; a wrong region is a 403)
    ST2K_SIGN_ACCOUNT    the Artifact Signing account name
    ST2K_SIGN_PROFILE    the certificate profile name (Public Trust for a shipped installer)
  Optional:
    ST2K_SIGN_DLIB       path to Azure.CodeSigning.Dlib.dll; otherwise it is looked up in the
                         NuGet cache and under tools\artifact-signing (see Resolve-Dlib).

  AUTHENTICATION is the dlib's business, not this script's: Azure.Identity's
  DefaultAzureCredential takes AZURE_TENANT_ID + AZURE_CLIENT_ID + AZURE_CLIENT_SECRET for a
  service principal holding the "Artifact Signing Certificate Profile Signer" role, or an
  `az login` session on a workstation. This script never reads, prints or forwards any of
  those values - it only needs the three names above.

  USAGE
    pwsh scripts\packaging\sign-release.ps1 -Path a.exe, b.dll      sign in place, then verify
    pwsh scripts\packaging\sign-release.ps1 -Path a.exe -WhatIf     print the exact command, sign nothing
    pwsh scripts\packaging\sign-release.ps1 -Status                 what this machine has (tool, dlib, config)
    pwsh scripts\packaging\sign-release.ps1 -Configured             exit 0 if signing is configured, 2 if not
    pwsh scripts\packaging\sign-release.ps1 -SelfTest               prove signtool + the verify step with the
                                                                    local self-signed cert on a scratch copy
  build-release.ps1 calls this with -AllowUnsigned on the staged binaries, and hands it to
  Inno Setup as the SignTool command (so Setup.exe AND the uninstaller are signed) whenever
  -Configured says yes. Without configuration it prints one yellow line and exits 0, so the
  release flow never assumes a certificate exists.

  EXIT CODES: 0 signed and verified (or nothing configured with -AllowUnsigned, or -WhatIf);
  1 a verdict (a sign or verify failure, a missing tool); 2 -Configured said "no".
#>
[CmdletBinding()]
param(
    [string[]]$Path = @(),
    [switch]$AllowUnsigned,
    [switch]$Status,
    [switch]$Configured,
    [switch]$SelfTest,
    [switch]$WhatIf,
    # Where -SelfTest finds a st2k.exe to copy (defaults to the release target dir).
    [string]$TargetDir
)

$ErrorActionPreference = 'Stop'
$root = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent

function Resolve-SignTool {
    $cmd = Get-Command signtool.exe -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($cmd) { return $cmd.Source }
    $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    if (Test-Path -LiteralPath $kits) {
        $hit = Get-ChildItem -LiteralPath $kits -Directory |
            Where-Object { $_.Name -match '^\d+\.' } |
            Sort-Object { [version]$_.Name } -Descending |
            Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'x64\signtool.exe') } |
            Select-Object -First 1
        if ($hit) { return Join-Path $hit.FullName 'x64\signtool.exe' }
    }
    return $null
}

function Resolve-Dlib {
    if ($env:ST2K_SIGN_DLIB) {
        if (Test-Path -LiteralPath $env:ST2K_SIGN_DLIB -PathType Leaf) { return $env:ST2K_SIGN_DLIB }
        throw "ST2K_SIGN_DLIB points at a file that does not exist: $env:ST2K_SIGN_DLIB"
    }
    $candidates = @()
    # The NuGet cache, newest version first. Microsoft.ArtifactSigning.Client is the current
    # package (the rename of Microsoft.Trusted.Signing.Client); both are searched.
    $nuget = Join-Path $env:USERPROFILE '.nuget\packages'
    foreach ($pkg in 'microsoft.artifactsigning.client', 'microsoft.trusted.signing.client') {
        $dir = Join-Path $nuget $pkg
        if (Test-Path -LiteralPath $dir) {
            $candidates += Get-ChildItem -LiteralPath $dir -Directory |
                Sort-Object { try { [version]$_.Name } catch { [version]'0.0' } } -Descending |
                ForEach-Object { Join-Path $_.FullName 'bin\x64\Azure.CodeSigning.Dlib.dll' }
        }
    }
    # A repo-local drop (gitignored): nuget install Microsoft.ArtifactSigning.Client -OutputDirectory tools\artifact-signing
    $local = Join-Path $root 'tools\artifact-signing'
    if (Test-Path -LiteralPath $local) {
        $candidates += Get-ChildItem -LiteralPath $local -Recurse -Filter 'Azure.CodeSigning.Dlib.dll' |
            Where-Object { $_.FullName -match '\\x64\\' } | ForEach-Object { $_.FullName }
    }
    foreach ($c in $candidates) { if (Test-Path -LiteralPath $c -PathType Leaf) { return $c } }
    return $null
}

function Get-SignConfig {
    [pscustomobject]@{
        Endpoint = $env:ST2K_SIGN_ENDPOINT
        Account  = $env:ST2K_SIGN_ACCOUNT
        Profile  = $env:ST2K_SIGN_PROFILE
    }
}

function Test-SignConfigured {
    $c = Get-SignConfig
    return [bool]($c.Endpoint -and $c.Account -and $c.Profile)
}

# The verify step reads the signature back through Windows itself, so a signtool that
# printed "Successfully signed" over a broken chain still fails here.
function Assert-Signed([string]$file, [string]$expectSubjectMatch, [switch]$RequireTrusted) {
    $sig = Get-AuthenticodeSignature -LiteralPath $file
    if (-not $sig.SignerCertificate) { throw "no Authenticode signature on $file" }
    if ($expectSubjectMatch -and $sig.SignerCertificate.Subject -notmatch $expectSubjectMatch) {
        throw "unexpected signer on ${file}: $($sig.SignerCertificate.Subject)"
    }
    if ($RequireTrusted -and $sig.Status -ne 'Valid') {
        throw "signature on $file is not Valid: $($sig.Status) - $($sig.StatusMessage)"
    }
    Write-Host ("  signed  {0}`n          by {1}`n          status {2}" -f $file, $sig.SignerCertificate.Subject, $sig.Status) -ForegroundColor DarkGray
}

if ($Configured) {
    if (Test-SignConfigured) { exit 0 } else { exit 2 }
}

if ($Status) {
    $c = Get-SignConfig
    $tool = Resolve-SignTool
    $dlib = Resolve-Dlib
    Write-Host "[sign-release] configuration"
    Write-Host ("  endpoint  {0}" -f $(if ($c.Endpoint) { $c.Endpoint } else { '(ST2K_SIGN_ENDPOINT unset)' }))
    Write-Host ("  account   {0}" -f $(if ($c.Account) { $c.Account } else { '(ST2K_SIGN_ACCOUNT unset)' }))
    Write-Host ("  profile   {0}" -f $(if ($c.Profile) { $c.Profile } else { '(ST2K_SIGN_PROFILE unset)' }))
    Write-Host ("  signtool  {0}" -f $(if ($tool) { $tool } else { 'NOT FOUND (install the Windows 10/11 SDK)' }))
    Write-Host ("  dlib      {0}" -f $(if ($dlib) { $dlib } else { 'NOT FOUND (nuget install Microsoft.ArtifactSigning.Client -OutputDirectory tools\artifact-signing)' }))
    Write-Host ("  verdict   {0}" -f $(if ((Test-SignConfigured) -and $tool -and $dlib) { 'READY to sign' } elseif (Test-SignConfigured) { 'configured, but tooling is missing' } else { 'not configured: releases ship unsigned' }))
    exit 0
}

if ($SelfTest) {
    # Proves everything this script does EXCEPT the Azure round trip: locating signtool,
    # driving it, and the read-back verify. Uses the same self-signed CN=SageThumbs2K
    # certificate make-msix.ps1 keeps in CurrentUser\My, on a scratch COPY of st2k.exe, so
    # nothing in the tree or the stage changes. A self-signed signer is not trusted, so the
    # verify here asserts the signer identity, not chain validity.
    $tool = Resolve-SignTool
    if (-not $tool) { Write-Host "  FAIL  signtool.exe not found (install the Windows 10/11 SDK)" -ForegroundColor Red; exit 1 }
    if (-not $TargetDir) {
        $TargetDir = Join-Path (& (Join-Path $root 'scripts\_targetdir.ps1')) 'release'
    }
    $src = Join-Path $TargetDir 'st2k.exe'
    if (-not (Test-Path -LiteralPath $src -PathType Leaf)) {
        $src = Join-Path (Join-Path (Split-Path $TargetDir -Parent) 'debug') 'st2k.exe'
    }
    if (-not (Test-Path -LiteralPath $src -PathType Leaf)) { Write-Host "  FAIL  no st2k.exe under $TargetDir to copy" -ForegroundColor Red; exit 1 }
    $subject = 'CN=SageThumbs2K'
    $cert = Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert | Where-Object { $_.Subject -eq $subject } | Select-Object -First 1
    if (-not $cert) {
        Write-Host "  generating the self-signed code-signing cert ($subject), 10 years, CurrentUser\My" -ForegroundColor DarkGray
        $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject $subject -CertStoreLocation Cert:\CurrentUser\My `
            -NotAfter (Get-Date).AddYears(10) -KeyExportPolicy Exportable -HashAlgorithm SHA256
    }
    $scratch = Join-Path ([IO.Path]::GetTempPath()) "st2k-sign-selftest-$PID"
    New-Item -ItemType Directory -Force -Path $scratch | Out-Null
    try {
        $copy = Join-Path $scratch 'st2k.exe'
        Copy-Item -LiteralPath $src -Destination $copy
        & $tool sign /q /fd SHA256 /sha1 $cert.Thumbprint $copy
        if ($LASTEXITCODE) { Write-Host "  FAIL  signtool sign exited $LASTEXITCODE" -ForegroundColor Red; exit 1 }
        Assert-Signed $copy ([regex]::Escape($subject))
        Write-Host "  PASS  signtool located at $tool, a signature was written and read back" -ForegroundColor Green
        $dlib = Resolve-Dlib
        Write-Host ("  {0}  Azure dlib {1}" -f $(if ($dlib) { 'PASS' } else { 'INFO' }), $(if ($dlib) { "found at $dlib" } else { 'not present; the Azure leg cannot be self-tested on this machine' })) -ForegroundColor $(if ($dlib) { 'Green' } else { 'Yellow' })
        exit 0
    } finally {
        Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if (-not $Path -or $Path.Count -eq 0) { throw 'Pass -Path <files>, or one of -Status / -Configured / -SelfTest.' }
$files = foreach ($p in $Path) {
    $item = Get-Item -LiteralPath $p -ErrorAction Stop
    if ($item.PSIsContainer) { throw "not a file: $p" }
    $item.FullName
}

if (-not (Test-SignConfigured)) {
    if ($AllowUnsigned) {
        Write-Host ("  unsigned: ST2K_SIGN_ENDPOINT / ST2K_SIGN_ACCOUNT / ST2K_SIGN_PROFILE are not set, so {0} file(s) ship without a signature (docs/RELEASE-SECURITY.md, issue #30)" -f $files.Count) -ForegroundColor Yellow
        exit 0
    }
    Write-Host "  FAIL  signing is not configured (ST2K_SIGN_ENDPOINT / ST2K_SIGN_ACCOUNT / ST2K_SIGN_PROFILE); pass -AllowUnsigned to ship unsigned on purpose" -ForegroundColor Red
    exit 1
}

$tool = Resolve-SignTool
if (-not $tool) { Write-Host "  FAIL  signtool.exe not found (install the Windows 10/11 SDK)" -ForegroundColor Red; exit 1 }
$dlib = Resolve-Dlib
if (-not $dlib) { Write-Host "  FAIL  Azure.CodeSigning.Dlib.dll not found: nuget install Microsoft.ArtifactSigning.Client -OutputDirectory tools\artifact-signing, or set ST2K_SIGN_DLIB" -ForegroundColor Red; exit 1 }
$cfg = Get-SignConfig
$metadata = Join-Path ([IO.Path]::GetTempPath()) "st2k-sign-metadata-$PID.json"
# The exact shape the dlib reads; the endpoint must be the account's own region.
@{
    Endpoint               = $cfg.Endpoint
    CodeSigningAccountName = $cfg.Account
    CertificateProfileName = $cfg.Profile
} | ConvertTo-Json | Set-Content -LiteralPath $metadata -Encoding utf8

$signArgs = @('sign', '/fd', 'SHA256', '/tr', 'http://timestamp.acs.microsoft.com', '/td', 'SHA256',
    '/dlib', $dlib, '/dmdf', $metadata) + $files
try {
    if ($WhatIf) {
        Write-Host "  would run: `"$tool`" $($signArgs -join ' ')"
        Write-Host "  metadata.json: $(Get-Content -LiteralPath $metadata -Raw)"
        exit 0
    }
    Write-Host ("[sign-release] Azure Artifact Signing: {0} file(s) via {1} / {2}" -f $files.Count, $cfg.Account, $cfg.Profile) -ForegroundColor Green
    & $tool @signArgs
    if ($LASTEXITCODE) { Write-Host "  FAIL  signtool sign exited $LASTEXITCODE" -ForegroundColor Red; exit 1 }
    foreach ($f in $files) { Assert-Signed $f $null -RequireTrusted }
    exit 0
} finally {
    Remove-Item -LiteralPath $metadata -Force -ErrorAction SilentlyContinue
}
