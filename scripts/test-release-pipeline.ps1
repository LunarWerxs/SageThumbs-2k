<#
  Dependency-free fail-closed tests for release provenance and curated notes.
  These tests intentionally do not need a built installer.
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'release-manifest-lib.ps1')

$scratch = Join-Path ([IO.Path]::GetTempPath()) ("st2k-release-pipeline-" + [guid]::NewGuid().ToString('N'))
$script:passed = 0
. (Join-Path $PSScriptRoot 'test-assert-lib.ps1')

function Assert-Fails([string]$Name, [scriptblock]$Body) {
    $failed = $false
    try { & $Body *> $null } catch { $failed = $true }
    if (-not $failed) { throw "expected FAILURE for '$Name'" }
    Write-Host "  PASS  $Name (failed closed)" -ForegroundColor Green
    $script:passed++
}

New-Item -ItemType Directory -Path $scratch -Force | Out-Null
try {
    $changelog = Join-Path $scratch 'CHANGELOG.md'
    @'
# Changelog

## 9.8.7

- **A curated release note.** This deliberately contains enough useful detail to
  satisfy the quality floor and prove that only the requested version is exported.

## 9.8.6

- Older release material must not leak into the current release notes.
'@ | Set-Content -LiteralPath $changelog -Encoding utf8

    Assert-Passes 'extracts one substantive current-version changelog section' {
        $section = Get-ReleaseChangelogSection -ChangelogPath $changelog -Version '9.8.7'
        if ($section -notmatch 'curated release note' -or $section -match 'Older release') {
            throw 'wrong changelog section was extracted'
        }
    }

    Assert-Fails 'missing current-version changelog section' {
        Get-ReleaseChangelogSection -ChangelogPath $changelog -Version '9.8.8'
    }

    Add-Content -LiteralPath $changelog -Value @'

## 9.8.7

- A duplicate heading is ambiguous and must fail instead of picking one.
'@
    Assert-Fails 'duplicate current-version changelog section' {
        Get-ReleaseChangelogSection -ChangelogPath $changelog -Version '9.8.7'
    }

    @'
# Changelog

## 9.8.7

- TODO: replace this placeholder with reviewed and useful release notes before publishing.
'@ | Set-Content -LiteralPath $changelog -Encoding utf8
    Assert-Fails 'placeholder release notes' {
        Get-ReleaseChangelogSection -ChangelogPath $changelog -Version '9.8.7'
    }

    # Every shape a real unfilled template takes must still fail closed.
    foreach ($marker in @(
            '- TBD before we ship this.',
            '- CHANGEME: write the notes.',
            '- PLACEHOLDER - fill this in.',
            '- <placeholder> goes here.',
            '- [placeholder] goes here.',
            '- {{placeholder}} goes here.')) {
        @"
# Changelog

## 9.8.7

$marker
Some more filler so the section clears the minimum length check that runs before this one.
"@ | Set-Content -LiteralPath $changelog -Encoding utf8
        Assert-Fails "template marker still fails closed: $marker" {
            Get-ReleaseChangelogSection -ChangelogPath $changelog -Version '9.8.7'
        }
    }

    # And the false positive that blocked a finished 2.3.1: `placeholder` is ordinary domain
    # prose in a thumbnailing product. A gate that rejects correct release notes gets worked
    # around, which is worse than not having one.
    @'
# Changelog

## 9.8.7

- **Some camera RAW photos thumbnailed as a black square.** Some cameras, Kodak among them,
  store a blank placeholder in that slot rather than a real preview, and being large is not the
  same as having a picture in it. The preview is now checked for content before it is used.
'@ | Set-Content -LiteralPath $changelog -Encoding utf8
    Assert-Passes 'the word placeholder in ordinary release prose is not a template marker' {
        $sec = Get-ReleaseChangelogSection -ChangelogPath $changelog -Version '9.8.7'
        if ($sec -notmatch 'blank placeholder') { throw 'section did not round-trip' }
    }

    Assert-Fails 'manifest relative path cannot escape repository' {
        Get-ReleasePathUnderRoot -Root $root -RelativePath '..\outside.exe'
    }

    Assert-Passes 'build-release helper scripts are manifest-hashed' {
        $buildText = Get-Content -LiteralPath (
            Join-Path $root 'scripts\build-release.ps1'
        ) -Raw
        $helpers = [Collections.Generic.HashSet[string]]::new(
            [StringComparer]::OrdinalIgnoreCase
        )
        foreach ($match in [regex]::Matches(
                $buildText,
                '\$PSScriptRoot[\\/](?<name>[A-Za-z0-9_.-]+\.(?:ps1|mjs))',
                [Text.RegularExpressions.RegexOptions]::IgnoreCase
            )) {
            $null = $helpers.Add("scripts/$($match.Groups['name'].Value)")
        }
        foreach ($match in [regex]::Matches(
                $buildText,
                '\$root[\\/](?<path>(?:scripts|packaging)[\\/][^"''\s]+\.(?:ps1|mjs))',
                [Text.RegularExpressions.RegexOptions]::IgnoreCase
            )) {
            $null = $helpers.Add($match.Groups['path'].Value.Replace('\', '/'))
        }
        foreach ($match in [regex]::Matches(
                $buildText,
                'Join-Path\s+\$PSScriptRoot\s+[''"](?<name>[^''"]+\.(?:ps1|mjs))[''"]',
                [Text.RegularExpressions.RegexOptions]::IgnoreCase
            )) {
            $null = $helpers.Add("scripts/$($match.Groups['name'].Value)")
        }
        foreach ($match in [regex]::Matches(
                $buildText,
                'Join-Path\s+\$root\s+[''"](?<path>(?:scripts|packaging)[\\/][^''"]+\.(?:ps1|mjs))[''"]',
                [Text.RegularExpressions.RegexOptions]::IgnoreCase
            )) {
            $null = $helpers.Add($match.Groups['path'].Value.Replace('\', '/'))
        }

        $manifestInputs = @(Get-ReleaseRequiredInputPaths)
        foreach ($helper in $helpers) {
            if ($manifestInputs -cnotcontains $helper) {
                throw "direct build-release helper is not manifest-hashed: $helper"
            }
        }
        foreach ($expected in @(
                'scripts/packaging/make-msix.ps1',
                'scripts/_targetdir.ps1',
                'scripts/check-installer.ps1',
                'scripts/check-magick-bundle.ps1',
                'scripts/check-magick-source.ps1',
                'scripts/check-release-size.ps1',
                'scripts/gen-site.mjs',
                'scripts/prune-magick-unreferenced.ps1',
                'scripts/write-release-manifest.ps1'
            )) {
            if (-not $helpers.Contains($expected)) {
                throw "helper discovery test stopped recognizing: $expected"
            }
        }
    }

    Assert-Passes 'required release inputs are Git-tracked' {
        $manifestInputs = @(Get-ReleaseRequiredInputPaths)
        Assert-ReleaseRequiredInputsTracked -Root $root -RelativePaths $manifestInputs
    }

    # The portable zip is an ordinary asset with no provenance manifest and no calibrated size
    # reference, so NOTHING else in this pipeline would notice if it silently stopped being
    # built or uploaded - the release would just publish installers and look entirely healthy.
    # These two pin the parts that have no other alarm.
    Assert-Passes 'release.ps1 builds and uploads the portable zips' {
        $releaseText = Get-Content -LiteralPath (Join-Path $root 'scripts\release.ps1') -Raw
        if ($releaseText -notmatch '-Portable') {
            throw 'release.ps1 no longer invokes build-release.ps1 -Portable'
        }
        if ($releaseText -notmatch '\$artifact\.Portable\.FullName') {
            throw 'release.ps1 no longer adds the portable zip to the uploaded release assets'
        }
        foreach ($expected in 'SageThumbs2K-Portable-$ver.zip', 'SageThumbs2K-Portable-$ver-arm64.zip') {
            if ($releaseText -notmatch [regex]::Escape($expected)) {
                throw "release.ps1 artifact table no longer names the portable zip: $expected"
            }
        }
    }

    Assert-Passes 'portable builds stage outside the installer stage' {
        # Staging wipes and rebuilds its directory, and the portable pass deliberately omits the
        # DLL. If the two ever share a directory again, a portable build inside a release gets to
        # gut the exact stage check-release-manifest.ps1 validates the installer against.
        $buildText = Get-Content -LiteralPath (Join-Path $root 'scripts\build-release.ps1') -Raw
        if ($buildText -notmatch 'portable-src-\$Architecture') {
            throw 'build-release.ps1 -Portable no longer stages into its own portable-src directory'
        }
        $stageAssignment = [regex]::Match(
            $buildText,
            '(?s)\$stage\s*=\s*if\s*\(\s*\$Portable\s*\)\s*\{.*?\}\s*else\s*\{.*?\}'
        )
        if (-not $stageAssignment.Success) {
            throw 'build-release.ps1 $stage is no longer selected by the -Portable switch'
        }
    }

    $corrupt = Join-Path $scratch 'corrupt.exe'
    [IO.File]::WriteAllBytes($corrupt, [byte[]](0x4D, 0x5A, 0, 0))
    Assert-Fails 'truncated MZ file is not accepted as a PE' {
        Assert-ReleasePeFile -Path $corrupt
    }

    $x64Pe = Join-Path $scratch 'x64-machine.exe'
    $x64PeBytes = [byte[]]::new(128)
    $x64PeBytes[0] = 0x4D; $x64PeBytes[1] = 0x5A
    [BitConverter]::GetBytes([int32]64).CopyTo($x64PeBytes, 0x3C)
    [BitConverter]::GetBytes([uint32]0x00004550).CopyTo($x64PeBytes, 64)
    [BitConverter]::GetBytes([uint16]0x8664).CopyTo($x64PeBytes, 68)
    [IO.File]::WriteAllBytes($x64Pe, $x64PeBytes)
    Assert-Passes 'x64 PE machine is accepted for x64 payloads' {
        Assert-ReleasePeArchitecture -Path $x64Pe -Architecture x64
    }
    Assert-FailsLike 'x64 PE machine is rejected for ARM64 payloads' '*PE machine mismatch*' {
        Assert-ReleasePeArchitecture -Path $x64Pe -Architecture arm64
    }

    $fakeMsix = Join-Path $scratch 'fake.msix'
    [IO.File]::WriteAllBytes($fakeMsix, [byte[]](0x50, 0x4B, 0x03, 0x06) + [byte[]]::new(32))
    Assert-Fails 'mismatched ZIP signature is not accepted as an MSIX' {
        Assert-ReleaseZipFile -Path $fakeMsix
    }

    $truncatedMsix = Join-Path $scratch 'truncated.msix'
    [IO.File]::WriteAllBytes(
        $truncatedMsix,
        [byte[]](0x50, 0x4B, 0x03, 0x04) + [byte[]]::new(18)
    )
    Assert-Fails 'ZIP header without a valid archive is not accepted as an MSIX' {
        Assert-ReleaseZipFile -Path $truncatedMsix
    }

    $validZip = Join-Path $scratch 'valid.msix'
    $archive = [IO.Compression.ZipFile]::Open(
        $validZip,
        [IO.Compression.ZipArchiveMode]::Create
    )
    try {
        foreach ($name in @(
                '[Content_Types].xml',
                'AppxManifest.xml',
                'AppxBlockMap.xml',
                'AppxSignature.p7x'
            )) {
            $entry = $archive.CreateEntry($name)
            $writer = [IO.StreamWriter]::new($entry.Open())
            try {
                $writer.Write("test content for $name")
            } finally {
                $writer.Dispose()
            }
        }
    } finally {
        $archive.Dispose()
    }
    Assert-Passes 'structurally valid MSIX archive is accepted' {
        Assert-ReleaseZipFile -Path $validZip
    }

    $fakeCertificate = Join-Path $scratch 'fake.cer'
    Set-Content -LiteralPath $fakeCertificate -Value 'not a certificate' -Encoding ascii
    Assert-Fails 'invalid modern-menu certificate' {
        Assert-ReleaseCertificate -Path $fakeCertificate
    }

    $hashedFile = Join-Path $scratch 'hashed.bin'
    Set-Content -LiteralPath $hashedFile -Value 'first bytes' -Encoding utf8
    $recordBefore = Get-ReleaseFileRecord -Path $hashedFile -RelativeTo $scratch
    Set-Content -LiteralPath $hashedFile -Value 'different bytes' -Encoding utf8
    $recordAfter = Get-ReleaseFileRecord -Path $hashedFile -RelativeTo $scratch
    Assert-Fails 'post-build artifact mutation changes its manifest record' {
        Assert-ReleaseRecordMatches -Expected $recordBefore -Actual $recordAfter -Context 'test artifact'
    }

    $stage = Join-Path $scratch 'stage'
    New-Item -ItemType Directory -Path $stage | Out-Null
    $manifest = Join-Path $scratch 'setup.release.json'
    $installer = Join-Path $scratch 'setup.exe'
    Copy-Item -LiteralPath $corrupt -Destination $installer

    Assert-Fails 'missing release provenance manifest' {
        & (Join-Path $PSScriptRoot 'check-release-manifest.ps1') `
            -InstallerPath $installer `
            -StagePath $stage `
            -ExpectedVersion '9.8.7' `
            -ExpectedCommitSha ('a' * 40) `
            -ManifestPath (Join-Path $scratch 'missing.json')
    }

    Set-Content -LiteralPath $manifest -Value '{not-json' -Encoding utf8
    Assert-Fails 'malformed release provenance manifest' {
        & (Join-Path $PSScriptRoot 'check-release-manifest.ps1') `
            -InstallerPath $installer `
            -StagePath $stage `
            -ExpectedVersion '9.8.7' `
            -ExpectedCommitSha ('a' * 40) `
            -ManifestPath $manifest
    }

    @{
        schemaVersion = 1
        product = 'SageThumbs 2K'
        version = '9.8.7'
        createdUtc = [DateTime]::UtcNow.ToString('o')
        commitSha = ('a' * 40)
        sourceTreeClean = $true
        publishable = $false
        publishableReasons = @('Rust build was skipped')
        build = @{
            rustBuildPerformed = $false
            imageMagickBundled = $true
            modernMenuBundled = $true
        }
    } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $manifest -Encoding utf8
    # ConvertFrom-Json in current PowerShell converts this ISO-Z value directly
    # to DateTime. The checker must preserve UTC and reach the intended gate,
    # rather than reinterpret it as local time and report a false future date.
    Assert-FailsLike 'ISO-Z timestamp reaches non-publishable gate' '*marks this artifact non-publishable*' {
        & (Join-Path $PSScriptRoot 'check-release-manifest.ps1') `
            -InstallerPath $installer `
            -StagePath $stage `
            -ExpectedVersion '9.8.7' `
            -ExpectedCommitSha ('a' * 40) `
            -ManifestPath $manifest
    }

    @{
        schemaVersion = 1
        product = 'SageThumbs 2K'
        version = '9.8.7'
        createdUtc = [DateTime]::UtcNow.ToString('o')
        commitSha = ('b' * 40)
        sourceTreeClean = $true
        publishable = $true
        publishableReasons = @()
        build = @{
            rustBuildPerformed = $true
            cargoLocked = $true
            rustTarget = 'x86_64-pc-windows-msvc'
            rustFlags = '-C target-feature=+crt-static'
            imageMagickBundled = $true
            modernMenuBundled = $true
        }
    } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $manifest -Encoding utf8
    Assert-Fails 'manifest from a different commit is stale' {
        & (Join-Path $PSScriptRoot 'check-release-manifest.ps1') `
            -InstallerPath $installer `
            -StagePath $stage `
            -ExpectedVersion '9.8.7' `
            -ExpectedCommitSha ('a' * 40) `
            -ManifestPath $manifest
    }

    $currentHead = (& git -C $root rev-parse HEAD).Trim()

    # A manifest may not claim an ImageMagick payload the STAGE does not have. This used
    # to be an architecture rule ("ARM64 is Compact"); ARM64 is Full now, so the check is
    # against the staged reality instead, which catches the same lie on either arch.
    @{
        schemaVersion = 1
        product = 'SageThumbs 2K'
        version = '9.8.7'
        createdUtc = [DateTime]::UtcNow.ToString('o')
        commitSha = $currentHead
        sourceTreeClean = $true
        publishable = $true
        publishableReasons = @()
        architecture = 'arm64'
        build = @{
            rustBuildPerformed = $true
            cargoLocked = $true
            rustTarget = 'aarch64-pc-windows-msvc'
            rustFlags = '-C target-feature=+crt-static'
            imageMagickBundled = $true
            modernMenuBundled = $true
        }
    } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $manifest -Encoding utf8
    Assert-FailsLike 'manifest claiming ImageMagick the stage lacks is rejected' '*disagrees with the stage*' {
        & (Join-Path $PSScriptRoot 'check-release-manifest.ps1') `
            -InstallerPath $installer `
            -StagePath $stage `
            -ExpectedVersion '9.8.7' `
            -ExpectedCommitSha $currentHead `
            -Architecture arm64 `
            -ManifestPath $manifest
    }

    @{
        schemaVersion = 1
        product = 'SageThumbs 2K'
        version = '9.8.7'
        createdUtc = [DateTime]::UtcNow.ToString('o')
        commitSha = $currentHead
        sourceTreeClean = $true
        publishable = $true
        publishableReasons = @()
        build = @{
            rustBuildPerformed = $true
            cargoLocked = $false
            rustTarget = 'x86_64-pc-windows-msvc'
            rustFlags = '-C target-feature=+crt-static'
            imageMagickBundled = $true
            modernMenuBundled = $true
        }
    } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $manifest -Encoding utf8
    Assert-Fails 'unlocked Cargo build recipe is not publishable' {
        & (Join-Path $PSScriptRoot 'check-release-manifest.ps1') `
            -InstallerPath $installer `
            -StagePath $stage `
            -ExpectedVersion '9.8.7' `
            -ExpectedCommitSha $currentHead `
            -ManifestPath $manifest
    }

    # The checker must use the explicit expected architecture, rather than allowing
    # an ARM64-named installer to smuggle an x64 Rust payload through. This reaches
    # the target gate before it touches the deliberately fake installer/stage.
    @{
        schemaVersion = 1
        product = 'SageThumbs 2K'
        version = '9.8.7'
        createdUtc = [DateTime]::UtcNow.ToString('o')
        commitSha = $currentHead
        sourceTreeClean = $true
        publishable = $true
        publishableReasons = @()
        architecture = 'arm64'
        build = @{
            rustBuildPerformed = $true
            cargoLocked = $true
            rustTarget = 'x86_64-pc-windows-msvc'
            rustFlags = '-C target-feature=+crt-static'
            imageMagickBundled = $false
            modernMenuBundled = $true
        }
    } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $manifest -Encoding utf8
    Assert-FailsLike 'ARM64 manifest rejects an x64 Rust target' '*wrong Rust target*' {
        & (Join-Path $PSScriptRoot 'check-release-manifest.ps1') `
            -InstallerPath $installer `
            -StagePath $stage `
            -ExpectedVersion '9.8.7' `
            -ExpectedCommitSha $currentHead `
            -Architecture arm64 `
            -ManifestPath $manifest
    }

    Assert-Fails 'manifest writer rejects corrupt release binaries' {
        foreach ($name in 'sagethumbs2k.dll', 'SageThumbs2K.exe', 'st2k.exe') {
            Copy-Item -LiteralPath $corrupt -Destination (Join-Path $stage $name)
        }
        & (Join-Path $PSScriptRoot 'write-release-manifest.ps1') `
            -InstallerPath $installer `
            -StagePath $stage `
            -Version '9.8.7' `
            -ImageMagickBundled:$true `
            -ModernMenuBundled:$true `
            -RustBuildPerformed:$true `
            -ExeCargoArguments @('--release', '--locked', '-p', 'sagethumbs2k', '--features', 'webp-lossy,html-preview,hdr-capture') `
            -DllCargoArguments @('--release', '--locked', '-p', 'sagethumbs2k-dll', '--features', 'webp-lossy,dll-i18n-subset') `
            -OutputPath $manifest
    }

    Assert-Passes 'manifest scripts pin architecture-specific Cargo targets and installer names' {
        $writerText = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'write-release-manifest.ps1') -Raw
        $checkerText = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'check-release-manifest.ps1') -Raw
        foreach ($script in @($writerText, $checkerText)) {
            if ($script -notmatch "ValidateSet\('x64', 'arm64'\)" -or
                $script -notmatch 'Get-ReleaseCargoBuildArguments' -or
                $script -notmatch 'SageThumbs2K-Setup-\$Version-arm64\.exe|SageThumbs2K-Setup-\$ExpectedVersion-arm64\.exe') {
                throw 'release manifest architecture contract is incomplete'
            }
        }
        if ($checkerText -notmatch '\$sizeCheck\s+-InstallerPath\s+\$installer\.FullName\s+-StagePath\s+\$stage\.FullName\s+-Architecture\s+\$Architecture') {
            throw 'manifest checker does not forward architecture to the size gate'
        }
    }

    Assert-Passes 'build runner and provenance gates share the exact Cargo recipe' {
        . (Join-Path $PSScriptRoot 'release-manifest-lib.ps1')
        $expectedArmExe = @('--release', '--locked', '--target', 'aarch64-pc-windows-msvc', '-p', 'sagethumbs2k', '--features', 'webp-lossy,html-preview,hdr-capture')
        $expectedArmDll = @('--release', '--locked', '--target', 'aarch64-pc-windows-msvc', '-p', 'sagethumbs2k-dll', '--features', 'webp-lossy,dll-i18n-subset')
        $actualArmExe = @(Get-ReleaseCargoBuildArguments -Architecture arm64 -Package sagethumbs2k -Features 'webp-lossy,html-preview,hdr-capture')
        $actualArmDll = @(Get-ReleaseCargoBuildArguments -Architecture arm64 -Package sagethumbs2k-dll -Features 'webp-lossy,dll-i18n-subset')
        if (($actualArmExe -join "`0") -cne ($expectedArmExe -join "`0") -or
            ($actualArmDll -join "`0") -cne ($expectedArmDll -join "`0")) {
            throw 'canonical ARM Cargo arguments changed or are out of order'
        }
        foreach ($name in 'build-release.ps1', 'write-release-manifest.ps1', 'check-release-manifest.ps1') {
            $text = Get-Content -LiteralPath (Join-Path $PSScriptRoot $name) -Raw
            if ($text -notmatch 'Get-ReleaseCargoBuildArguments') {
                throw "$name does not use the canonical Cargo build recipe"
            }
        }
    }

    Write-Host "[release-pipeline-test] ALL GREEN ($script:passed cases)" -ForegroundColor Green
} finally {
    if (Test-Path -LiteralPath $scratch) {
        Remove-Item -LiteralPath $scratch -Recurse -Force
    }
}
