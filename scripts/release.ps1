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
param(
    [switch]$SkipBuild,
    # Publish without waiting for the ARM64 portable zip to run on real ARM64 silicon
    # (step [5a/6]). Only for a release with no ARM64 artifact of its own to prove, or when
    # the windows-11-arm runner pool is down; the outcome line says OVERRIDDEN so the record
    # shows the gate was not run.
    [switch]$SkipArm64Gate,
    # Publish without running the suite against the RELEASE-profile, shipped-feature binaries
    # (step [3b/6]). CI only ever tests the debug build with default features, so this is the
    # only proof the artifacts we are about to ship actually pass their own tests; skip it only
    # when the runner pool is down. The outcome line says OVERRIDDEN so the record shows it.
    [switch]$SkipReleaseProfileTests
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'release-manifest-lib.ps1')

# MUST match `UPDATE_PUBLIC_KEY` in src\bin\app\update.rs byte-for-byte. Kept here as a plain
# constant rather than parsed out of the built binary - a self-check that re-derives the value
# it is checking from the same build proves nothing, and pasting the same array literal into
# two places at key-rotation time is the same one-line diff either way. Whoever runs
# `cargo run --release --example update-keygen` updates BOTH this line and update.rs together;
# see docs\RELEASE-SECURITY.md for the rotation procedure.
# Placeholder (32 zero bytes) until the integrator pastes the real key from
# `update-keygen`'s output - matches update.rs's placeholder, so a release built before the
# real key exists fails loudly at [4e/6] instead of quietly shipping a signature nothing can
# ever verify.
$UpdatePublicKeyHex = '169fce0ade4aeced2dcb36a376c127743844779184f5109bc38c00603fa8325b'

# Standardised stage-outcome line (2026-09-05 audit, finding F22b): before this, a stage that
# skipped for a good reason ("scanner absent") and one that ran clean printed in whatever prose
# that call site happened to use, so scanning the run's own console output (its only record -
# this script writes no separate run log) could not tell the two apart at a glance. Every
# stage that can do something other than a plain pass/throw goes through this so the outcome
# word is always one of: PASSED, FAILED (non-fatal), SKIPPED (optional), OVERRIDDEN. A stage
# whose only outcomes are "ran fine" or "threw and aborted the release" needs no call here -
# the throw itself is already an unambiguous FAILED.
function Write-ReleaseStageOutcome {
    param(
        [Parameter(Mandatory)][ValidateSet('PASSED', 'FAILED (non-fatal)', 'SKIPPED (optional)', 'OVERRIDDEN')]
        [string]$Outcome,
        [Parameter(Mandatory)][string]$Stage,
        [Parameter(Mandatory)][string]$Reason
    )
    # Three colours, not two: a non-fatal FAILURE (winget did not complete, SourceForge did not
    # flip) must not blend into routine yellow skips and overrides in console scrollback. The
    # first cut of this helper mapped everything but PASSED to Yellow and lost the red the
    # winget failure used to print in (caught in review, 2026-09-05 audit, F22).
    $color = switch ($Outcome) {
        'PASSED' { 'Green' }
        'FAILED (non-fatal)' { 'Red' }
        default { 'Yellow' }
    }
    Write-Host "      ${Outcome}: ${Stage}: ${Reason}" -ForegroundColor $color
}

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
    # Signing is a PRECONDITION of a release, not a feature of one (owner directive, Michael,
    # 2026-09-10: "all builds moving forward should be signed... can't be sending out any
    # unsigned executables"). build-release.ps1 tolerates an unconfigured signer because CI and
    # dev builds have none; this script does not. Refuse here, before a single minute of the
    # pipeline is spent, rather than at [4/6] where the manifest gate would catch the same
    # thing with the build already done.
    pwsh "$root\scripts\packaging\sign-release.ps1" -Configured
    if ($LASTEXITCODE) {
        throw "code signing is not configured on this machine (ST2K_SIGN_ENDPOINT/ACCOUNT/PROFILE plus the Azure lease; docs/RELEASE-SECURITY.md) - a release is never cut unsigned"
    }
    $changelog = Join-Path $root 'docs\CHANGELOG.md'
    $null = Get-ReleaseChangelogSection -ChangelogPath $changelog -Version $ver
    pwsh "$root\scripts\check-consistency.ps1"; if ($LASTEXITCODE) { throw "consistency check failed - fix before releasing" }
    # The pre-release issue review (CLAUDE.md 6.2). Informational, never a gate - see the
    # header of check-issues.ps1 for why gating would be wrong. It prints the OPEN issues AND,
    # crucially, the CLOSED ones commented since the last release: v2.1.0 shipped while a long
    # follow-up sat unread on closed issue #26, which `gh issue list --state open` cannot show.
    pwsh "$root\scripts\check-issues.ps1"
    # The other half of "what happened after we last shipped": winget-submit.ps1 opens a PR and
    # exits, so a submission that FAILS validation is invisible here until a notification lands
    # days later. v2.3.0 and v2.3.1 both sat open and failing for days that way. Informational
    # for the same reason check-issues is: a stale failing PR for a superseded version must
    # never block the release that supersedes it.
    pwsh "$root\scripts\check-winget.ps1"
    # The third "what happened after we last shipped" question, and the one a user had to ask
    # for us: antivirus engines keep re-scoring a hash long after step 4b's gate cleared it, so
    # a release can pass on the day and drift afterwards. v2.5.0 passed the gate at 2 or 3
    # detections and read 9 four days later; the way we learned that was a comment on issue
    # #30. Informational for the same reason as the two above - a verdict on a superseded
    # release must never block the release that supersedes it, which is why check-av.ps1 scopes
    # its own -Gate to the CURRENT release only.
    # -Releases 3 rather than the default: the question here is "did what we last shipped
    # drift", and each artifact costs a paced VirusTotal call (their public API allows four a
    # minute), so the full history would add minutes to every release for context nobody reads
    # at this moment. `pwsh scripts\check-av.ps1 -Releases 12` is there when the history IS the
    # question.
    pwsh "$root\scripts\check-av.ps1" -Releases 3

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
    # A green CI run on an ANCESTOR whose commits since are verification-only proves these same
    # binaries (the rule the manifest check already applies to artifacts, now shared through
    # the lib): a script-only fix pushed mid-release must not cost another 30-minute wait.
    $runId = $null
    $proving = Find-ReleaseProvingRun -Root $root -Sha $sha -Workflow CI
    if ($proving -and $proving.HeadSha -cne $sha) {
        Write-Host ("      CI run {0} on ancestor {1} proves this commit ({2}); the commits since touch only verification scripts and docs" -f `
            $proving.Id, $proving.HeadSha.Substring(0, 7), $proving.Status) -ForegroundColor Yellow
        $runId = $proving.Id
    }
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
    $concl = Wait-ReleaseRunConclusion -RunId $runId -MaxMinutes 45
    if ($concl -ne 'success') { throw "CI on $($sha.Substring(0,7)) finished '$concl' (not success) - NOT releasing. Fix + re-run." }
    Write-Host "      CI green." -ForegroundColor Green

    # 3b) The suite against the RELEASE-profile, shipped-feature binaries.
    #
    # CI green above means the DEBUG build passed with DEFAULT features. The binary users
    # install is a different artifact: opt-level="z", fat LTO, stripped, and built with
    # webp-lossy,dll-i18n-subset. `build-release.ps1` runs no tests at all, so without this
    # step nothing has ever run the suite against the shape we ship. It also holds the only
    # honest reading of the latency budgets (750 ms right-click, the 6 s / 2 s video ceilings),
    # which mean nothing measured on unoptimised code.
    #
    # It is a separate DISPATCHED workflow rather than a CI job on purpose: a release-profile
    # run costs a full LTO build, and charging that to every push and pull request is what
    # `test-architecture-release-contract.ps1` ("CI keeps production payloads release and
    # validation debug") deliberately keeps out. Same dispatch-and-wait shape as the ARM64
    # gate at [5a/6]; no `gh run watch` (headless TTY trap, see [3/6]).
    Write-Host "[3b/6] release-profile tests on the shipped feature sets (gate)" -ForegroundColor Green
    if ($SkipReleaseProfileTests) {
        Write-ReleaseStageOutcome -Outcome 'OVERRIDDEN' -Stage 'release-profile tests' -Reason (
            '-SkipReleaseProfileTests flag: the suite was NOT run against the release-profile, ' +
            'shipped-feature binaries; only the debug default-feature CI run stands behind this release')
    } else {
        # A suite already running (or green) on this commit, or on an ancestor whose commits
        # since are verification-only, proves the same shipped binaries: reuse it rather than
        # dispatching a second full LTO build. The relaunch after a script-only fix is exactly
        # this case, and it used to cost the whole suite again.
        $rptRunId = $null
        $rptProving = Find-ReleaseProvingRun -Root $root -Sha $sha -Workflow 'release-profile-tests.yml' -Event workflow_dispatch
        if ($rptProving) {
            Write-Host ("      release-profile run {0} on {1} ({2}) proves this commit; reusing it" -f `
                $rptProving.Id, $rptProving.HeadSha.Substring(0, 7), $rptProving.Status) -ForegroundColor Yellow
            $rptRunId = $rptProving.Id
        } else {
            $rptDispatchedAt = (Get-Date).ToUniversalTime().AddSeconds(-2).ToString('o')
            gh workflow run 'release-profile-tests.yml'
            if ($LASTEXITCODE) { throw "could not dispatch release-profile-tests.yml; nothing has been built or published" }
            for ($i = 0; $i -lt 40 -and -not $rptRunId; $i++) {
                Start-Sleep -Seconds 6
                $rptRunId = (gh run list --workflow 'release-profile-tests.yml' --event workflow_dispatch --limit 10 `
                        --json databaseId,createdAt --jq "[.[] | select(.createdAt >= `"$rptDispatchedAt`")][0].databaseId" 2>$null)
            }
            if (-not $rptRunId) { throw 'release-profile-tests.yml was dispatched but no run appeared in 4 min; nothing has been built or published' }
        }
        Write-Host "      run $rptRunId found - waiting for the release-profile suite..." -ForegroundColor Green
        $rptConcl = Wait-ReleaseRunConclusion -RunId $rptRunId -MaxMinutes 60
        if ($rptConcl -ne 'success') {
            throw "the release-profile suite finished '$rptConcl' (run $rptRunId) - NOT releasing. The debug CI run passing does not cover this; the shipped shape genuinely fails."
        }
        Write-ReleaseStageOutcome -Outcome 'PASSED' -Stage 'release-profile tests' -Reason "the suite passed against the release-profile shipped feature sets (run $rptRunId)"
    }

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
        Write-Host "[4/6] -SkipBuild" -ForegroundColor Yellow
        Write-ReleaseStageOutcome -Outcome 'OVERRIDDEN' -Stage 'build installers' -Reason (
            "-SkipBuild flag: requires exact full-build provenance for $($releaseArtifacts.Architecture -join ' + ') from a prior build, re-hashed below"
        )
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
                Write-ReleaseStageOutcome -Outcome 'OVERRIDDEN' -Stage 'VirusTotal' -Reason (
                    "analysis for $($artifact.Setup.Name) did not finish in time - not blocking; " +
                    "re-check the permalink above before announcing (python push_to_vt.py `"$($artifact.Setup.FullName)`" --gate)"
                )
            } elseif ($LASTEXITCODE) {
                throw "VirusTotal gate FAILED for $($artifact.Setup.Name) - NOT publishing. Review the permalink above."
            }
        }
    } else {
        # SKIPPED, never silent: this is exactly the line finding F22 called out as not
        # distinguishable from a stage that ran. push_to_vt.py and .env are both gitignored, so
        # a fresh clone never has them until recreated.
        Write-ReleaseStageOutcome -Outcome 'SKIPPED (optional)' -Stage 'VirusTotal' -Reason (
            'push_to_vt.py, .env, or python missing (both are gitignored; recreate them after a fresh clone)'
        )
        foreach ($artifact in $releaseArtifacts) {
            Write-Host "        scan manually: python push_to_vt.py `"$($artifact.Setup.FullName)`" --gate" -ForegroundColor Yellow
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
        Write-ReleaseStageOutcome -Outcome 'OVERRIDDEN' -Stage 'Defender scan' -Reason (
            'real Defender named a threat (the submission above IS warranted) - not blocking; the VirusTotal gate above is the blocking one'
        )
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
        Write-ReleaseStageOutcome -Outcome 'SKIPPED (optional)' -Stage 'self-update smoke' -Reason (
            "no x64 artifact in this run (ARM64-only builds can't upgrade an x64 host)"
        )
    }

    # 4e) Sign every installer + portable zip with the ed25519 key that
    # `update.rs::verify_signature` checks before the in-app updater ever launches a
    # downloaded installer. This is a SEPARATE mechanism from the Authenticode signing in
    # scripts\packaging\sign-release.ps1 (that one proves "this file is really ours" to
    # Windows, when it is configured at all; this one is what our OWN updater trusts, and it
    # must exist regardless of whether Authenticode is configured this run - see
    # docs\RELEASE-SECURITY.md). FAILS THE RELEASE LOUDLY on any problem: a self-update
    # pipeline that quietly ships an unsigned build is worse than one that never had this step.
    Write-Host "[4e/6] sign release artifacts (update signature)" -ForegroundColor Green
    if (-not $env:ST2K_UPDATE_SIGNING_KEY) {
        $envFile = "$root\.env"
        if (Test-Path $envFile) {
            $line = Get-Content $envFile |
                Where-Object { $_ -match '^\s*ST2K_UPDATE_SIGNING_KEY\s*=' } |
                Select-Object -First 1
            if ($line) { $env:ST2K_UPDATE_SIGNING_KEY = ($line -split '=', 2)[1].Trim() }
        }
    }
    if (-not $env:ST2K_UPDATE_SIGNING_KEY) {
        throw ("ST2K_UPDATE_SIGNING_KEY is not set (checked the environment and $root\.env) - " +
            "run 'cargo run --release --example update-keygen' once and keep its .env line, " +
            "or set the variable yourself. A release must not publish without an update " +
            "signature.")
    }
    $signAssetPaths = @(
        foreach ($artifact in $releaseArtifacts) {
            $artifact.Setup.FullName
            $artifact.Portable.FullName
        }
    )
    & cargo run --release --example update-sign -- @signAssetPaths
    if ($LASTEXITCODE) { throw "update-sign failed - NOT publishing an unsigned release" }
    foreach ($assetPath in $signAssetPaths) {
        if (-not (Test-Path "$assetPath.sig")) {
            throw "update-sign reported success but $assetPath.sig is missing - NOT publishing"
        }
    }
    # Self-check against the compiled-in public key ($UpdatePublicKeyHex, top of this script):
    # a release can never publish a signature this same tool could not also verify.
    & cargo run --release --example update-sign -- --verify $UpdatePublicKeyHex @signAssetPaths
    if ($LASTEXITCODE) {
        throw ("update-sign --verify failed on our own freshly-signed artifacts - NOT " +
            "publishing. If UPDATE_PUBLIC_KEY was rotated, update `$UpdatePublicKeyHex at the " +
            "top of this script to match.")
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
        # PARENTHESES ARE LOAD-BEARING. In PowerShell the comma binds TIGHTER than `+`, so
        # `@( '', 'a' + $x + 'b' )` parses as `('', 'a') + $x + 'b'` and yields FOUR elements,
        # not two. Add-Content then writes each on its own line, which is why every ARM64
        # release note from at least 2.4.0 to 2.4.1 published as a code span split across three
        # lines: the label, then the filename, then the closing backticks. Cosmetic, public,
        # and invisible until someone read the rendered page rather than the script.
        # Single backticks to match the x64 lines that export-release-notes.ps1 writes above.
        Add-Content -LiteralPath $notes -Encoding utf8 -Value @(
            ''
            ('- **ARM64 installer:** `' + $arm64Artifact[0].Setup.Name + '`')
            ('- **ARM64 SHA-256:** `' + (Get-ReleaseSha256 -Path $arm64Artifact[0].Setup.FullName) + '`')
        )
    }
    # The x64 installer's second name (uploaded at step 5), explained where people will see it.
    # Parenthesised for the reason the ARM64 block above spells out.
    Add-Content -LiteralPath $notes -Encoding utf8 -Value @(
        ''
        ('- **x64 installer, second copy:** `SageThumbs2K-Setup-' + $ver + '-amd64.exe` is the same file as' +
            ' the verified x64 installer above, published under a name that lists first so that copies' +
            ' older than 1.3.6 can update themselves. Download the plain one.')
    )

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
        # Parenthesised for the same reason as the ARM64 block above: the comma binds tighter
        # than `+`, so without these each line published as FIVE separate lines, splitting a
        # code span across the architecture name and the filename.
        Add-Content -LiteralPath $notes -Encoding utf8 -Value @(
            ''
            ('- **' + $artifact.Architecture + ' portable:** `' + $artifact.Portable.Name + '`')
            ('- **' + $artifact.Architecture + ' portable SHA-256:** `' +
                (Get-ReleaseSha256 -Path $artifact.Portable.FullName) + '`')
        )
    }

    # The VirusTotal permalink for each installer, which step [4b/6] has just scanned anyway.
    # Reputation verdicts on an unsigned installer are this project's most persistent support
    # question (issue #30, and #12 before it), and the single most useful answer is the actual
    # ratio next to the actual file rather than one popup's opinion. The link is derived from
    # the artifact's own SHA-256, so it always points at exactly the bytes attached below and
    # cannot drift onto some other build. README.md's antivirus FAQ promises this link exists;
    # this is what makes that true.
    # Read the truth off the artifacts rather than asserting it: once Azure Artifact Signing
    # is live (scripts/packaging/sign-release.ps1) the installers carry a valid signature and
    # the old sentence would be a lie in every release note that followed.
    # A HARD GATE since 3.0.1 (owner directive, Michael, 2026-09-10): every installer this run
    # is about to publish must carry a valid Authenticode signature, or nothing is published.
    # [1/6] already refused an unconfigured signer; this is the proof on the artifact itself.
    foreach ($artifact in $releaseArtifacts) {
        $setupSignature = Get-AuthenticodeSignature -LiteralPath $artifact.Setup.FullName
        if ($setupSignature.Status -ne 'Valid') {
            throw "REFUSING to publish: $($artifact.Setup.Name) is not validly signed ($($setupSignature.Status)) - no unsigned executable leaves this pipeline"
        }
        if ($artifact.Portable -and (Test-Path -LiteralPath $artifact.Portable.FullName)) {
            $portableUnsigned = @(Get-ReleasePortableUnsignedPes -ZipPath $artifact.Portable.FullName)
            if ($portableUnsigned.Count) {
                throw "REFUSING to publish: $($artifact.Portable.Name) carries unsigned or invalidly signed binaries: $($portableUnsigned -join ', ')"
            }
        }
    }
    $signingLine = '**Antivirus / SmartScreen:** these builds are code-signed. A brand-new signed file with no'
    Add-Content -LiteralPath $notes -Encoding utf8 -Value @(
        ''
        $signingLine
        'download history draws reputation-based warnings. Scan results for these exact bytes:'
    )
    foreach ($artifact in $releaseArtifacts) {
        # NOT `$sha`: that is the validated COMMIT, and `--target $sha` at [5/6] reads it. Reusing
        # the name here (1568e12) made the first release after it (3.0.0) hand GitHub a file
        # hash as the target commit: "Release.target_commitish is invalid", after every gate.
        $installerSha256 = Get-ReleaseSha256 -Path $artifact.Setup.FullName
        Add-Content -LiteralPath $notes -Encoding utf8 -Value @(
            ('- ' + $artifact.Architecture + ' installer on VirusTotal: ' +
                'https://www.virustotal.com/gui/file/' + $installerSha256.ToLower())
        )
    }
    Add-Content -LiteralPath $notes -Encoding utf8 -Value @(
        ''
        'See the [antivirus FAQ](https://github.com/LunarWerxs/SageThumbs-2k#why-did-windows-or-my-antivirus-flag-the-installer)'
        'for what those detection names actually mean and two measurements showing the count'
        'moves without the software changing.'
    )

    # 5) Create a DRAFT first. Verify GitHub received the exact local bytes before
    # publishing, so an upload anomaly never briefly exposes a corrupt public build.
    # Target the immutable SHA we actually checked, not the moving `main` ref: another push while
    # this script waits/builds must never make the release tag point at an unvalidated commit.
    Write-Host "[5/6] create + verify draft release $tag" -ForegroundColor Green
    # Installers + portable zips + their `.sig` files. The .release.json build manifest is
    # still generated and still gated on (step [4/6] runs check-release-manifest.ps1 against it
    # BEFORE anything is uploaded), but it is LOCAL provenance: nothing downstream reads the
    # published copy - not the in-app updater (which reads GitHub's own release JSON), not
    # winget, not CI. Publishing it only put a large, noisy file next to the things people
    # actually download. The `.sig` files ARE read downstream (by the in-app updater, see
    # [4e/6] above) so they ride the same upload + digest-verify loop as everything else here.
    $releaseAssetPaths = @(
        foreach ($artifact in $releaseArtifacts) {
            $artifact.Setup.FullName
            "$($artifact.Setup.FullName).sig"
            $artifact.Portable.FullName
            "$($artifact.Portable.FullName).sig"
        }
    )
    # A SECOND COPY of the x64 installer, named so it lists FIRST. Builds 0.6.3 through 1.3.5
    # self-update by taking the first `.exe` asset with "setup" in its name (GitHub lists
    # assets by name), and since 1.6.0 that has been the ARM64 installer, which an x64 PC
    # silently refuses to run - so every one of those copies has sat on "update available"
    # with nothing ever installing. "amd64" sorts ahead of "arm64" under any case rule
    # ('m' < 'r'), so they take this copy and land on the current build; every build from
    # 1.3.6 on matches the exact names above and never looks at it. Byte-identical to the x64
    # installer, so its `.sig` is the same signature, and the digest loop below verifies it
    # like any other asset. Retire this once builds older than 1.3.6 are no longer in use.
    $x64Alias = Join-Path $root "dist\SageThumbs2K-Setup-$ver-amd64.exe"
    Copy-Item -LiteralPath $x64Artifact[0].Setup.FullName -Destination $x64Alias -Force
    Copy-Item -LiteralPath "$($x64Artifact[0].Setup.FullName).sig" -Destination "$x64Alias.sig" -Force
    $releaseAssetPaths += @($x64Alias, "$x64Alias.sig")
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
    # ---- [5a/6] the ARM64 payload on real ARM64 silicon, BEFORE anyone can download it ----
    # `arm64-portable-verify.yml` runs the portable zip (our binaries, the bundled ARM64
    # ImageMagick, the out-of-process decoders) on a windows-11-arm runner. It used to trigger
    # only on `release: published`, i.e. after users could already have the file (queue item
    # G35): a wrong-architecture DLL or a magick bundle that loads on nobody's machine passed
    # every static packaging check and reached the public release before the one job that runs
    # it ever started. Dispatching it here against the DRAFT (the workflow resolves draft
    # releases through the API, which is why it carries `contents: write`) and waiting for green
    # turns it into a gate: a failure leaves $tag a draft, nothing public. Same polling shape as
    # the CI wait at [3/6]; no `gh run watch` (headless TTY trap, see there).
    Write-Host "[5a/6] ARM64 portable zip on real ARM64 silicon (gate)" -ForegroundColor Green
    $armArtifact = $releaseArtifacts | Where-Object Architecture -eq 'arm64' | Select-Object -First 1
    if (-not $armArtifact) {
        Write-ReleaseStageOutcome -Outcome 'SKIPPED (optional)' -Stage 'ARM64 silicon verify' -Reason 'this release ships no ARM64 artifact'
    } elseif ($SkipArm64Gate) {
        Write-ReleaseStageOutcome -Outcome 'OVERRIDDEN' -Stage 'ARM64 silicon verify' -Reason (
            "-SkipArm64Gate flag: $($armArtifact.Portable.Name) was NOT run on ARM64 silicon before publishing; " +
            'the post-publish run of arm64-portable-verify.yml is the only proof it works')
    } else {
        $dispatchedAt = (Get-Date).ToUniversalTime().AddSeconds(-2).ToString('o')
        gh workflow run 'arm64-portable-verify.yml' -f "tag=$tag"
        if ($LASTEXITCODE) { throw "could not dispatch arm64-portable-verify.yml for $tag; $tag remains a draft" }
        $armRunId = $null
        for ($i = 0; $i -lt 40 -and -not $armRunId; $i++) {
            Start-Sleep -Seconds 6
            $armRunId = (gh run list --workflow 'arm64-portable-verify.yml' --event workflow_dispatch --limit 10 `
                    --json databaseId,createdAt --jq "[.[] | select(.createdAt >= `"$dispatchedAt`")][0].databaseId" 2>$null)
        }
        if (-not $armRunId) { throw "arm64-portable-verify.yml was dispatched for $tag but no run appeared in 4 min; $tag remains a draft" }
        Write-Host "      run $armRunId found - waiting for the ARM64 runner..." -ForegroundColor Green
        $armConcl = Wait-ReleaseRunConclusion -RunId $armRunId -MaxMinutes 45
        if ($armConcl -ne 'success') {
            throw "the ARM64 portable zip failed on real ARM64 silicon (run $armRunId finished '$armConcl'); $tag remains a draft - pull the artifact apart before anyone downloads it"
        }
        Write-ReleaseStageOutcome -Outcome 'PASSED' -Stage 'ARM64 silicon verify' -Reason "$($armArtifact.Portable.Name) ran on windows-11-arm (run $armRunId)"
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
        Write-ReleaseStageOutcome -Outcome 'FAILED (non-fatal)' -Stage 'SourceForge default download' -Reason (
            'the green Download button on SourceForge may point at the wrong installer - re-run: pwsh scripts\set-sourceforge-default.ps1'
        )
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
        Write-ReleaseStageOutcome -Outcome 'FAILED (non-fatal)' -Stage 'winget submission' -Reason (
            "winget users stay on the previous version - retry: pwsh scripts\winget-submit.ps1 -Version $ver"
        )
    }
}
finally { Pop-Location }
