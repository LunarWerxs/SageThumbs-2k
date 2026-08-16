<#
  release.ps1 - a GATED release: it never creates a release/tag until CI is GREEN
  on that exact commit and the full artifact provenance gate passes. The GitHub
  release starts as a draft; it becomes public only after the uploaded installer
  and provenance-manifest digests match the locally validated bytes.

  Prereqs: the version is already bumped in Cargo.toml and the release commit is on `main`
  (committed, not pushed). Run from anywhere:  pwsh scripts\release.ps1

  Flow:  curated-notes + consistency check  ->  clean-main guard  ->  push
         ->  WAIT for CI green  ->  build + provenance-validate every installer whose
             size reference is calibrated in scripts\packaging\size-budget.json (x64 always;
             ARM64 only once its first installer has been recorded there)
         ->  create a draft, verify the uploaded digest, publish -> winget.

  -SkipBuild is safe only after a full build of this exact clean commit: the
  ignored installer, stage, and provenance manifest are all re-hashed before use.
#>
[CmdletBinding()]
param([switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'release-manifest-lib.ps1')
Push-Location $root
try {
    $ver = ([regex]::Match((Get-Content "$root\Cargo.toml" -Raw), '(?m)^\s*version\s*=\s*"([^"]+)"')).Groups[1].Value
    if (-not $ver) { throw "could not read version from Cargo.toml" }
    $tag = "v$ver"
    Write-Host "== Releasing $tag ==" -ForegroundColor Cyan
    # A release ships one artifact per architecture whose installer size reference is
    # CALIBRATED in scripts\packaging\size-budget.json.  Keep the candidate table explicit:
    # selecting a "newest" setup from dist would let an old or wrong-architecture
    # installer become a release asset.
    #
    # An architecture is filtered out until its first installer has been built and recorded
    # in that policy (ARM64 was, on 2026-08-01).  check-release-size.ps1 refuses an
    # uncalibrated reference by design, so keeping ARM64 in the table unconditionally only
    # aborts the run at step [4/6] - AFTER main is pushed and CI is already green, leaving
    # a pushed, CI-green, untagged commit and no published release.  Calibrating the policy
    # turns this leg back on with no edit here.
    $candidateArtifacts = @(
        [pscustomobject]@{
            Architecture = 'x64'
            SetupPath    = Join-Path $root "dist\SageThumbs2K-Setup-$ver.exe"
            ManifestPath = Join-Path $root "dist\SageThumbs2K-Setup-$ver.release.json"
            StagePath    = Join-Path $root 'scripts\packaging\stage\x64'
            PortablePath = Join-Path $root "dist\SageThumbs2K-Portable-$ver.zip"
            Setup        = $null
            Manifest     = $null
            Portable     = $null
        }
        [pscustomobject]@{
            Architecture = 'arm64'
            SetupPath    = Join-Path $root "dist\SageThumbs2K-Setup-$ver-arm64.exe"
            ManifestPath = Join-Path $root "dist\SageThumbs2K-Setup-$ver-arm64.release.json"
            StagePath    = Join-Path $root 'scripts\packaging\stage\arm64'
            PortablePath = Join-Path $root "dist\SageThumbs2K-Portable-$ver-arm64.zip"
            Setup        = $null
            Manifest     = $null
            Portable     = $null
        }
    )

    $sizePolicyPath = Join-Path $root 'scripts\packaging\size-budget.json'
    try { $sizePolicy = Get-Content -LiteralPath $sizePolicyPath -Raw | ConvertFrom-Json }
    catch { throw "release size policy is not valid JSON: $sizePolicyPath`n$($_.Exception.Message)" }
    function Test-InstallerReferenceCalibrated([string]$Architecture) {
        # Mirrors check-release-size.ps1's profile selection so the two cannot disagree
        # about which profile a given architecture ships.
        # Both architectures are Full now; keep this identical to check-release-size.ps1.
        $profileName = 'full'
        $architecturePolicy = $sizePolicy.architectures.PSObject.Properties[$Architecture]
        if ($null -eq $architecturePolicy -or $null -eq $architecturePolicy.Value) {
            throw "size policy has no '$Architecture' architecture policy: $sizePolicyPath"
        }
        $profilePolicy = $architecturePolicy.Value.PSObject.Properties[$profileName]
        if ($null -eq $profilePolicy -or $null -eq $profilePolicy.Value) {
            throw "size policy has no '$Architecture/$profileName' profile: $sizePolicyPath"
        }
        $calibrated = $profilePolicy.Value.PSObject.Properties['installerReferenceCalibrated']
        if ($null -eq $calibrated -or $calibrated.Value -isnot [bool]) {
            throw "size policy '$Architecture/$profileName' needs a boolean installerReferenceCalibrated"
        }
        return [bool]$calibrated.Value
    }

    $releaseArtifacts = @()
    $skippedArchitectures = @()
    foreach ($candidate in $candidateArtifacts) {
        if (Test-InstallerReferenceCalibrated $candidate.Architecture) {
            $releaseArtifacts += $candidate
        } else {
            $skippedArchitectures += $candidate.Architecture
        }
    }
    # x64 is the established primary installer; a release without it is never correct.
    if (@($releaseArtifacts | Where-Object Architecture -ceq 'x64').Count -ne 1) {
        throw "x64 installer size reference is not calibrated in $sizePolicyPath - refusing to release"
    }
    Write-Host "   architectures in this release: $($releaseArtifacts.Architecture -join ', ')" -ForegroundColor Cyan
    foreach ($architecture in $skippedArchitectures) {
        Write-Host "   NOT in this release: $architecture - its installer size reference is uncalibrated in scripts\packaging\size-budget.json." -ForegroundColor Yellow
    }

    # 0) Curated notes + consistency. The release body is derived from this exact
    # tracked changelog section; there is deliberately no generated-notes fallback.
    Write-Host "[1/6] curated notes + consistency check" -ForegroundColor Green
    $changelog = Join-Path $root 'docs\CHANGELOG.md'
    $null = Get-ReleaseChangelogSection -ChangelogPath $changelog -Version $ver
    pwsh "$root\scripts\check-consistency.ps1"; if ($LASTEXITCODE) { throw "consistency check failed - fix before releasing" }
    # The pre-release issue review (CLAUDE.md 6.2). Informational, never a gate - see the
    # header of check-issues.ps1 for why gating would be wrong. It prints the OPEN issues AND,
    # crucially, the CLOSED ones commented since the last release: v2.1.0 shipped while a long
    # follow-up sat unread on closed issue #26, which `gh issue list --state open` cannot show.
    pwsh "$root\scripts\check-issues.ps1"

    # 1) must be on main with a clean tree (so we release exactly what's committed).
    Write-Host "[2/6] clean-tree + branch guard" -ForegroundColor Green
    $branch = (git rev-parse --abbrev-ref HEAD).Trim()
    if ($branch -ne 'main') { throw "not on main (on '$branch') - release from main" }
    if (git status --porcelain) { throw "working tree is dirty - commit or stash before releasing" }

    # 2) refuse to clobber an existing tag (bump the version instead).
    if (git ls-remote --tags origin "refs/tags/$tag") { throw "$tag already exists on origin - bump the version in Cargo.toml" }

    # 3) push, then WAIT for CI to go GREEN on this exact commit before doing anything irreversible.
    $sha = (git rev-parse HEAD).Trim()
    Write-Host "[3/6] push main + wait for CI on $($sha.Substring(0,7))" -ForegroundColor Green
    git push origin main; if ($LASTEXITCODE) { throw "git push failed" }
    # Find the CI run for THIS exact commit. It usually registers in seconds, but under
    # Actions load (e.g. a prior push's run still queued) it can lag minutes — so poll for up
    # to 12 min (the old 6-min window aborted the 0.8.0 release when a prior run was busy).
    # `--limit 30` guards against the target being pushed past the default page of 20.
    # CRITICAL: `--json headSha,databaseId` must have NO space after the comma. With a space,
    # PowerShell splits it into two native args and gh dies with `unknown command "databaseId"`
    # — which `2>$null` swallows, so every iteration returns empty and this throws a bogus
    # "no CI run found". That silently broke the 1.1.1 release (commit WAS pushed + CI green,
    # just never detected); the release had to be finished by hand.
    $runId = $null
    for ($i = 0; $i -lt 120 -and -not $runId; $i++) {
        Start-Sleep -Seconds 6
        $runId = (gh run list --branch main --workflow CI --limit 30 --json headSha,databaseId `
                --jq "[.[] | select(.headSha==`"$sha`")][0].databaseId" 2>$null)
    }
    if (-not $runId) {
        # "check Actions" reads like the repo did something wrong, and on 2026-08-06 it did not:
        # GitHub Actions was in a MAJOR OUTAGE, so no run was ever created for the commit and
        # three earlier runs sat queued for an hour. Their status API answers that in one
        # request, so ask before blaming the push. Best-effort: if the status page is
        # unreachable too, fall back to the original wording rather than hiding the real error.
        $actions = try {
            (Invoke-RestMethod -Uri 'https://www.githubstatus.com/api/v2/components.json' `
                -TimeoutSec 15).components |
                Where-Object name -eq 'Actions' | Select-Object -ExpandProperty status -First 1
        } catch { $null }
        if ($actions -and $actions -ne 'operational') {
            throw "no CI run found for $sha after 12 min, because GitHub Actions is '$actions' " +
                  "(https://www.githubstatus.com). NOT a problem with this commit or this repo - " +
                  "main is pushed, nothing is tagged or published. Re-run this script once " +
                  "Actions is operational and it will start over cleanly."
        }
        throw "no CI run found for $sha after 12 min - check Actions (GitHub reports Actions " +
              "'$(if ($actions) { $actions } else { 'status unknown' })', so this is more likely " +
              "the push not triggering the workflow than an outage)"
    }
    # POLL the run to completion via `gh run view` (JSON). We deliberately do NOT use
    # `gh run watch`: it needs a live TTY and exits non-zero when run headless (from a
    # background / non-interactive shell), which aborts the release even though CI is fine
    # (this is exactly what broke the 0.7.1 release run).
    Write-Host "      run $runId found - waiting for it to finish..." -ForegroundColor Green
    $status = ''
    for ($i = 0; $i -lt 160 -and ($status -eq '' -or $status -eq 'queued' -or $status -eq 'in_progress'); $i++) {
        Start-Sleep -Seconds 15
        $status = (gh run view $runId --json status --jq .status 2>$null)
    }
    $concl = (gh run view $runId --json conclusion --jq .conclusion 2>$null)
    if ($concl -ne 'success') { throw "CI on $($sha.Substring(0,7)) finished '$concl' (not success) - NOT releasing. Fix + re-run." }
    Write-Host "      CI green." -ForegroundColor Green

    # 4) Build the shippable installers.  CI validates code; it does not
    # build installers.  The build driver keeps their stages separate.
    if (-not $SkipBuild) {
        Write-Host "[4/6] build installers: $($releaseArtifacts.Architecture -join ' + ')" -ForegroundColor Green
        foreach ($artifact in $releaseArtifacts) {
            $buildArgs = @('-Architecture', $artifact.Architecture)
            pwsh "$root\scripts\build-release.ps1" @buildArgs
            if ($LASTEXITCODE) { throw "$($artifact.Architecture) installer build failed" }
        }
    } else {
        Write-Host "[4/6] -SkipBuild: require exact full-build provenance for $($releaseArtifacts.Architecture -join ' + ')" -ForegroundColor Yellow
    }
    foreach ($artifact in $releaseArtifacts) {
        $artifact.Setup = Get-Item -LiteralPath $artifact.SetupPath -ErrorAction Stop
        $artifact.Manifest = Get-Item -LiteralPath $artifact.ManifestPath -ErrorAction Stop
        pwsh "$root\scripts\check-release-manifest.ps1" `
            -InstallerPath $artifact.Setup.FullName `
            -StagePath $artifact.StagePath `
            -ManifestPath $artifact.Manifest.FullName `
            -ExpectedVersion $ver `
            -ExpectedCommitSha $sha `
            -Architecture $artifact.Architecture
        if ($LASTEXITCODE) {
            throw "$($artifact.Architecture) release provenance/integrity gate failed - NOT publishing"
        }
    }

    # 4a) The portable zips, built AFTER the installer provenance gate above has read each
    # stage. `-Portable` stages into its own directory so it cannot disturb them either way,
    # but ordering it here means even a future change to that can't invalidate a gate that
    # already passed. `-SkipBuild` is always safe for this leg: the installer pass immediately
    # above just built these exact binaries, so this only re-stages and zips.
    #
    # NOT provenance- or size-gated, deliberately: there is no .release.json for a zip and no
    # calibrated size reference, and inventing either would put a brand-new failure mode
    # AFTER main is already pushed and CI is already green - the exact trap the artifact-table
    # comment above exists to avoid. It is NOT separately VirusTotal'd either, because the
    # bytes in it are the same EXEs the scanned installer carries. It IS digest-verified after
    # upload like every other asset (step 5).
    Write-Host "[4a/6] build portable zips: $($releaseArtifacts.Architecture -join ' + ')" -ForegroundColor Green
    foreach ($artifact in $releaseArtifacts) {
        pwsh "$root\scripts\build-release.ps1" -Portable -SkipBuild -Architecture $artifact.Architecture
        if ($LASTEXITCODE) { throw "$($artifact.Architecture) portable zip build failed - NOT publishing" }
        if (-not (Test-Path -LiteralPath $artifact.PortablePath -PathType Leaf)) {
            throw "portable zip missing after its build: $($artifact.PortablePath)"
        }
        $artifact.Portable = Get-Item -LiteralPath $artifact.PortablePath -ErrorAction Stop
    }

    # 4b) VirusTotal the EXACT artifact we are about to publish, BEFORE publishing it.
    # Added 2026-07-18: nothing scanned releases up to and including v1.2.0, so ESET's
    # generic-ML "Generik.*" verdicts on 1.1.0/1.1.1 were first seen on SourceForge's
    # listing rather than here. Non-fatal if the scanner is unavailable (missing .env /
    # no network): a release must not be blocked by tooling absence, only by a real
    # verdict. push_to_vt.py --gate decides what counts as real - see its TIER1/MAX_TOTAL.
    # NOTE: push_to_vt.py and .env are BOTH gitignored (the key lives beside the script), so
    # after a fresh clone neither exists — hence the existence checks rather than assuming.
    Write-Host "[4b/6] VirusTotal scan (gate)" -ForegroundColor Green
    $vt = Join-Path $root 'push_to_vt.py'
    if ((Test-Path $vt) -and (Test-Path "$root\.env") -and (Get-Command python -EA SilentlyContinue)) {
        foreach ($artifact in $releaseArtifacts) {
            python $vt $artifact.Setup.FullName --gate
            # 75 = EX_TEMPFAIL: the scan was still queued when the poll window ran out.
            # That is the scanner being busy, not a verdict about the file, and treating
            # it as a failure aborted a clean 1.6.0 release after both installers had
            # already built. Same rule as a missing scanner above: tooling absence must
            # not block a release, only a real detection. A real detection still exits 1.
            if ($LASTEXITCODE -eq 75) {
                Write-Host "      VT analysis did not finish in time for $($artifact.Setup.Name)." -ForegroundColor Yellow
                Write-Host "      NOT blocking. Re-check the permalink above before announcing:" -ForegroundColor Yellow
                Write-Host "        python push_to_vt.py `"$($artifact.Setup.FullName)`" --gate" -ForegroundColor Yellow
            } elseif ($LASTEXITCODE) {
                throw "VirusTotal gate FAILED for $($artifact.Setup.Name) - NOT publishing. Review the permalink above."
            }
        }
    } else {
        Write-Host "      SKIPPED - push_to_vt.py, .env, or python missing (both are gitignored;" -ForegroundColor Yellow
        Write-Host "      recreate them after a fresh clone). Scan manually before announcing:" -ForegroundColor Yellow
        foreach ($artifact in $releaseArtifacts) {
            Write-Host "        python push_to_vt.py `"$($artifact.Setup.FullName)`" --gate" -ForegroundColor Yellow
        }
    }

    # Local Defender scan (informational, never blocks). VirusTotal's Microsoft engine runs
    # WITHOUT the cloud/reputation context a real Defender install has, so it reports an ML
    # generic on every unsigned low-prevalence Inno installer we ship. docs/AV-SUBMISSION.md's
    # rule is that a false-positive submission is only meaningful once REAL Defender names a
    # threat - and the portal requires that name. This answers that question automatically, so
    # a release no longer ends with a manual "go check Defender" whose answer is always clean.
    Write-Host "[4c/6] local Defender scan (informational)" -ForegroundColor Green
    & (Join-Path $PSScriptRoot 'av-defender-check.ps1') -Path ($releaseArtifacts | ForEach-Object { $_.Setup.FullName })
    if ($LASTEXITCODE) {
        Write-Host "      Real Defender named a threat - the submission above IS warranted." -ForegroundColor Yellow
        Write-Host "      NOT blocking the release (the VT gate above is the blocking one)." -ForegroundColor Yellow
    }
    $global:LASTEXITCODE = 0

    # The updater must NEVER ship broken again (owner directive, 2026-08-10, after
    # 1.3.3..=1.10.0 shipped a self-lock that failed every one-click update). Prove it on
    # the EXACT x64 artifact being published: run the built app's own verify -> lock ->
    # elevated-launch pipeline against it, which upgrades THIS machine's install in place
    # and throws (aborting the release) if the upgrade does not land. CI's
    # self-update-smoke job already gated the commit on a Compact build at [3/6]; this is
    # the same harness on the real Full installer users will download. Side effect by
    # design: the release machine ends the ritual running the build it just shipped.
    Write-Host "[4d/6] self-update smoke on this machine (x64 artifact)" -ForegroundColor Green
    $x64Artifact = $releaseArtifacts | Where-Object { $_.Architecture -eq 'x64' } | Select-Object -First 1
    if ($x64Artifact) {
        & (Join-Path $PSScriptRoot 'test-self-update.ps1') -Setup $x64Artifact.Setup.FullName
        if ($LASTEXITCODE) { throw 'Self-update smoke FAILED - NOT publishing.' }
    } else {
        Write-Host "      SKIPPED - no x64 artifact in this run (ARM64-only builds can't upgrade an x64 host)." -ForegroundColor Yellow
    }

    # The build must not move HEAD or rewrite tracked inputs after we captured + validated $sha.
    # The optional local marketing-site refresh is ignored and is deliberately not an
    # installer/provenance input.
    $headAfterBuild = (git rev-parse HEAD).Trim()
    if ($headAfterBuild -ne $sha) {
        throw "HEAD moved from validated commit $sha to $headAfterBuild during the release - NOT publishing."
    }
    if (git status --porcelain) {
        throw "working tree changed during the release build - commit the generated changes, then re-run."
    }

    # Produce the release body from the reviewed changelog and the now-validated
    # x64 is the established primary installer in the exporter; append ARM64's
    # independently validated digest so the public notes cover both uploads.
    $notes = Join-Path $root "dist\RELEASE-NOTES-$tag.md"
    $x64Artifact = @($releaseArtifacts | Where-Object Architecture -ceq 'x64')
    if ($x64Artifact.Count -ne 1) { throw 'release artifact table has no unique x64 installer' }
    pwsh "$root\scripts\export-release-notes.ps1" `
        -Version $ver `
        -InstallerPath $x64Artifact[0].Setup.FullName `
        -OutputPath $notes
    if ($LASTEXITCODE) { throw "curated release-note export failed - NOT publishing" }
    $arm64Artifact = @($releaseArtifacts | Where-Object Architecture -ceq 'arm64')
    if ($arm64Artifact.Count -gt 1) { throw 'release artifact table has more than one ARM64 installer' }
    if ($arm64Artifact.Count -eq 1) {
        Add-Content -LiteralPath $notes -Encoding utf8 -Value @(
            '',
            '- **ARM64 installer:** ``' + $arm64Artifact[0].Setup.Name + '``',
            '- **ARM64 SHA-256:** ``' + (Get-ReleaseSha256 -Path $arm64Artifact[0].Setup.FullName) + '``'
        )
    }

    # State the portable scope in the notes rather than letting the filename imply more than
    # it delivers. Everyone who downloads it will otherwise ask the same question, which is
    # the one issue #13 already asked. No sizes or counts here - the notes stay evergreen.
    #
    # CORRECTED 2026-08-14. This block used to say the portable build could NOT do Explorer
    # thumbnails or the right-click menu. That was true of 1.8.0 and was FIXED IN 1.8.1, which
    # ships the handler in the zip and registers it per user - and nobody updated the notes, so
    # every release from 1.8.1 to 1.12.0 shipped release notes denying one of its own headline
    # features. Keep this in step with CLAUDE.md's portable section if the scope changes again.
    Add-Content -LiteralPath $notes -Encoding utf8 -Value @(
        '',
        '### Portable (no installer)',
        '',
        'Extract and run. Nothing is installed, nothing goes in the registry, no admin needed.',
        'Settings live in `SageThumbs2K.ini` next to the exe.',
        '',
        'You get the app and the command line tool: settings, convert and resize, quick',
        'preview, screenshots, OCR, the colour picker and the folder tools.',
        '',
        '**Explorer thumbnails and the classic right-click menu work too.** Turn them on with',
        '`st2k register`, or the button in Settings under Advanced. That registers the handler',
        'for your user account only: no installer, no administrator rights, nothing written',
        'machine-wide. `st2k register --off` undoes it, and you should run that *before* moving',
        'or deleting the folder, otherwise Windows is left pointing at a file that has gone.',
        '',
        'Three things still need the installer, because Windows only accepts them registered',
        'for the whole machine: the Explorer **preview pane**, the **Details pane** columns,',
        'and the **Windows 11 right-click menu** (the compact one).'
    )
    foreach ($artifact in $releaseArtifacts) {
        Add-Content -LiteralPath $notes -Encoding utf8 -Value @(
            '',
            '- **' + $artifact.Architecture + ' portable:** ``' + $artifact.Portable.Name + '``',
            '- **' + $artifact.Architecture + ' portable SHA-256:** ``' +
                (Get-ReleaseSha256 -Path $artifact.Portable.FullName) + '``'
        )
    }

    # 5) Create a DRAFT first. Verify GitHub received the exact local bytes before
    # publishing, so an upload anomaly never briefly exposes a corrupt public build.
    # Target the immutable SHA we actually checked, not the moving `main` ref: another push while
    # this script waits/builds must never make the release tag point at an unvalidated commit.
    Write-Host "[5/6] create + verify draft release $tag" -ForegroundColor Green
    # Installers ONLY. The .release.json build manifest is still generated and still gated on
    # (step [4/6] runs check-release-manifest.ps1 against it BEFORE anything is uploaded), but
    # it is LOCAL provenance: nothing downstream reads the published copy - not the in-app
    # updater (which reads GitHub's own release JSON), not winget, not CI. Publishing it only
    # put a large, noisy file next to the two things people actually download.
    $releaseAssetPaths = @(
        foreach ($artifact in $releaseArtifacts) {
            $artifact.Setup.FullName
            $artifact.Portable.FullName
        }
    )
    gh release create $tag @releaseAssetPaths `
        --draft `
        --title "SageThumbs 2K $ver" `
        --target $sha `
        --notes-file $notes
    if ($LASTEXITCODE) { throw "gh draft release create failed" }

    foreach ($assetPath in $releaseAssetPaths) {
        $asset = Get-Item -LiteralPath $assetPath -ErrorAction Stop
        $localDigest = 'sha256:' + (Get-ReleaseSha256 -Path $asset.FullName)
        $remoteDigest = gh release view $tag --json assets `
            --jq ".assets[] | select(.name == `"$($asset.Name)`") | .digest"
        if ($LASTEXITCODE -ne 0 -or -not $remoteDigest) {
            throw "could not verify uploaded digest for $($asset.Name); $tag remains a draft"
        }
        $remoteDigest = ([string]$remoteDigest).Trim().ToLowerInvariant()
        if ($remoteDigest -cne $localDigest) {
            throw "uploaded digest mismatch for $($asset.Name) (local $localDigest, GitHub $remoteDigest); $tag remains a draft"
        }
    }
    gh release edit $tag --draft=false
    if ($LASTEXITCODE) { throw "draft verified but publication failed; $tag remains a draft" }

    # SourceForge's green Download button. The default lives on the FILE, so every release
    # starts with none and SourceForge guesses - and on v1.7.5 it guessed the ARM64 installer
    # and served it to every Windows visitor. Run it here so it is never a thing to remember.
    #
    # NON-FATAL by design, and it must stay that way: the GitHub release is already public by
    # this point, so throwing would report a failed release that actually succeeded.
    #
    # SourceForge does not have the files yet when we get here - that upload runs on its own
    # schedule and on 1.8.0 landed about six minutes later - so the script RETRIES for up to
    # 20 minutes rather than returning a note telling you to come back and run it yourself.
    # That means this step can sit here for a few minutes; everything else is already done.
    Write-Host "[6/6] SourceForge default download" -ForegroundColor Green
    & pwsh -NoProfile -File "$root\scripts\set-sourceforge-default.ps1" -Version $ver
    if ($LASTEXITCODE) {
        Write-Host "  NOT set - the green Download button on SourceForge may point at the wrong" -ForegroundColor Yellow
        Write-Host "  installer. Re-run after uploading:  pwsh scripts\set-sourceforge-default.ps1" -ForegroundColor Yellow
    }

    Write-Host "[6/6] DONE - $tag released." -ForegroundColor Cyan

    # 7) Submit to winget, FROM HERE, with the local `gh`. No secret, no CI run, no PAT.
    #
    # This used to publish through the `winget.yml` GitHub Action driven by a WINGET_TOKEN
    # secret, and that arrangement failed repeatedly and silently for a year:
    #   * a classic PAT expired after 1.7.2, so 1.7.3 and 1.7.4 never published;
    #   * the fine-grained PAT that replaced it on 2026-08-06 was answering 401 within two
    #     hours, and because the workflow's onboarding guard treated ANY non-200 as "package
    #     not onboarded yet" and skipped with a green tick, nothing ever said so. Nine
    #     releases (1.8.2 .. 1.12.0) reported success while publishing nothing.
    # The token was replaced several times. It could not have worked in the shape it was asked
    # to: a fine-grained PAT only carries permissions on repositories its owner OWNS, and
    # microsoft/winget-pkgs is permanently read-only to one, so the pull-request step was
    # unreachable by construction. The recurring "the token is dead again" was a symptom.
    #
    # The local `gh` OAuth login already carries everything the whole job needs, and
    # `winget-submit.ps1` does the whole job with it: read the release's own published digests,
    # rewrite the manifest triplet, sync the fork, push the branch, open the PR. It is
    # idempotent and stops early if the version is already published or a PR is already open.
    # A release failing at winget is reported, never fatal - the release itself is already
    # public and correct by this point.
    Write-Host "[winget] submitting $ver to winget-pkgs..." -ForegroundColor DarkGray
    & (Join-Path $PSScriptRoot 'winget-submit.ps1') -Version $ver
    if ($LASTEXITCODE -ne 0) {
        Write-Host "[winget] submission did NOT complete - winget users stay on the previous version." -ForegroundColor Red
        Write-Host "  retry with: pwsh scripts\winget-submit.ps1 -Version $ver" -ForegroundColor Yellow
    }
}
finally { Pop-Location }
