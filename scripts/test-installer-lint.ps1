<#
  Dependency-free regression tests for check-installer.ps1. These exercise the
  exact real installer plus mutations that must fail closed.
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$lint = Join-Path $PSScriptRoot 'check-installer.ps1'
$installer = Join-Path $root 'scripts\packaging\installer.iss'
$scratch = Join-Path (
    [IO.Path]::GetTempPath()
) ("st2k-installer-lint-" + [guid]::NewGuid().ToString('N'))
$script:passed = 0

function Invoke-InstallerLint {
    param(
        [Parameter(Mandatory)]
        [string]$IssPath,

        [string]$ManagedPayloadPath,

        [string]$CorePolicyPath
    )

    $arguments = @('-NoProfile', '-File', $lint, '-IssPath', $IssPath)
    if ($ManagedPayloadPath) {
        $arguments += @('-ManagedPayloadPath', $ManagedPayloadPath)
    }
    if ($CorePolicyPath) {
        $arguments += @('-CorePolicyPath', $CorePolicyPath)
    }
    & pwsh @arguments *> $null
    return $LASTEXITCODE
}

function Assert-LintPasses([string]$Name, [scriptblock]$Body) {
    $code = & $Body
    if ($code -ne 0) {
        throw "expected installer lint PASS for '$Name', got exit $code"
    }
    Write-Host "  PASS  $Name" -ForegroundColor Green
    $script:passed++
}

function Assert-LintFails([string]$Name, [scriptblock]$Body) {
    $code = & $Body
    if ($code -eq 0) {
        throw "expected installer lint FAILURE for '$Name'"
    }
    Write-Host "  PASS  $Name (failed closed)" -ForegroundColor Green
    $script:passed++
}

function Assert-ReleaseArchitectureContract([string]$Text) {
    $lines = $Text -split "\r?\n"
    foreach ($definition in @(
            '#define Architecture "x64"',
            '#define StageDir "stage"',
            '#define CompactOnly "0"',
            '#define OutputSuffix ""',
            '#define ArchitectureMatcher "x64compatible and not arm64"',
            '#define ArchitectureMatcher "arm64"'
        )) {
        if (-not $Text.Contains($definition, [StringComparison]::Ordinal)) {
            throw "missing architecture preprocessor contract: $definition"
        }
    }
    foreach ($entry in @(
            'AppId={{B0A1C2D3-E4F5-4607-8899-AABBCCDDEEFF}',
            'DefaultDirName={autopf}\SageThumbs2K',
            'UsePreviousAppDir=yes',
            'ArchitecturesAllowed={#ArchitectureMatcher}',
            'ArchitecturesInstallIn64BitMode={#ArchitectureMatcher}',
            'SetupIconFile={#StageDir}\app.ico',
            'OutputBaseFilename=SageThumbs2K-Setup-{#AppVer}{#OutputSuffix}'
        )) {
        if (@($lines | Where-Object { $_ -ceq $entry }).Count -ne 1) {
            throw "expected exactly one architecture-aware installer entry: $entry"
        }
    }
    if ($Text.Contains('SageThumbs2K-arm64', [StringComparison]::Ordinal)) {
        throw 'release installers must share one application directory; the ARM64 suffix is dev-only'
    }
    if ($Text.Contains('UsePreviousAppDir=no', [StringComparison]::Ordinal)) {
        throw 'release installers must reuse the prior architecture installation directory'
    }
    if ($Text.Contains('Source: "stage\', [StringComparison]::Ordinal)) {
        throw 'installer contains a hard-coded stage source instead of {#StageDir}'
    }
    # The Compact PRODUCT TIER was removed (2026-08-12): every install now carries the full
    # ImageMagick payload. `CompactOnly` survives only as an internal build switch for CI
    # jobs that skip staging the engine, so exactly ONE block stays guarded — the engine
    # source row. More than one means a user-facing tier is creeping back.
    $engineGuard = '#if CompactOnly == "0"'
    if (@([regex]::Matches($Text, [regex]::Escape($engineGuard))).Count -ne 1) {
        throw 'expected exactly one CompactOnly guard (the ImageMagick source row)'
    }
    if ($Text.Contains('(Architecture == "x64") && (CompactOnly', [StringComparison]::Ordinal)) {
        throw 'ImageMagick staging must not be architecture-gated; both architectures bundle the engine'
    }
    # No install-type or component SELECTION may return. Both sections being absent is what
    # keeps Inno from rendering a components page, and it is the whole point of the removal:
    # "all N formats" must never again depend on a checkbox the user did not understand.
    # Section headers must be matched as WHOLE LINES, not substrings: the .iss carries a
    # comment explaining why these sections are absent, and that comment names them.
    foreach ($section in @('[Types]', '[Components]')) {
        if ($lines | Where-Object { $_.Trim() -ceq $section }) {
            throw "the Compact/Full install choice must not come back (found a $section section)"
        }
    }
    foreach ($banned in @('Types: full', 'Types: compact', 'Components: magick')) {
        if ($Text.Contains($banned, [StringComparison]::Ordinal)) {
            throw "the Compact/Full install choice must not come back (found '$banned')"
        }
    }
}

function Assert-ArchitectureContractFails([string]$Name, [string]$Text) {
    $failed = $false
    try {
        Assert-ReleaseArchitectureContract $Text
    } catch {
        $failed = $true
    }
    if (-not $failed) {
        throw "expected architecture contract failure for '$Name'"
    }
    Write-Host "  PASS  $Name (failed closed)" -ForegroundColor Green
    $script:passed++
}

New-Item -ItemType Directory -Path $scratch | Out-Null
try {
    $source = Get-Content -LiteralPath $installer -Raw

    # The architecture variants are preprocessor-only: keep this regression test
    # dependency-free even on developer boxes without ISCC installed. The release
    # pipeline compiles the selected variant; these assertions make the variant
    # contract fail closed before that expensive step.
    Assert-ReleaseArchitectureContract $source
    Write-Host '  PASS  architecture-specific installer contract' -ForegroundColor Green
    $script:passed++

    Assert-ArchitectureContractFails 'architecture-specific AppId' (
        $source.Replace(
            'AppId={{B0A1C2D3-E4F5-4607-8899-AABBCCDDEEFF}',
            'AppId={{A0A1C2D3-E4F5-4607-8899-AABBCCDDEEFF}'
        )
    )
    Assert-ArchitectureContractFails 'ARM-suffixed release directory' (
        $source.Replace(
            'DefaultDirName={autopf}\SageThumbs2K',
            'DefaultDirName={autopf}\SageThumbs2K-arm64'
        )
    )
    Assert-ArchitectureContractFails 'disabled previous-directory reuse' (
        $source.Replace('UsePreviousAppDir=yes', 'UsePreviousAppDir=no')
    )
    Assert-ArchitectureContractFails 'x64 installer allowed on ARM64' (
        $source.Replace(
            '#define ArchitectureMatcher "x64compatible and not arm64"',
            '#define ArchitectureMatcher "x64compatible"'
        )
    )

    Assert-LintPasses 'real installer exact cleanup allowlist' {
        Invoke-InstallerLint -IssPath $installer
    }

    $payload = Join-Path $scratch 'managed-payload'
    New-Item -ItemType Directory -Path (Join-Path $payload 'modules') -Force | Out-Null
    foreach ($name in @(
            'magick.exe',
            'CORE_RL_test_.dll',
            'mfc140u.dll',
            'msvcp140.dll',
            'vcomp140.dll',
            'vcruntime140_1.dll',
            'colors.xml',
            'configure.xml',
            'delegates.xml',
            'english.xml',
            'locale.xml',
            'log.xml',
            'mime.xml',
            'policy.xml',
            'thresholds.xml',
            'type-ghostscript.xml',
            'type.xml',
            'License.txt',
            'NOTICE.txt'
        )) {
        [IO.File]::WriteAllBytes((Join-Path $payload $name), [byte[]](1))
    }
    $corePolicy = Join-Path $scratch 'core-policy.xml'
    Copy-Item -LiteralPath (Join-Path $payload 'policy.xml') -Destination $corePolicy
    Assert-LintPasses 'staged payload coverage and identical core policy' {
        Invoke-InstallerLint `
            -IssPath $installer `
            -ManagedPayloadPath $payload `
            -CorePolicyPath $corePolicy
    }

    $missing = Join-Path $scratch 'missing-cleanup.iss'
    $needle = 'Type: files; Name: "{app}\policy.xml"'
    $mutated = $source.Replace($needle, '')
    if ($mutated -ceq $source) { throw 'test mutation did not remove policy.xml cleanup' }
    Set-Content -LiteralPath $missing -Value $mutated -Encoding utf8
    Assert-LintFails 'missing managed cleanup entry' {
        Invoke-InstallerLint -IssPath $missing
    }

    $broad = Join-Path $scratch 'broad-cleanup.iss'
    $mutated = $source.Replace(
        '[Files]',
        "Type: filesandordirs; Name: `"{app}\*`"`r`n`r`n[Files]"
    )
    if ($mutated -ceq $source) { throw 'test mutation did not add broad cleanup' }
    Set-Content -LiteralPath $broad -Value $mutated -Encoding utf8
    Assert-LintFails 'broad application-directory cleanup entry' {
        Invoke-InstallerLint -IssPath $broad
    }

    $missingCorePolicy = Join-Path $scratch 'missing-core-policy.iss'
    $needle =
        'Source: "{#StageDir}\policy.xml"; DestDir: "{app}"; Flags: ignoreversion'
    $mutated = $source.Replace($needle, '')
    if ($mutated -ceq $source) { throw 'test mutation did not remove core policy mapping' }
    Set-Content -LiteralPath $missingCorePolicy -Value $mutated -Encoding utf8
    Assert-LintFails 'hardened policy mapping removed' {
        Invoke-InstallerLint -IssPath $missingCorePolicy
    }

    $duplicatePolicy = Join-Path $scratch 'duplicate-policy.iss'
    $needle =
        'Source: "{#StageDir}\magick\*"; DestDir: "{app}"; Excludes: "policy.xml"; Flags: ignoreversion recursesubdirs createallsubdirs'
    $replacement =
        'Source: "{#StageDir}\magick\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs'
    $mutated = $source.Replace($needle, $replacement)
    if ($mutated -ceq $source) { throw 'test mutation did not remove policy exclusion' }
    Set-Content -LiteralPath $duplicatePolicy -Value $mutated -Encoding utf8
    Assert-LintFails 'bundled Magick row no longer excludes duplicate policy' {
        Invoke-InstallerLint -IssPath $duplicatePolicy
    }

    $unexpected = Join-Path $payload 'unexpected-third-party.dat'
    [IO.File]::WriteAllBytes($unexpected, [byte[]](1))
    Assert-LintFails 'staged basename outside cleanup allowlist' {
        Invoke-InstallerLint `
            -IssPath $installer `
            -ManagedPayloadPath $payload `
            -CorePolicyPath $corePolicy
    }
    Remove-Item -LiteralPath $unexpected -Force

    [IO.File]::WriteAllBytes($corePolicy, [byte[]](2))
    Assert-LintFails 'core and bundled hardened policies diverge' {
        Invoke-InstallerLint `
            -IssPath $installer `
            -ManagedPayloadPath $payload `
            -CorePolicyPath $corePolicy
    }

    $unsafeForm = Join-Path $scratch 'unsafe-form.iss'
    Set-Content -LiteralPath $unsafeForm -Value (
        $source + "`r`nprocedure LintRegression;`r`nbegin`r`n" +
        "  F := TSetupForm.Create(nil);`r`nend;`r`n"
    ) -Encoding utf8
    Assert-LintFails 'resource-dependent uninstaller form constructor' {
        Invoke-InstallerLint -IssPath $unsafeForm
    }

    Write-Host "[installer-lint-test] ALL GREEN ($script:passed cases)" -ForegroundColor Green
} finally {
    if (Test-Path -LiteralPath $scratch) {
        Remove-Item -LiteralPath $scratch -Recurse -Force
    }
}

# Assert-LintFails deliberately ends by running a native command that must fail. GitHub's pwsh
# step observes that expected command's LASTEXITCODE even though all assertions passed, so make the
# test harness's successful result explicit.
exit 0
