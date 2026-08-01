<#
  Dependency-free fail-closed tests for release provenance and curated notes.
  These tests intentionally do not need a built installer.
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'release-manifest-lib.ps1')

$scratch = Join-Path ([IO.Path]::GetTempPath()) ("st2k-release-pipeline-" + [guid]::NewGuid().ToString('N'))
$script:passed = 0

function Assert-Passes([string]$Name, [scriptblock]$Body) {
    try {
        & $Body *> $null
        Write-Host "  PASS  $Name" -ForegroundColor Green
        $script:passed++
    } catch {
        throw "expected PASS for '$Name', got: $($_.Exception.Message)"
    }
}

function Assert-Fails([string]$Name, [scriptblock]$Body) {
    $failed = $false
    try { & $Body *> $null } catch { $failed = $true }
    if (-not $failed) { throw "expected FAILURE for '$Name'" }
    Write-Host "  PASS  $Name (failed closed)" -ForegroundColor Green
    $script:passed++
}

function Assert-FailsLike([string]$Name, [string]$MessagePattern, [scriptblock]$Body) {
    try {
        & $Body *> $null
    } catch {
        if ($_.Exception.Message -notlike $MessagePattern) {
            throw "expected '$Name' to fail like '$MessagePattern', got: $($_.Exception.Message)"
        }
        Write-Host "  PASS  $Name (failed at the intended gate)" -ForegroundColor Green
        $script:passed++
        return
    }
    throw "expected FAILURE for '$Name'"
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
                'packaging/make-msix.ps1',
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

    $corrupt = Join-Path $scratch 'corrupt.exe'
    [IO.File]::WriteAllBytes($corrupt, [byte[]](0x4D, 0x5A, 0, 0))
    Assert-Fails 'truncated MZ file is not accepted as a PE' {
        Assert-ReleasePeFile -Path $corrupt
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

    # The shipping build recipe is written down in THREE scripts: build-release.ps1 runs
    # it, write-release-manifest.ps1 pins what it expects to have been run, and
    # check-release-manifest.ps1 pins what it will accept. That redundancy is deliberate
    # (the gate must be able to refuse a build that silently gained or lost a feature),
    # but nothing made the three agree, so they drifted: `hdr-capture` was added to two
    # of them and the 1.5.0 release died at the publish gate after a full build, ~20
    # minutes in. Catch it here instead, in milliseconds, before anything is built.
    Assert-Passes 'the three copies of the build recipe agree' {
        $recipeFiles = @{
            'build-release.ps1'         = ''
            'write-release-manifest.ps1' = ''
            'check-release-manifest.ps1' = ''
        }
        $seen = @{}
        foreach ($name in @($recipeFiles.Keys)) {
            $text = Get-Content -LiteralPath (Join-Path $PSScriptRoot $name) -Raw
            $recipeFiles[$name] = $text
            # Matches the '-p', '<package>', '--features', '<features>' run in all three
            # files, including the multi-line array form in write-release-manifest.ps1
            # and any `#` comment lines sitting between the two (there is one, explaining
            # why hdr-capture is on the EXE recipe and not the DLL's).
            $found = [regex]::Matches($text, "'-p',\s*'([^']+)',(?:\s*#[^\r\n]*)*\s*'--features',\s*'([^']+)'")
            if ($found.Count -lt 2) {
                throw "$name declares $($found.Count) package recipes, expected 2 (exe + dll)"
            }
            foreach ($m in $found) {
                $pkg = $m.Groups[1].Value
                $feat = $m.Groups[2].Value
                if (-not $seen.ContainsKey($pkg)) { $seen[$pkg] = @{} }
                $seen[$pkg][$name] = $feat
            }
        }
        foreach ($pkg in $seen.Keys) {
            $distinct = @($seen[$pkg].Values | Sort-Object -Unique)
            if ($distinct.Count -ne 1) {
                $detail = ($seen[$pkg].GetEnumerator() | ForEach-Object { "$($_.Key)='$($_.Value)'" }) -join '; '
                throw "build recipe for '$pkg' differs between scripts: $detail"
            }
            # check-release-manifest.ps1 also pins the feature LIST separately from the
            # argument line. That second copy is exactly what went stale in 1.5.0, so
            # assert it matches the argument line it sits beside.
            $expectedArray = "@('" + (($distinct[0] -split ',') -join "', '") + "')"
            if ($recipeFiles['check-release-manifest.ps1'] -notlike "*$expectedArray*") {
                throw "check-release-manifest.ps1 has no feature-list expectation $expectedArray for '$pkg'"
            }
        }
    }

    Write-Host "[release-pipeline-test] ALL GREEN ($script:passed cases)" -ForegroundColor Green
} finally {
    if (Test-Path -LiteralPath $scratch) {
        Remove-Item -LiteralPath $scratch -Recurse -Force
    }
}
