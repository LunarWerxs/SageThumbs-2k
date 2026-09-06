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

# --- F08 + F09 (2026-09-05 audit): modern-menu per-user registration + exact-thumbprint
# certificate cleanup. Pinned here as plain text predicates (not routed through
# check-installer.ps1 - that script polices the payload-cleanup allowlist and the resource-
# safe-form/brace rules, not user-context or certificate-scope semantics) so a future edit to
# the [Run]/[UninstallRun] entries cannot silently regress either fix.
function Test-ModernMenuRegistersAsOriginalUser([string]$Text) {
    # The per-user step is identified by the Add-AppxPackage call it actually makes (not by
    # position), then its OWN Flags value - up to the end of that physical line - must carry
    # runasoriginaluser. The cert-trust step above it deliberately does NOT carry the flag
    # (it is genuinely machine-wide), so this must anchor on the Add-AppxPackage text itself,
    # not merely "the flag appears somewhere in the file" (it does, on unrelated entries).
    $registerBlock = [regex]::Match($Text, 'Add-AppxPackage -Path[\s\S]*?Flags:[^\r\n]*')
    return $registerBlock.Success -and
        $registerBlock.Value.Contains('runasoriginaluser', [StringComparison]::Ordinal)
}
function Test-NoSubjectWildcardCertRemoval([string]$Text) {
    # F09's bug, verbatim: `Get-ChildItem Cert:\...\TrustedPeople | Where-Object Subject -like
    # '*SageThumbs2K*'` removes every certificate with a matching SUBJECT, not just the one
    # this installer trusted - a developer's manual trust or another install's own copy of the
    # same self-signed certificate (different key, different thumbprint) would be deleted too.
    return -not $Text.Contains('Subject -like', [StringComparison]::Ordinal)
}
function Test-ExactThumbprintCertRemoval([string]$Text) {
    # The old version of this check was satisfied by the WORD 'ModernMenuCertThumbprint'
    # appearing anywhere in the file - a comment mentioning the property name alone would
    # pass it. This now requires the [UninstallRun] section to actually (a) read the marker
    # into a variable and (b) build the certificate path to remove from THAT SAME variable
    # (thumbprint equality via path construction - Cert:\LocalMachine\TrustedPeople\<value>),
    # not merely mention the property name in prose.
    # Anchored to a LINE that is exactly "[UninstallRun]" (^, multiline) - the bare
    # "(?s)\[UninstallRun\]" this used to be also matches the phrase inside prose comments
    # (e.g. "see [UninstallRun] below"), which can capture from an unrelated earlier comment
    # all the way to some later section and silently look at the wrong text.
    $section = [regex]::Match($Text, '(?ms)^\[UninstallRun\]\r?\n(.*?)(?=\r?\n\[[A-Za-z]|\z)')
    if (-not $section.Success) { return $false }
    $block = $section.Value
    $read = [regex]::Match($block,
        "\`$(\w+)\s*=\s*\(Get-ItemProperty\s+-Path\s+'HKLM:\\Software\\SageThumbs2K'\s+-Name\s+ModernMenuCertThumbprint[^)]*\)\.ModernMenuCertThumbprint")
    if (-not $read.Success) { return $false }
    $varName = $read.Groups[1].Value
    $removal = [regex]::Match($block, "Remove-Item\s+-Path\s+\('Cert:\\LocalMachine\\TrustedPeople\\'\s*\+\s*\`$$varName\)")
    return $removal.Success
}
function Test-NoWildcardCertMatch([string]$Text) {
    # F09's bug generalised: no certificate-removal command ANYWHERE in the file may match by
    # a wildcard or a `-like` comparison against Subject (or against the TrustedPeople path
    # itself) - the exact shape that deletes a developer's manual trust or another install's
    # copy of the same self-signed certificate under a different key/thumbprint.
    if ($Text.Contains('Subject -like', [StringComparison]::Ordinal)) { return $false }
    if ([regex]::IsMatch($Text, '(?i)TrustedPeople[^\r\n]{0,80}-like')) { return $false }
    if ([regex]::IsMatch($Text, "TrustedPeople\\'\s*\+[^\r\n]*'\*")) { return $false }
    return $true
}
function Test-ModernMenuMigratesPreFixThumbprint([string]$Text) {
    # F09 migration (review pass 2): everybody who upgraded from a build older than this fix
    # already has the certificate trusted with no ModernMenuCertThumbprint marker recorded (the
    # marker did not exist yet). Left alone, the "already trusted" branch would silently skip
    # the import AND never write a marker, so uninstall could never remove a certificate this
    # product genuinely introduced. The cert-trust [Run] step must recognise that case (already
    # trusted, no marker, and this run is an upgrade) and record the marker there.
    $registerBlock = [regex]::Match($Text,
        "(?s)Set-ItemProperty -Path 'HKLM:\\Software\\SageThumbs2K' -Name ModernMenuInstallDir.*?Check: ModernMenuUsable")
    if (-not $registerBlock.Success) { return $false }
    $block = $registerBlock.Value
    if (-not $block.Contains('ST2K_ISUPGRADE', [StringComparison]::Ordinal)) { return $false }
    # The upgrade flag must be tested INSIDE the already-trusted branch (right after the
    # Test-Path(...TrustedPeople...) that is true when the cert needs no import), not merely
    # appear somewhere in the step.
    return [regex]::IsMatch($block,
        "Test-Path\s+\('Cert:\\LocalMachine\\TrustedPeople\\'\+\`$t\)\)\{[^}]*ST2K_ISUPGRADE")
}
function Test-ModernMenuRemovesRotatedThumbprint([string]$Text) {
    # Certificate rotation: make-msix.ps1 mints a fresh self-signed cert on any signing machine
    # that lacks the previous one, so two releases can carry different thumbprints under the
    # same subject. Importing a new thumbprint must remove the PREVIOUSLY recorded one (which
    # this installer itself introduced) before recording the new marker, or the old certificate
    # stays trusted forever with nothing left to ever remove it.
    $registerBlock = [regex]::Match($Text,
        "(?s)Set-ItemProperty -Path 'HKLM:\\Software\\SageThumbs2K' -Name ModernMenuInstallDir.*?Check: ModernMenuUsable")
    if (-not $registerBlock.Success) { return $false }
    $block = $registerBlock.Value
    if (-not [regex]::IsMatch($block, '\$m\s*-ne\s*\$t')) { return $false }
    $removeOld = [regex]::Match($block, "Remove-Item\s+-Path\s+\('Cert:\\LocalMachine\\TrustedPeople\\'\+\`$m\)")
    if (-not $removeOld.Success) { return $false }
    $importIdx = $block.IndexOf('Import-Certificate', [StringComparison]::Ordinal)
    return ($importIdx -gt 0) -and ($removeOld.Index -lt $importIdx)
}
function Test-PackageRemovalIsSynchronousAllUsers([string]$Text) {
    # F08 uninstall fix (review pass 2): package removal must no longer go through the
    # fire-and-forget RunAsOriginalUser scheduled-task helper (asynchronous, and tied to
    # whichever user happens to be signed in) - it must be a SYNCHRONOUS, ELEVATED, -AllUsers
    # removal instead, so it works regardless of who is logged on, and its outcome is known
    # before uninstall proceeds.
    if ($Text -match 'RunAsOriginalUser\([^;]*Remove-AppxPackage') { return $false }
    $proc = [regex]::Match($Text, '(?s)procedure RemoveModernMenuPackageForAllUsers;.*?\r?\nend;')
    if (-not $proc.Success) { return $false }
    $body = $proc.Value
    if (-not [regex]::IsMatch($body, 'Get-AppxPackage\s+-AllUsers[^"]*Remove-AppxPackage\s+-AllUsers')) { return $false }
    if (-not $body.Contains('ewWaitUntilTerminated', [StringComparison]::Ordinal)) { return $false }
    if (-not $body.Contains('Log(', [StringComparison]::Ordinal)) { return $false }
    # And it must actually be CALLED (from CurUninstallStepChanged), not merely declared.
    $callCount = ([regex]::Matches($Text, 'RemoveModernMenuPackageForAllUsers')).Count
    return $callCount -ge 2
}
function Assert-ModernMenuUserContextContract([string]$Text) {
    if (-not (Test-ModernMenuRegistersAsOriginalUser $Text)) {
        throw 'F08: the per-user Add-AppxPackage registration must run as the original ' +
            'interactive user (runasoriginaluser), not whichever administrator answered UAC'
    }
    if (-not (Test-NoSubjectWildcardCertRemoval $Text)) {
        throw 'F09: certificate cleanup must never match by subject wildcard - it can ' +
            'delete a developer''s or another install''s trust in the same-named certificate'
    }
    if (-not (Test-NoWildcardCertMatch $Text)) {
        throw 'F09: no certificate-removal command anywhere may match by a wildcard or ' +
            '-like comparison against Subject or the TrustedPeople path'
    }
    if (-not (Test-ExactThumbprintCertRemoval $Text)) {
        throw 'F09: [UninstallRun] must read ModernMenuCertThumbprint into a variable and ' +
            'remove the certificate at that exact thumbprint path - a mention of the ' +
            'property name alone (e.g. in a comment) is not enough'
    }
    if (-not (Test-ModernMenuMigratesPreFixThumbprint $Text)) {
        throw 'F09 migration: an upgrade whose certificate is already trusted but has no ' +
            'recorded marker must record this thumbprint as ours (ST2K_ISUPGRADE gate ' +
            'inside the already-trusted branch), or uninstall can never remove it'
    }
    if (-not (Test-ModernMenuRemovesRotatedThumbprint $Text)) {
        throw 'F09 rotation: importing a certificate whose thumbprint differs from the ' +
            'recorded marker must remove the previously recorded thumbprint from ' +
            'TrustedPeople before recording the new one'
    }
    if (-not (Test-PackageRemovalIsSynchronousAllUsers $Text)) {
        throw 'F08 uninstall: package removal must be a synchronous, elevated, -AllUsers ' +
            'Get-AppxPackage | Remove-AppxPackage call (RemoveModernMenuPackageForAllUsers), ' +
            'not the RunAsOriginalUser scheduled-task helper'
    }
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

    Assert-ModernMenuUserContextContract $source
    Write-Host '  PASS  modern-menu per-user registration + exact-thumbprint cert cleanup' -ForegroundColor Green
    $script:passed++

    # Teeth proof, one mutation per pinned property: each one individually reverts the real
    # fixed source back to the exact pre-fix shape (2026-09-05 audit) for JUST that property,
    # confirming the check would have caught it, then confirms the OTHER two properties still
    # hold in that same mutated text (so a single mutation cannot be masked by the others).
    $noRunAsUser = $source.Replace(
        '  Flags: runhidden waituntilterminated runasoriginaluser; Check: ModernMenuUsable',
        '  Flags: runhidden waituntilterminated; Check: ModernMenuUsable'
    )
    if ($noRunAsUser -ceq $source) { throw 'test mutation did not remove runasoriginaluser from the registration entry' }
    if (Test-ModernMenuRegistersAsOriginalUser $noRunAsUser) {
        throw 'expected F08 teeth proof to fail: registration entry no longer has runasoriginaluser'
    }
    if (-not (Test-NoSubjectWildcardCertRemoval $noRunAsUser)) { throw 'F09 subject-wildcard check should still pass here' }
    if (-not (Test-ExactThumbprintCertRemoval $noRunAsUser)) { throw 'F09 exact-thumbprint check should still pass here' }
    Write-Host '  PASS  F08 teeth proof: registration missing runasoriginaluser is caught' -ForegroundColor Green
    $script:passed++

    $subjectWildcard = $source.Replace(
        "`$t=(Get-ItemProperty -Path 'HKLM:\Software\SageThumbs2K' -Name ModernMenuCertThumbprint -ErrorAction SilentlyContinue).ModernMenuCertThumbprint; if(`$t){{Remove-Item -Path ('Cert:\LocalMachine\TrustedPeople\'+`$t) -Force -ErrorAction SilentlyContinue}",
        "Get-ChildItem Cert:\LocalMachine\TrustedPeople | Where-Object Subject -like '*SageThumbs2K*' | Remove-Item -Force"
    )
    if ($subjectWildcard -ceq $source) { throw 'test mutation did not reintroduce the subject-wildcard removal' }
    if (Test-NoSubjectWildcardCertRemoval $subjectWildcard) {
        throw 'expected F09 teeth proof to fail: subject-wildcard removal was reintroduced'
    }
    if (-not (Test-ModernMenuRegistersAsOriginalUser $subjectWildcard)) { throw 'F08 runasoriginaluser check should still pass here' }
    Write-Host '  PASS  F09 teeth proof: subject-wildcard cert removal is caught' -ForegroundColor Green
    $script:passed++

    $noThumbprintTracking = $source.Replace('ModernMenuCertThumbprint', 'DiscardedForTest')
    if ($noThumbprintTracking -ceq $source) { throw 'test mutation did not remove ModernMenuCertThumbprint tracking' }
    if (Test-ExactThumbprintCertRemoval $noThumbprintTracking) {
        throw 'expected F09 teeth proof to fail: exact-thumbprint provenance tracking is gone'
    }
    if (-not (Test-ModernMenuRegistersAsOriginalUser $noThumbprintTracking)) { throw 'F08 runasoriginaluser check should still pass here' }
    if (-not (Test-NoSubjectWildcardCertRemoval $noThumbprintTracking)) { throw 'F09 subject-wildcard check should still pass here' }
    Write-Host '  PASS  F09 teeth proof: missing exact-thumbprint tracking is caught' -ForegroundColor Green
    $script:passed++

    # --- F09 migration (review pass 2): an upgrade whose certificate is already trusted but
    # carries no marker yet must record it. Mutation drops the ST2K_ISUPGRADE gate from the
    # already-trusted branch's condition, reverting to "never write a marker for an
    # already-trusted cert" - the exact shape that leaks a pre-fix installation's certificate
    # forever, undetectable by any of the OTHER checks above.
    $noMigration = $source.Replace(
        "if((-not `$m) -and (`$env:ST2K_ISUPGRADE -eq '1')){{Set-ItemProperty -Path 'HKLM:\Software\SageThumbs2K' -Name ModernMenuCertThumbprint -Value `$t}}else{{",
        "if((-not `$m)){{Set-ItemProperty -Path 'HKLM:\Software\SageThumbs2K' -Name ModernMenuCertThumbprint -Value `$t}}else{{"
    )
    if ($noMigration -ceq $source) { throw 'test mutation did not remove the ST2K_ISUPGRADE migration gate' }
    if (Test-ModernMenuMigratesPreFixThumbprint $noMigration) {
        throw 'expected F09 migration teeth proof to fail: ST2K_ISUPGRADE gate is gone'
    }
    if (-not (Test-ModernMenuRegistersAsOriginalUser $noMigration)) { throw 'F08 runasoriginaluser check should still pass here' }
    if (-not (Test-NoSubjectWildcardCertRemoval $noMigration)) { throw 'F09 subject-wildcard check should still pass here' }
    if (-not (Test-NoWildcardCertMatch $noMigration)) { throw 'F09 no-wildcard check should still pass here' }
    if (-not (Test-ExactThumbprintCertRemoval $noMigration)) { throw 'F09 exact-thumbprint removal check should still pass here' }
    if (-not (Test-ModernMenuRemovesRotatedThumbprint $noMigration)) { throw 'F09 rotation check should still pass here' }
    if (-not (Test-PackageRemovalIsSynchronousAllUsers $noMigration)) { throw 'F08 AllUsers-removal check should still pass here' }
    Write-Host '  PASS  F09 migration teeth proof: missing upgrade-gated marker recording is caught' -ForegroundColor Green
    $script:passed++

    # --- F09 rotation: a new signing-machine thumbprint must remove the PREVIOUSLY recorded
    # certificate before importing the new one. Mutation drops that whole cleanup conditional,
    # reverting to "just import" - the shape that leaves an orphaned rotated certificate
    # trusted forever with no marker left pointing at it.
    $noRotationCleanup = $source.Replace(
        "if(`$m -and (`$m -ne `$t)){{Remove-Item -Path ('Cert:\LocalMachine\TrustedPeople\'+`$m) -Force -ErrorAction SilentlyContinue}; Import-Certificate",
        'Import-Certificate'
    )
    if ($noRotationCleanup -ceq $source) { throw 'test mutation did not remove the rotation cleanup conditional' }
    if (Test-ModernMenuRemovesRotatedThumbprint $noRotationCleanup) {
        throw 'expected F09 rotation teeth proof to fail: old-thumbprint cleanup is gone'
    }
    if (-not (Test-ModernMenuRegistersAsOriginalUser $noRotationCleanup)) { throw 'F08 runasoriginaluser check should still pass here' }
    if (-not (Test-NoSubjectWildcardCertRemoval $noRotationCleanup)) { throw 'F09 subject-wildcard check should still pass here' }
    if (-not (Test-NoWildcardCertMatch $noRotationCleanup)) { throw 'F09 no-wildcard check should still pass here' }
    if (-not (Test-ExactThumbprintCertRemoval $noRotationCleanup)) { throw 'F09 exact-thumbprint removal check should still pass here' }
    if (-not (Test-ModernMenuMigratesPreFixThumbprint $noRotationCleanup)) { throw 'F09 migration check should still pass here' }
    if (-not (Test-PackageRemovalIsSynchronousAllUsers $noRotationCleanup)) { throw 'F08 AllUsers-removal check should still pass here' }
    Write-Host '  PASS  F09 rotation teeth proof: missing orphaned-certificate cleanup is caught' -ForegroundColor Green
    $script:passed++

    # --- F08 uninstall (review pass 2): package removal must be synchronous, elevated and
    # -AllUsers, not the old fire-and-forget RunAsOriginalUser scheduled-task helper. Mutation
    # reverts the call site back to that exact pre-fix shape.
    $noAllUsersRemoval = $source.Replace(
        "    RemoveModernMenuPackageForAllUsers;",
        "    RunAsOriginalUser(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'), " +
            "'-NoProfile -Command Get-AppxPackage -Name SageThumbs2K | Remove-AppxPackage -ErrorAction SilentlyContinue');"
    )
    if ($noAllUsersRemoval -ceq $source) { throw 'test mutation did not revert the package-removal call site' }
    if (Test-PackageRemovalIsSynchronousAllUsers $noAllUsersRemoval) {
        throw 'expected F08 AllUsers-removal teeth proof to fail: call site reverted to RunAsOriginalUser'
    }
    if (-not (Test-ModernMenuRegistersAsOriginalUser $noAllUsersRemoval)) { throw 'F08 runasoriginaluser check should still pass here' }
    if (-not (Test-NoSubjectWildcardCertRemoval $noAllUsersRemoval)) { throw 'F09 subject-wildcard check should still pass here' }
    if (-not (Test-NoWildcardCertMatch $noAllUsersRemoval)) { throw 'F09 no-wildcard check should still pass here' }
    if (-not (Test-ExactThumbprintCertRemoval $noAllUsersRemoval)) { throw 'F09 exact-thumbprint removal check should still pass here' }
    if (-not (Test-ModernMenuMigratesPreFixThumbprint $noAllUsersRemoval)) { throw 'F09 migration check should still pass here' }
    if (-not (Test-ModernMenuRemovesRotatedThumbprint $noAllUsersRemoval)) { throw 'F09 rotation check should still pass here' }
    Write-Host '  PASS  F08 AllUsers-removal teeth proof: reverted RunAsOriginalUser call site is caught' -ForegroundColor Green
    $script:passed++

    # --- Baseline proof: every assertion above (old and new) must fail against main's
    # pre-audit installer.iss, which has none of F08/F09 at all, and pass against ours. This
    # is the requested "prove it against main" check, run against the real git history rather
    # than a hand-written mutation.
    $mainSource = & git show main:scripts/packaging/installer.iss 2>$null
    if ($LASTEXITCODE -ne 0 -or -not $mainSource) {
        throw 'could not read scripts/packaging/installer.iss from main for the baseline proof'
    }
    $mainSource = $mainSource -join "`r`n"
    $mainFailed = $false
    try { Assert-ModernMenuUserContextContract $mainSource } catch { $mainFailed = $true }
    if (-not $mainFailed) {
        throw 'expected the full F08/F09 contract to fail against main (pre-audit) installer.iss'
    }
    Write-Host '  PASS  F08/F09 contract fails against main (pre-audit) installer.iss' -ForegroundColor Green
    $script:passed++

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

    # A PowerShell block in a Parameters: value with BARE braces. Inno reads '{' as the start of
    # one of its own constants, so ISCC aborts the whole compile with "Unknown constant" - which
    # is exactly what the 2026-08-22 registration rewrite did, and nothing caught it until a full
    # release build four minutes in. The mutation writes the real failure, not an invented one.
    $bareBraces = Join-Path $scratch 'bare-braces.iss'
    Set-Content -LiteralPath $bareBraces -Value (
        $source + "`r`n[Run]`r`n" +
        'Filename: "powershell.exe"; Parameters: "-NoProfile -Command ' +
        '""try{Add-AppxPackage -Path ''{app}\x.msix''}catch{Write-Host bad}"""' + "`r`n"
    ) -Encoding utf8
    Assert-LintFails 'unescaped PowerShell braces in a Parameters value' {
        Invoke-InstallerLint -IssPath $bareBraces
    }

    # And the correct spelling must PASS, or the rule would just ban shell commands outright.
    $escapedBraces = Join-Path $scratch 'escaped-braces.iss'
    Set-Content -LiteralPath $escapedBraces -Value (
        $source + "`r`n[Run]`r`n" +
        'Filename: "powershell.exe"; Parameters: "-NoProfile -Command ' +
        '""try{{Add-AppxPackage -Path ''{app}\x.msix''}catch{{Write-Host ok}"""' + "`r`n"
    ) -Encoding utf8
    Assert-LintPasses 'correctly escaped braces beside a real {app} constant' {
        Invoke-InstallerLint -IssPath $escapedBraces
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
