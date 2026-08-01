<#
  Windows-only cryptographic and identity regression tests for the release MSIX
  gate. A real package is built/signed with the same helper as release builds.
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'release-manifest-lib.ps1')

$version = ([regex]::Match(
    (Get-Content -LiteralPath (Join-Path $root 'Cargo.toml') -Raw),
    '(?m)^\s*version\s*=\s*"(\d+\.\d+\.\d+)"'
)).Groups[1].Value
if (-not $version) { throw 'could not determine test MSIX version' }

$scratch = Join-Path (
    [IO.Path]::GetTempPath()
) ("st2k-msix-integrity-" + [guid]::NewGuid().ToString('N'))
$existingSignerThumbprints = @(
    Get-ChildItem Cert:\CurrentUser\My |
        Where-Object { $_.Subject -ceq 'CN=SageThumbs2K' } |
        ForEach-Object { $_.Thumbprint }
)
$script:passed = 0

function Assert-Passes([string]$Name, [scriptblock]$Body) {
    try {
        & $Body *> $null
    } catch {
        throw "expected PASS for '$Name', got: $($_.Exception.Message)"
    }
    Write-Host "  PASS  $Name" -ForegroundColor Green
    $script:passed++
}

function Assert-Fails(
    [string]$Name,
    [string]$ExpectedMessage,
    [scriptblock]$Body
) {
    try {
        & $Body *> $null
    } catch {
        if ($_.Exception.Message -notmatch $ExpectedMessage) {
            throw "wrong failure for '$Name': $($_.Exception.Message)"
        }
        Write-Host "  PASS  $Name (failed closed)" -ForegroundColor Green
        $script:passed++
        return
    }
    throw "expected FAILURE for '$Name'"
}

function Set-ZipTextEntry {
    param(
        [Parameter(Mandatory)][string]$ArchivePath,
        [Parameter(Mandatory)][string]$EntryName,
        [Parameter(Mandatory)][scriptblock]$Transform
    )

    $archive = [IO.Compression.ZipFile]::Open(
        $ArchivePath,
        [IO.Compression.ZipArchiveMode]::Update
    )
    try {
        $entry = $archive.GetEntry($EntryName)
        if ($null -eq $entry) { throw "test archive entry missing: $EntryName" }
        $reader = [IO.StreamReader]::new($entry.Open(), [Text.UTF8Encoding]::new($false))
        try {
            $text = $reader.ReadToEnd()
        } finally {
            $reader.Dispose()
        }
        $changed = [string](& $Transform $text)
        if ($changed -ceq $text) { throw "test transform did not change $EntryName" }
        $entry.Delete()
        $replacement = $archive.CreateEntry($EntryName)
        $writer = [IO.StreamWriter]::new(
            $replacement.Open(),
            [Text.UTF8Encoding]::new($false)
        )
        try {
            $writer.Write($changed)
        } finally {
            $writer.Dispose()
        }
    } finally {
        $archive.Dispose()
    }
}

New-Item -ItemType Directory -Path $scratch | Out-Null
try {
    & (Join-Path $root 'packaging\make-msix.ps1') -OutDir $scratch *> $null
    if ($LASTEXITCODE -ne 0) { throw 'test MSIX build/sign failed' }
    $msix = Join-Path $scratch 'SageThumbs2K.msix'
    $certificate = Join-Path $scratch 'SageThumbs2K.cer'

    Assert-Passes 'real signature, signer certificate, and manifest identity' {
        Assert-ReleaseMsixPackage `
            -Path $msix `
            -CertificatePath $certificate `
            -Version $version
    }

    $arm64Out = Join-Path $scratch 'arm64'
    & (Join-Path $root 'packaging\make-msix.ps1') -OutDir $arm64Out -Architecture arm64 *> $null
    if ($LASTEXITCODE -ne 0) { throw 'ARM64 test MSIX build/sign failed' }
    Assert-Passes 'ARM64 signature, signer certificate, and manifest identity' {
        Assert-ReleaseMsixPackage `
            -Path (Join-Path $arm64Out 'SageThumbs2K.msix') `
            -CertificatePath (Join-Path $arm64Out 'SageThumbs2K.cer') `
            -Version $version `
            -ExpectedProcessorArchitecture arm64
    }
    Assert-Fails 'ARM64 package does not pass the neutral identity contract' 'ProcessorArchitecture' {
        Assert-ReleaseMsixPackage `
            -Path (Join-Path $arm64Out 'SageThumbs2K.msix') `
            -CertificatePath (Join-Path $arm64Out 'SageThumbs2K.cer') `
            -Version $version
    }

    $tampered = Join-Path $scratch 'tampered.msix'
    Copy-Item -LiteralPath $msix -Destination $tampered
    $archive = [IO.Compression.ZipFile]::Open(
        $tampered,
        [IO.Compression.ZipArchiveMode]::Update
    )
    try {
        $entry = $archive.GetEntry('Assets/StoreLogo.png')
        if ($null -eq $entry) { throw 'test package has no StoreLogo.png' }
        $stream = $entry.Open()
        try {
            $first = $stream.ReadByte()
            if ($first -lt 0) { throw 'test StoreLogo.png is empty' }
            $stream.Position = 0
            $stream.WriteByte([byte]($first -bxor 0xFF))
        } finally {
            $stream.Dispose()
        }
    } finally {
        $archive.Dispose()
    }
    Assert-Fails 'payload mutation invalidates Appx signature' 'signature verification failed|Authenticode signature' {
        Assert-ReleaseMsixPackage `
            -Path $tampered `
            -CertificatePath $certificate `
            -Version $version
    }

    $rsa = [Security.Cryptography.RSA]::Create(2048)
    $mismatchCertificate = $null
    try {
        $request = [Security.Cryptography.X509Certificates.CertificateRequest]::new(
            'CN=SageThumbs2K',
            $rsa,
            [Security.Cryptography.HashAlgorithmName]::SHA256,
            [Security.Cryptography.RSASignaturePadding]::Pkcs1
        )
        $mismatchCertificate = $request.CreateSelfSigned(
            [DateTimeOffset]::UtcNow.AddMinutes(-5),
            [DateTimeOffset]::UtcNow.AddDays(1)
        )
        $mismatchPath = Join-Path $scratch 'mismatched.cer'
        [IO.File]::WriteAllBytes(
            $mismatchPath,
            $mismatchCertificate.Export(
                [Security.Cryptography.X509Certificates.X509ContentType]::Cert
            )
        )
        Assert-Fails 'same-subject but different bundled certificate' 'does not equal bundled certificate' {
            Assert-ReleaseMsixPackage `
                -Path $msix `
                -CertificatePath $mismatchPath `
                -Version $version
        }
    } finally {
        if ($mismatchCertificate) { $mismatchCertificate.Dispose() }
        $rsa.Dispose()
    }

    $wrongVersion = Join-Path $scratch 'wrong-version.msix'
    Copy-Item -LiteralPath $msix -Destination $wrongVersion
    Set-ZipTextEntry -ArchivePath $wrongVersion -EntryName 'AppxManifest.xml' -Transform {
        param($text)
        $text.Replace("Version=`"$version.0`"", 'Version="9.9.9.0"')
    }
    Assert-Fails 'wrong Appx Identity version' 'Identity Version' {
        Assert-ReleaseMsixIdentity -Path $wrongVersion -Version $version
    }

    $wrongPublisher = Join-Path $scratch 'wrong-publisher.msix'
    Copy-Item -LiteralPath $msix -Destination $wrongPublisher
    Set-ZipTextEntry -ArchivePath $wrongPublisher -EntryName 'AppxManifest.xml' -Transform {
        param($text)
        $text.Replace('Publisher="CN=SageThumbs2K"', 'Publisher="CN=SomeoneElse"')
    }
    Assert-Fails 'wrong Appx Identity publisher' 'Identity Publisher' {
        Assert-ReleaseMsixIdentity -Path $wrongPublisher -Version $version
    }

    Write-Host "[msix-integrity-test] ALL GREEN ($script:passed cases)" -ForegroundColor Green
} finally {
    if (Test-Path -LiteralPath $scratch) {
        [IO.Directory]::Delete($scratch, $true)
    }
    $newSignerCertificates = @(
        Get-ChildItem Cert:\CurrentUser\My |
            Where-Object {
                $_.Subject -ceq 'CN=SageThumbs2K' -and
                $existingSignerThumbprints -notcontains $_.Thumbprint
            }
    )
    foreach ($newCertificate in $newSignerCertificates) {
        Remove-Item -LiteralPath (
            "Cert:\CurrentUser\My\$($newCertificate.Thumbprint)"
        ) -Force
    }
}

# Expected-failure cases intentionally leave signtool's native exit code
# nonzero. The assertions above are authoritative once all cases complete.
exit 0
