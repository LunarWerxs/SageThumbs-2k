Set-StrictMode -Version Latest

function Get-ReleaseCargoBuildArguments {
    param(
        [Parameter(Mandatory)]
        [ValidateSet('x64', 'arm64')]
        [string]$Architecture,

        [Parameter(Mandatory)]
        [ValidateSet('sagethumbs2k', 'sagethumbs2k-dll')]
        [string]$Package,

        [Parameter(Mandatory)]
        [ValidateNotNullOrEmpty()]
        [string]$Features
    )

    $target = if ($Architecture -eq 'arm64') {
        @('--target', 'aarch64-pc-windows-msvc')
    } else {
        @()
    }
    # Keep this one canonical ordering in the build runner and both provenance
    # gates. The ARM64 target must be an actual Cargo argument, not host trivia.
    return @('--release', '--locked') + $target + @('-p', $Package, '--features', $Features)
}

function Get-ReleaseRequiredInputPaths {
    @(
        '.gitattributes',
        'Cargo.toml',
        'Cargo.lock',
        'docs/CHANGELOG.md',
        'docs/MAGICK.md',
        'packaging/make-msix.ps1',
        'packaging/installer.iss',
        'packaging/size-budget.json',
        'packaging/AppxManifest.xml',
        'packaging/imagemagick-source.json',
        'packaging/imagemagick-policy.xml',
        'scripts/_targetdir.ps1',
        'scripts/check-consistency.ps1',
        'scripts/check-dicom.ps1',
        'scripts/check-email-rule.ps1',
        'scripts/check-installer.ps1',
        'scripts/check-magick-bundle.ps1',
        'scripts/check-magick-source.ps1',
        'scripts/build-release.ps1',
        'scripts/check-release-size.ps1',
        'scripts/check-release-manifest.ps1',
        'scripts/check-vendored-exr.ps1',
        'scripts/export-release-notes.ps1',
        'scripts/gen-site.mjs',
        'scripts/prune-magick-unreferenced.ps1',
        'scripts/regression.ps1',
        'scripts/regression-baseline.txt',
        'scripts/release-manifest-lib.ps1',
        'scripts/release.ps1',
        'scripts/test-magick-packaging.ps1',
        'scripts/test-installer-lint.ps1',
        'scripts/test-msix-integrity.ps1',
        'scripts/test-staged-regression.ps1',
        'scripts/write-release-manifest.ps1',
        'vendor/exr/Cargo.toml',
        'vendor/exr/Cargo.toml.orig',
        'vendor/exr/SAGETHUMBS-PATCH.md',
        'vendor/exr/src/lib.rs',
        'vendor/exr/src/compression/dwa/lossy_dct/transfer_curve.rs'
    )
}

function Assert-ReleaseRequiredInputsTracked {
    param(
        [Parameter(Mandatory)]
        [string]$Root,

        [Parameter(Mandatory)]
        [string[]]$RelativePaths
    )

    foreach ($relative in $RelativePaths) {
        & git -C $Root ls-files --error-unmatch -- $relative 2>$null | Out-Null
        if ($LASTEXITCODE -ne 0) {
            throw "required release source input is not Git-tracked: $relative"
        }
    }
}

function Get-ReleaseSha256 {
    param(
        [Parameter(Mandatory)]
        [string]$Path
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "file not found: $Path"
    }
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-ReleasePathUnderRoot {
    param(
        [Parameter(Mandatory)]
        [string]$Root,

        [Parameter(Mandatory)]
        [string]$RelativePath
    )

    if ([IO.Path]::IsPathRooted($RelativePath)) {
        throw "manifest path must be relative: $RelativePath"
    }

    $rootFull = [IO.Path]::GetFullPath($Root).TrimEnd('\', '/')
    $candidate = [IO.Path]::GetFullPath((Join-Path $rootFull $RelativePath))
    $prefix = $rootFull + [IO.Path]::DirectorySeparatorChar
    if (-not $candidate.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "manifest path escapes the repository: $RelativePath"
    }
    return $candidate
}

function Get-ReleaseRelativePath {
    param(
        [Parameter(Mandatory)]
        [string]$Root,

        [Parameter(Mandatory)]
        [string]$Path
    )

    $relative = [IO.Path]::GetRelativePath(
        [IO.Path]::GetFullPath($Root),
        [IO.Path]::GetFullPath($Path)
    ).Replace('\', '/')
    if ($relative -eq '..' -or $relative.StartsWith('../', [StringComparison]::Ordinal)) {
        throw "path is outside its declared root: $Path"
    }
    return $relative
}

function Get-ReleaseFileRecord {
    param(
        [Parameter(Mandatory)]
        [string]$Path,

        [Parameter(Mandatory)]
        [string]$RelativeTo
    )

    $item = Get-Item -LiteralPath $Path -ErrorAction Stop
    if ($item.PSIsContainer) {
        throw "expected a file, got a directory: $Path"
    }
    if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "release payload must not contain reparse points: $Path"
    }

    $record = [ordered]@{
        path   = Get-ReleaseRelativePath -Root $RelativeTo -Path $item.FullName
        bytes  = [int64]$item.Length
        sha256 = Get-ReleaseSha256 -Path $item.FullName
    }

    if ($item.Extension -ieq '.exe' -or $item.Extension -ieq '.dll') {
        $record.fileVersion = ([string]$item.VersionInfo.FileVersion).Trim()
        $record.productVersion = ([string]$item.VersionInfo.ProductVersion).Trim()
        $record.productName = ([string]$item.VersionInfo.ProductName).Trim()
        $record.fileDescription = ([string]$item.VersionInfo.FileDescription).Trim()
    }

    return [pscustomobject]$record
}

function Get-ReleaseStageInventory {
    param(
        [Parameter(Mandatory)]
        [string]$StagePath
    )

    if (-not (Test-Path -LiteralPath $StagePath -PathType Container)) {
        throw "release stage directory not found: $StagePath"
    }

    $records = @(
        Get-ChildItem -LiteralPath $StagePath -Recurse -File |
            ForEach-Object {
                Get-ReleaseFileRecord -Path $_.FullName -RelativeTo $StagePath
            } |
            Sort-Object -Property path
    )
    if ($records.Count -eq 0) {
        throw "release stage is empty: $StagePath"
    }
    return $records
}

function Assert-ReleasePeFile {
    param(
        [Parameter(Mandatory)]
        [string]$Path
    )

    $item = Get-Item -LiteralPath $Path -ErrorAction Stop
    if ($item.Length -lt 68) {
        throw "PE file is too small: $Path"
    }

    $stream = [IO.File]::Open(
        $item.FullName,
        [IO.FileMode]::Open,
        [IO.FileAccess]::Read,
        [IO.FileShare]::Read
    )
    try {
        $reader = [IO.BinaryReader]::new($stream)
        try {
            if ($reader.ReadByte() -ne 0x4D -or $reader.ReadByte() -ne 0x5A) {
                throw "file does not have an MZ header: $Path"
            }
            $stream.Position = 0x3C
            $peOffset = $reader.ReadInt32()
            if ($peOffset -lt 64 -or $peOffset -gt $item.Length - 4) {
                throw "file has an invalid PE header offset: $Path"
            }
            $stream.Position = $peOffset
            if ($reader.ReadUInt32() -ne 0x00004550) {
                throw "file does not have a PE signature: $Path"
            }
        } finally {
            $reader.Dispose()
        }
    } finally {
        $stream.Dispose()
    }
}

function Assert-ReleasePeArchitecture {
    param(
        [Parameter(Mandatory)]
        [string]$Path,

        [Parameter(Mandatory)]
        [ValidateSet('x64', 'arm64')]
        [string]$Architecture
    )

    Assert-ReleasePeFile -Path $Path
    $expectedMachine = if ($Architecture -eq 'arm64') { [uint16]0xAA64 } else { [uint16]0x8664 }
    $item = Get-Item -LiteralPath $Path -ErrorAction Stop
    $stream = [IO.File]::Open($item.FullName, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $reader = [IO.BinaryReader]::new($stream)
        try {
            $stream.Position = 0x3C
            $peOffset = $reader.ReadInt32()
            if ($peOffset -lt 64 -or $peOffset -gt $item.Length - 6) {
                throw "file has an invalid PE/COFF machine offset: $Path"
            }
            $stream.Position = $peOffset + 4
            $machine = $reader.ReadUInt16()
            if ($machine -ne $expectedMachine) {
                throw ("PE machine mismatch for '{0}': 0x{1:X4} (expected {2}/0x{3:X4})" -f $Path, $machine, $Architecture, $expectedMachine)
            }
        } finally {
            $reader.Dispose()
        }
    } finally {
        $stream.Dispose()
    }
}

function Assert-ReleasePeMetadata {
    param(
        [Parameter(Mandatory)]
        [string]$Path,

        [Parameter(Mandatory)]
        [string]$Version,

        [string]$ProductName = 'SageThumbs 2K',

        [string]$Description
    )

    Assert-ReleasePeFile -Path $Path
    $info = (Get-Item -LiteralPath $Path -ErrorAction Stop).VersionInfo
    $fileVersion = ([string]$info.FileVersion).Trim()
    $productVersion = ([string]$info.ProductVersion).Trim()
    $actualProduct = ([string]$info.ProductName).Trim()
    $actualDescription = ([string]$info.FileDescription).Trim()

    if ($fileVersion -ne $Version -or $productVersion -ne $Version) {
        throw "PE version mismatch for '$Path': file=$fileVersion product=$productVersion expected=$Version"
    }
    if ($actualProduct -ne $ProductName) {
        throw "PE product-name mismatch for '$Path': '$actualProduct' (expected '$ProductName')"
    }
    if ($Description -and $actualDescription -ne $Description) {
        throw "PE description mismatch for '$Path': '$actualDescription' (expected '$Description')"
    }
}

function Assert-ReleaseZipFile {
    param(
        [Parameter(Mandatory)]
        [string]$Path
    )

    $item = Get-Item -LiteralPath $Path -ErrorAction Stop
    if ($item.Length -lt 22) {
        throw "ZIP/MSIX file is too small: $Path"
    }
    $stream = [IO.File]::OpenRead($item.FullName)
    try {
        $signature = [byte[]]::new(4)
        $read = $stream.Read($signature, 0, $signature.Length)
        $zipSignature = $signature[0] -eq 0x50 -and $signature[1] -eq 0x4B -and (
            ($signature[2] -eq 0x03 -and $signature[3] -eq 0x04) -or
            ($signature[2] -eq 0x05 -and $signature[3] -eq 0x06) -or
            ($signature[2] -eq 0x07 -and $signature[3] -eq 0x08)
        )
        if ($read -ne $signature.Length -or -not $zipSignature) {
            throw "file does not have a ZIP/MSIX header: $Path"
        }
    } finally {
        $stream.Dispose()
    }

    # A four-byte signature alone is not integrity: parse the central directory,
    # require the MSIX control files, and consume every member so truncation or a
    # corrupt compressed stream fails before publication.
    try {
        $archive = [IO.Compression.ZipFile]::OpenRead($item.FullName)
        try {
            if ($archive.Entries.Count -eq 0) {
                throw 'archive contains no entries'
            }
            $seen = [Collections.Generic.HashSet[string]]::new(
                [StringComparer]::OrdinalIgnoreCase
            )
            $buffer = [byte[]]::new(65536)
            foreach ($entry in $archive.Entries) {
                $entryPath = $entry.FullName.Replace('\', '/')
                if ([string]::IsNullOrWhiteSpace($entryPath) -or
                    $entryPath.StartsWith('/', [StringComparison]::Ordinal) -or
                    $entryPath -eq '..' -or
                    $entryPath.StartsWith('../', [StringComparison]::Ordinal) -or
                    $entryPath.Contains('/../', [StringComparison]::Ordinal)) {
                    throw "archive contains an unsafe entry path: '$($entry.FullName)'"
                }
                if (-not $seen.Add($entryPath)) {
                    throw "archive contains duplicate entry path: '$entryPath'"
                }

                if (-not [string]::IsNullOrEmpty($entry.Name)) {
                    $entryStream = $entry.Open()
                    try {
                        while ($entryStream.Read($buffer, 0, $buffer.Length) -ne 0) {}
                    } finally {
                        $entryStream.Dispose()
                    }
                }
            }

            foreach ($required in @(
                    '[Content_Types].xml',
                    'AppxManifest.xml',
                    'AppxBlockMap.xml',
                    'AppxSignature.p7x'
                )) {
                if (-not $seen.Contains($required)) {
                    throw "archive is missing required MSIX entry '$required'"
                }
                $requiredEntry = $archive.GetEntry($required)
                if ($null -eq $requiredEntry -or $requiredEntry.Length -eq 0) {
                    throw "required MSIX entry '$required' is empty"
                }
            }
        } finally {
            $archive.Dispose()
        }
    } catch {
        throw "file is not a structurally valid MSIX archive '$Path': $($_.Exception.Message)"
    }
}

function Assert-ReleaseCertificate {
    param(
        [Parameter(Mandatory)]
        [string]$Path,

        [string]$ExpectedSubject = 'CN=SageThumbs2K'
    )

    try {
        $certificate = [Security.Cryptography.X509Certificates.X509Certificate2]::new(
            (Resolve-Path -LiteralPath $Path).Path
        )
        try {
            if ($certificate.Subject -cne $ExpectedSubject) {
                throw "certificate subject is '$($certificate.Subject)' (expected '$ExpectedSubject')"
            }
            if ($certificate.NotAfter.ToUniversalTime() -le [DateTime]::UtcNow) {
                throw "certificate is expired: $($certificate.NotAfter)"
            }
        } finally {
            $certificate.Dispose()
        }
    } catch {
        throw "invalid release certificate '$Path': $($_.Exception.Message)"
    }
}

function Resolve-ReleaseSignTool {
    $command = Get-Command signtool.exe -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($command) {
        return $command.Source
    }

    $sdkBin = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    if (Test-Path -LiteralPath $sdkBin -PathType Container) {
        $sdk = Get-ChildItem -LiteralPath $sdkBin -Directory |
            Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'x64\signtool.exe') } |
            Sort-Object Name -Descending |
            Select-Object -First 1
        if ($sdk) {
            return Join-Path $sdk.FullName 'x64\signtool.exe'
        }
    }
    throw 'signtool.exe is required to verify the MSIX signature'
}

function Get-ReleaseMsixIdentity {
    param(
        [Parameter(Mandatory)]
        [string]$Path
    )

    Assert-ReleaseZipFile -Path $Path
    $archive = [IO.Compression.ZipFile]::OpenRead(
        (Resolve-Path -LiteralPath $Path -ErrorAction Stop).Path
    )
    try {
        $entry = $archive.GetEntry('AppxManifest.xml')
        if ($null -eq $entry) {
            throw 'MSIX does not contain AppxManifest.xml'
        }
        $reader = [IO.StreamReader]::new($entry.Open(), [Text.UTF8Encoding]::new($false))
        try {
            $manifestText = $reader.ReadToEnd()
        } finally {
            $reader.Dispose()
        }
    } finally {
        $archive.Dispose()
    }

    try {
        [xml]$manifest = $manifestText
    } catch {
        throw "MSIX AppxManifest.xml is invalid XML: $($_.Exception.Message)"
    }
    $identity = $manifest.SelectSingleNode(
        '/*[local-name()="Package"]/*[local-name()="Identity"]'
    )
    if ($null -eq $identity) {
        throw 'MSIX AppxManifest.xml has no Package/Identity element'
    }
    foreach ($attribute in 'Name', 'Publisher', 'Version', 'ProcessorArchitecture') {
        if (-not $identity.HasAttribute($attribute)) {
            throw "MSIX Identity is missing '$attribute'"
        }
    }
    [pscustomobject]@{
        Name = $identity.GetAttribute('Name')
        Publisher = $identity.GetAttribute('Publisher')
        Version = $identity.GetAttribute('Version')
        ProcessorArchitecture = $identity.GetAttribute('ProcessorArchitecture')
    }
}

function Assert-ReleaseMsixIdentity {
    param(
        [Parameter(Mandatory)]
        [string]$Path,

        [Parameter(Mandatory)]
        [ValidatePattern('^\d+\.\d+\.\d+$')]
        [string]$Version,

        [string]$ExpectedName = 'SageThumbs2K',

        [string]$ExpectedPublisher = 'CN=SageThumbs2K',

        [ValidateSet('neutral', 'arm64')]
        [string]$ExpectedProcessorArchitecture = 'neutral'
    )

    $identity = Get-ReleaseMsixIdentity -Path $Path
    $expectedVersion = "$Version.0"
    if ([string]$identity.Name -cne $ExpectedName) {
        throw "MSIX Identity Name is '$($identity.Name)' (expected '$ExpectedName')"
    }
    if ([string]$identity.Publisher -cne $ExpectedPublisher) {
        throw "MSIX Identity Publisher is '$($identity.Publisher)' (expected '$ExpectedPublisher')"
    }
    if ([string]$identity.Version -cne $expectedVersion) {
        throw "MSIX Identity Version is '$($identity.Version)' (expected '$expectedVersion')"
    }
    if ([string]$identity.ProcessorArchitecture -cne $ExpectedProcessorArchitecture) {
        throw "MSIX Identity ProcessorArchitecture is '$($identity.ProcessorArchitecture)' (expected '$ExpectedProcessorArchitecture')"
    }
}

function Assert-ReleaseMsixPackage {
    param(
        [Parameter(Mandatory)]
        [string]$Path,

        [Parameter(Mandatory)]
        [string]$CertificatePath,

        [Parameter(Mandatory)]
        [ValidatePattern('^\d+\.\d+\.\d+$')]
        [string]$Version,

        [ValidateSet('neutral', 'arm64')]
        [string]$ExpectedProcessorArchitecture = 'neutral',

        [string]$SignToolPath
    )

    Assert-ReleaseZipFile -Path $Path
    Assert-ReleaseCertificate -Path $CertificatePath

    $tool = if ($SignToolPath) {
        (Resolve-Path -LiteralPath $SignToolPath -ErrorAction Stop).Path
    } else {
        Resolve-ReleaseSignTool
    }
    $expectedCertificate = [Security.Cryptography.X509Certificates.X509Certificate2]::new(
        (Resolve-Path -LiteralPath $CertificatePath -ErrorAction Stop).Path
    )
    $trustedPeople = [Security.Cryptography.X509Certificates.X509Store]::new(
        'TrustedPeople',
        [Security.Cryptography.X509Certificates.StoreLocation]::LocalMachine
    )
    $addedTemporaryTrust = $false
    try {
        $untrustedSignature = Get-AuthenticodeSignature -LiteralPath $Path
        if ($null -eq $untrustedSignature -or
            $null -eq $untrustedSignature.SignerCertificate) {
            throw 'MSIX has no readable Authenticode signer certificate'
        }
        $expectedCertificateHash = [Convert]::ToHexString(
            [Security.Cryptography.SHA256]::HashData($expectedCertificate.RawData)
        )
        $signerCertificateHash = [Convert]::ToHexString(
            [Security.Cryptography.SHA256]::HashData(
                $untrustedSignature.SignerCertificate.RawData
            )
        )
        if ($signerCertificateHash -cne $expectedCertificateHash) {
            throw "MSIX signer certificate does not equal bundled certificate " +
                "(signer $($untrustedSignature.SignerCertificate.Thumbprint), " +
                "bundled $($expectedCertificate.Thumbprint))"
        }

        # The shipped package deliberately uses a self-signed app-package
        # certificate. Verify it against that exact bundled public certificate
        # without requiring a release runner to have installed SageThumbs first.
        # TrustedPeople is the same non-root store used by the installer.
        $trustedPeople.Open(
            [Security.Cryptography.X509Certificates.OpenFlags]::ReadOnly
        )
        $trusted = $trustedPeople.Certificates.Find(
            [Security.Cryptography.X509Certificates.X509FindType]::FindByThumbprint,
            $expectedCertificate.Thumbprint,
            $false
        )
        if ($trusted.Count -eq 0) {
            $trustedPeople.Close()
            $trustedPeople.Open(
                [Security.Cryptography.X509Certificates.OpenFlags]::ReadWrite
            )
            $trustedPeople.Add($expectedCertificate)
            $addedTemporaryTrust = $true
        }
        $trustedPeople.Close()

        $verifyOutput = @(& $tool verify /pa /all /v $Path 2>&1)
        if ($LASTEXITCODE -ne 0) {
            throw "MSIX signature verification failed using signtool:`n$($verifyOutput -join "`n")"
        }

        $signature = Get-AuthenticodeSignature -LiteralPath $Path
        if ($null -eq $signature -or [string]$signature.Status -cne 'Valid' -or
            $null -eq $signature.SignerCertificate) {
            $status = if ($null -eq $signature) { '<no result>' } else { [string]$signature.Status }
            throw "MSIX Authenticode signature is not valid: $status"
        }

        Assert-ReleaseMsixIdentity `
            -Path $Path `
            -Version $Version `
            -ExpectedPublisher $expectedCertificate.Subject `
            -ExpectedProcessorArchitecture $ExpectedProcessorArchitecture
    } finally {
        $trustedPeople.Close()
        if ($addedTemporaryTrust) {
            $cleanupStore = [Security.Cryptography.X509Certificates.X509Store]::new(
                'TrustedPeople',
                [Security.Cryptography.X509Certificates.StoreLocation]::LocalMachine
            )
            try {
                $cleanupStore.Open(
                    [Security.Cryptography.X509Certificates.OpenFlags]::ReadWrite
                )
                $temporaryCertificates = $cleanupStore.Certificates.Find(
                    [Security.Cryptography.X509Certificates.X509FindType]::FindByThumbprint,
                    $expectedCertificate.Thumbprint,
                    $false
                )
                foreach ($temporaryCertificate in $temporaryCertificates) {
                    $cleanupStore.Remove($temporaryCertificate)
                }
            } finally {
                $cleanupStore.Close()
                $cleanupStore.Dispose()
            }
        }
        $trustedPeople.Dispose()
        $expectedCertificate.Dispose()
    }
}

function Get-ReleaseRequiredProperty {
    param(
        [Parameter(Mandatory)]
        [object]$Object,

        [Parameter(Mandatory)]
        [string]$Name
    )

    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property -or $null -eq $property.Value) {
        throw "release manifest is missing '$Name'"
    }
    return $property.Value
}

function Assert-ReleaseRecordMatches {
    param(
        [Parameter(Mandatory)]
        [object]$Expected,

        [Parameter(Mandatory)]
        [object]$Actual,

        [Parameter(Mandatory)]
        [string]$Context
    )

    foreach ($field in 'path', 'bytes', 'sha256') {
        $expectedValue = Get-ReleaseRequiredProperty -Object $Expected -Name $field
        $actualValue = Get-ReleaseRequiredProperty -Object $Actual -Name $field
        if ([string]$expectedValue -cne [string]$actualValue) {
            throw "$Context changed: $field expected '$expectedValue', got '$actualValue'"
        }
    }

    foreach ($field in 'fileVersion', 'productVersion', 'productName', 'fileDescription') {
        $expectedProperty = $Expected.PSObject.Properties[$field]
        if ($null -ne $expectedProperty) {
            $actualValue = Get-ReleaseRequiredProperty -Object $Actual -Name $field
            if ([string]$expectedProperty.Value -cne [string]$actualValue) {
                throw "$Context changed: $field expected '$($expectedProperty.Value)', got '$actualValue'"
            }
        }
    }
}

function Get-ReleaseChangelogSection {
    param(
        [Parameter(Mandatory)]
        [string]$ChangelogPath,

        [Parameter(Mandatory)]
        [string]$Version
    )

    if (-not (Test-Path -LiteralPath $ChangelogPath -PathType Leaf)) {
        throw "release changelog not found: $ChangelogPath"
    }

    $text = Get-Content -LiteralPath $ChangelogPath -Raw
    $headingPattern = "(?m)^##[ ]+$([regex]::Escape($Version))[ ]*\r?$"
    $headings = [regex]::Matches($text, $headingPattern)
    if ($headings.Count -ne 1) {
        throw "changelog must contain exactly one '## $Version' section (found $($headings.Count))"
    }

    $start = $headings[0].Index + $headings[0].Length
    $remainder = $text.Substring($start)
    $next = [regex]::Match($remainder, '(?m)^##[ ]+\S.*\r?$')
    $section = if ($next.Success) {
        $remainder.Substring(0, $next.Index)
    } else {
        $remainder
    }
    $section = $section.Trim()
    if ($section.Length -lt 80 -or $section -notmatch '(?m)^-[ ]+\S') {
        throw "changelog section $Version is too short or has no release-note bullets"
    }
    if ($section -match '(?i)\b(?:TODO|TBD|PLACEHOLDER|CHANGEME)\b') {
        throw "changelog section $Version still contains placeholder text"
    }
    return $section
}
