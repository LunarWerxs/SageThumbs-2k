<#
.SYNOPSIS
  Local pre-push gate. Mirrors the GitHub CI `build-test` + `deny` jobs so a broken
  push never has to round-trip through CI to be caught.

  Wired up as a git pre-push hook (.git/hooks/pre-push), but also runnable by hand:
      pwsh scripts/preflight.ps1

  These checks are RELEASE builds on purpose: the tree ships `panic="abort"` + `lto`,
  and some failures (e.g. link-time "unresolved external symbol") only surface in a
  release link — a debug `cargo test` would NOT catch them.
#>
$ErrorActionPreference = 'Stop'
$failed = $false

function Step([string]$name, [scriptblock]$block) {
    Write-Host ""
    Write-Host "==> $name" -ForegroundColor Cyan
    & $block
    if ($LASTEXITCODE -ne 0) {
        Write-Host "FAILED: $name (exit $LASTEXITCODE)" -ForegroundColor Red
        $script:failed = $true
    }
}

# ORDER THE CHEAP CHECKS FIRST. Rustfmt costs ~2 SECONDS and is the check a fresh edit is
# most likely to trip, yet it used to run near the END, behind three LTO release builds and
# two full test passes. On 2026-09-08 that cost a ~30-minute gate run to report one wrapped
# function signature, and then a second ~30-minute run to re-prove everything that had already
# passed. A gate that finds the cheapest failure last converts a two-second fix into an hour.
#
# The rule this encodes, and it generalises to any gate you add here: sort steps by
# (cost to run) ASCENDING, not by the order of the CI job being mirrored. CI runs its jobs in
# PARALLEL across runners, so its ordering carries no signal about what should block first on
# one machine. Anything that needs no build artifacts belongs above this line.
Step 'rustfmt (--check)' { cargo fmt --all --check }
# The complexity gate, which until 2026-09-20 ran ONLY in CI - so a function over the line, or
# a warn band that had grown, was discovered after the push, by a red runner, which is the
# exact round trip this gate exists to prevent. It costs about two seconds (stdlib-only Python
# over the tracked scanner), so it belongs in the cheap tier beside rustfmt. It mirrors CI's
# own `complexity` job byte for byte: same script, same default arguments.
if (-not $failed) {
    Step 'complexity (gate at 30, and the warn band may not grow)' {
        & pwsh -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'check-complexity.ps1')
    }
}
# CI's `consistency` job, run HERE and first (they are seconds, the builds are minutes): every
# gate before the push, none discovered by a red CI after `release.ps1` has already pushed
# (2026-08-02) or by a launch that died at [4/6] on a gate script fault (2026-09-09, twice).
# Invoked the way CI's `shell: pwsh` does - a script that leaves a non-zero $LASTEXITCODE
# behind fails the step even when every assertion passed - so a local green means a CI green.
#
# ⚠ THE LIST IS DERIVED, NEVER TYPED (2026-09-20). It used to be eleven script names written out
# here by hand; CI's consistency job had grown to twenty-two, and the eleven this file had never
# heard of included `check-registration-symmetry.ps1`, which went stale the moment a refactor
# moved a registry write one call deeper and then sat red on a PUBLIC repo for four commits
# because nothing local ran it. `ci-consistency-steps.ps1` reads the workflow and hands back
# exactly what CI runs, in CI's order, and refuses to report fewer than ten steps as "the gate".
# `check-complexity.ps1` is the one deliberate omission here: the cheap tier above already ran it.
if (-not $failed) {
    Step 'consistency scripts (derived from CI''s consistency job)' {
        & pwsh -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'ci-consistency-steps.ps1') -Run -Skip 'check-complexity.ps1'
    }
}

# THE INSTALLER COMPILES (2026-09-19). CI's self-update smoke job compiles installer.iss with
# Inno Setup; nothing before the push did, so a Pascal type mismatch in the [Code] section
# (the WTS block, that morning) passed every local gate and cost a CI round trip. The static
# lints (check-installer.ps1, test-installer-lint.ps1) read the script; only ISCC compiles it.
# Gate mode (`/DGateCompile=1`) stores instead of compressing, so against the LAST STAGED
# payload this is seconds, and the output goes to a temp folder and is deleted: nothing built
# here is ever shipped. Needs a stage from a prior `build-release.ps1` and ISCC on the machine;
# either missing is reported as a yellow SKIP line naming what was not compiled (the run can
# still end PASSED, so read the yellow lines).
if (-not $failed) {
    Step 'installer.iss compiles (ISCC, gate mode, against the last staged payload)' {
        . (Join-Path $PSScriptRoot 'release-manifest-lib.ps1')
        $iscc = Find-ReleaseInnoSetupCompiler
        $stage = Join-Path $PSScriptRoot 'packaging\stage\x64'
        if (-not $iscc) {
            Write-Host '  SKIPPED - Inno Setup (ISCC.exe) is not installed here; the [Code] section was NOT compiled (winget install JRSoftware.InnoSetup)' -ForegroundColor Yellow
            $global:LASTEXITCODE = 0
            return
        }
        if (-not (Test-Path -LiteralPath (Join-Path $stage 'SageThumbs2K.exe') -PathType Leaf)) {
            Write-Host "  SKIPPED - no staged payload at $stage (build-release.ps1 makes one); the [Code] section was NOT compiled" -ForegroundColor Yellow
            $global:LASTEXITCODE = 0
            return
        }
        $ver = (Select-String -LiteralPath (Join-Path $PSScriptRoot '..\Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
        $compactOnly = if (Test-Path -LiteralPath (Join-Path $stage 'magick') -PathType Container) { '0' } else { '1' }
        $out = Join-Path ([System.IO.Path]::GetTempPath()) "st2k-iss-gate-$PID"
        New-Item -ItemType Directory -Force -Path $out | Out-Null
        try {
            & $iscc /Q "/DGateCompile=1" "/DAppVer=$ver" '/DArchitecture=x64' '/DStageDir=stage\x64' "/DCompactOnly=$compactOnly" '/DOutputSuffix=-gate' "/O$out" (Join-Path $PSScriptRoot 'packaging\installer.iss')
            $rc = $LASTEXITCODE
        } finally {
            Remove-Item -LiteralPath $out -Recurse -Force -ErrorAction SilentlyContinue
        }
        if ($rc -ne 0) { Write-Host "  ISCC exit $rc - the installer script does not compile; run scripts\build-release.ps1 for the full detail" -ForegroundColor Red }
        else { Write-Host ("  ok  installer.iss compiles ({0}, {1})" -f $ver, (Split-Path -Leaf (Split-Path -Parent $iscc))) }
        $global:LASTEXITCODE = $rc
    }
}

# Mirror .github/workflows/ci.yml -> build-test job, in order. A bare default-feature
# `cargo build --release` (no -p split) used to stand in for this and NEVER built the
# dll/dlghook packages or the webp-lossy/html-preview/hdr-capture/dll-i18n-subset feature
# combinations CI gates on — a compile error reachable only under one of those passed here
# and only surfaced after a CI round-trip. `--locked` (matching CI) also means a stale
# Cargo.lock now fails the build directly here, same as it would in CI.
if (-not $failed) { Step 'build production EXEs' { cargo build --release --locked -p sagethumbs2k --features webp-lossy,html-preview,hdr-capture } }
if (-not $failed) { Step 'build production slim DLL' { cargo build --release --locked -p sagethumbs2k-dll --features webp-lossy,dll-i18n-subset } }
if (-not $failed) { Step 'build dialog hook DLL'      { cargo build --release --locked -p sagethumbs2k-dlghook } }

# ~10 SECONDS over the whole corpus, and it is the cheap half of the staged gate. With
# ST2K_NO_MAGICK=1, every sample that used to stand on our own decoders must still stand on
# them. A file that quietly starts leaning on ImageMagick instead looks perfect on any dev box
# (magick_exe() falls back to C:\Program Files\ImageMagick*) and shows the stock icon in a real
# install, because the shipped bundle omits the rsvg/cairo/pango stack on purpose. That is
# exactly how 3.1.0 nearly shipped an SVG whose root element sits behind a licence comment: the
# only gate that could see it was test-staged-regression.ps1, twenty-five minutes into
# release.ps1, so it cost a whole release run to find. This sits directly behind the release
# EXE it tests, which is the earliest minute it can possibly run.
#   exit 2 = no corpus on this machine. Reported loudly as a SKIP, never folded into green.
if (-not $failed) {
    Step 'magick-free reliance (a format must not silently start needing ImageMagick)' {
        pwsh -NoProfile -File (Join-Path $PSScriptRoot 'check-magick-reliance.ps1')
        if ($LASTEXITCODE -eq 2) {
            Write-Host '  SKIPPED - no test corpus on this machine; scripts/build-corpus.ps1 builds one' -ForegroundColor Yellow
            $global:LASTEXITCODE = 0
        }
    }
}
# Guard: a build that DID succeed can still have regenerated Cargo.lock in a way `--locked`
# let through (e.g. a lockfile-only change unrelated to what was just compiled) — if it now
# differs from the COMMITTED lock, the committed lock is stale. CI runs `cargo-deny --locked`
# and REJECTS that (this is exactly what broke the 0.8.0 release: the version bump landed in
# Cargo.toml but the lock update never got committed). Catch it here, before the push.
if (-not $failed) {
    Step 'Cargo.lock in sync with Cargo.toml' {
        # Against HEAD, not the index: a regenerated lock that was only STAGED would
        # otherwise compare equal here and still not be in the commit being pushed.
        git diff --quiet HEAD -- Cargo.lock
        if ($LASTEXITCODE -ne 0) {
            Write-Host "  Cargo.lock was regenerated by the build — your COMMITTED lock is STALE." -ForegroundColor Yellow
            Write-Host "  CI's cargo-deny --locked check will reject it. Fix: git add Cargo.lock, commit (or --amend), then re-push." -ForegroundColor Yellow
        }
    }
}
# CI's build-test job builds a DEBUG cdylib and runs the suite against IT. These two steps are
# that job's two main steps (ci.yml "Build debug test DLL" / "Unit and integration tests"; the
# job's extra magick-subprocess test run is not repeated here), which is
# the whole reason they are here: this gate exists to catch what CI would catch, before the
# round trip. The COM integration tests LoadLibrary whichever cdylib sits in the profile
# directory they were built into, so the debug DLL built immediately above is the one they get.
#
# An earlier version of this comment justified the pairing by claiming debug and release differ
# behaviourally for the DLL, on the Media Foundation delay-load / video decode paths. MEASURED
# 2026-09-09, that is false for the test suite: `format_capability_claims` runs all 6 of its
# tests with nothing skipped in BOTH profiles. `video::media_foundation_available()` is a
# runtime LoadLibrary probe, identical either way, and src/build.rs applies /DELAYLOAD per-BIN,
# not per-profile. The CLAUDE.md 6.1 note that claim came from is about verify.ps1 rendering
# corpus samples through the debug st2k.exe, which is a different binary and a different
# question. Don't reintroduce the claim without re-measuring it.
if (-not $failed) { Step 'build debug test DLL (mirrors CI)' { cargo build --locked } }
# The suite records which tests read `..\test-corpus` (every access goes through
# `testcorpus::dir()`, which appends the calling test's name to this file), so the step after
# can re-run exactly those with the corpus made to vanish.
$corpusTouchLog = Join-Path ([System.IO.Path]::GetTempPath()) "st2k-corpus-touch-$PID.txt"
Remove-Item -LiteralPath $corpusTouchLog -ErrorAction SilentlyContinue
$env:ST2K_CORPUS_TOUCH_LOG = $corpusTouchLog
if (-not $failed) { Step 'unit + integration tests, debug profile (mirrors CI)' { cargo test --locked --tests } }
Remove-Item Env:\ST2K_CORPUS_TOUCH_LOG -ErrorAction SilentlyContinue

# THE CORPUS-ABSENT PASS (2026-09-19). CI has no `..\test-corpus` (it is a sibling of the repo,
# never in git), this machine does, so a test that reads a sample and unwraps the read passes
# here and fails there - three CI runs in a row went red that way on the day this was added,
# a class of failure the gate above is structurally blind to. This step makes the corpus
# vanish (`ST2K_CORPUS_ABSENT=1`: every `testcorpus::` accessor answers a path that does not
# exist) and re-runs ONLY the tests that touched it in the pass above, by exact name, so the
# CI shape is proven here in seconds rather than after a twenty-minute round trip. If no
# corpus exists on this machine the pass above already WAS the absent run.
if (-not $failed) {
    Step 'the corpus-reading tests with the corpus ABSENT (mirrors a CI checkout)' {
        $corpusDir = Join-Path (Split-Path -Parent $PSScriptRoot) '..\test-corpus'
        if (-not (Test-Path -LiteralPath $corpusDir -PathType Container)) {
            Write-Host '  no ..\test-corpus on this machine: the run above already ran without one' -ForegroundColor Yellow
            $global:LASTEXITCODE = 0
            return
        }
        if (-not (Test-Path -LiteralPath $corpusTouchLog -PathType Leaf)) {
            Write-Host "  no touch log at $corpusTouchLog - testcorpus::dir() never ran; the instrument is broken, not the code" -ForegroundColor Red
            $global:LASTEXITCODE = 1
            return
        }
        $names = @(Get-Content -LiteralPath $corpusTouchLog | Where-Object { $_ -and $_ -ne '<unnamed>' } | Sort-Object -Unique)
        $unnamed = @(Get-Content -LiteralPath $corpusTouchLog | Where-Object { $_ -eq '<unnamed>' }).Count
        Remove-Item -LiteralPath $corpusTouchLog -ErrorAction SilentlyContinue
        if ($names.Count -eq 0) {
            Write-Host '  the touch log is empty: dozens of tests read the corpus, so the instrument is broken' -ForegroundColor Red
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host ("  {0} tests read the corpus; re-running them with it absent" -f $names.Count)
        if ($unnamed) { Write-Host ("  ({0} reads came from worker threads and cannot be attributed; those tests are covered only where the read happens on the test thread)" -f $unnamed) -ForegroundColor Yellow }
        $env:ST2K_CORPUS_ABSENT = '1'
        try {
            $lines = cargo test --locked --tests -- --exact @names 2>&1 | ForEach-Object { "$_" }
            $rc = $LASTEXITCODE
        } finally {
            Remove-Item Env:\ST2K_CORPUS_ABSENT -ErrorAction SilentlyContinue
        }
        $lines | Where-Object { $_ -match '^test .*(FAILED|panicked)|^test result|panicked at|NOT MEASURED' } | ForEach-Object { Write-Host "  $_" }
        # A name that matched no test proves nothing: the exact filters MUST have run as many
        # tests as were recorded, or the instrument (not the code) is what failed.
        $ran = 0
        foreach ($l in $lines) { if ($l -match 'test result: \w+\. (\d+) passed; (\d+) failed') { $ran += [int]$Matches[1] + [int]$Matches[2] } }
        if ($rc -eq 0 -and $ran -lt $names.Count) {
            Write-Host ("  only {0} of the {1} recorded tests ran under --exact; the recorded names do not match the suite (a garbled touch log?)" -f $ran, $names.Count) -ForegroundColor Red
            $rc = 1
        }
        $global:LASTEXITCODE = $rc
    }
}

# A SECOND, release-profile run of the same suite used to sit here and it cost ~8 minutes of
# every push. It now lives in `.github/workflows/release-profile-tests.yml` and runs as a GATE
# inside `release.ps1` at step [3b/6], dispatched and waited on.
#
# Moving it was not a downgrade. As a local step it ran only on the machine whose pre-push hook
# is installed, so a pull request, a push from another machine, and `main` itself were never
# covered - while being the only place the shipped-shape artifacts were tested at all. It now
# gates the release itself, which is the moment that coverage actually protects somebody, and
# it costs an ordinary push nothing. It is deliberately NOT a ci.yml job: a release-profile run
# is a full LTO build, and `test-architecture-release-contract.ps1` keeps per-push validation
# in the fast debug profile on purpose.
#
# The release BUILD steps above stay here, because a release-only link failure (panic="abort"
# + LTO, "unresolved external symbol") is worth catching before the push and costs seconds on
# a warm cache rather than minutes.
if (-not $failed) { Step 'clippy (-D warnings)'  { cargo clippy --release --all-targets -- -D warnings } }
# Rustfmt used to run HERE, and was missing entirely until 2026-08-05 (this gate printed
# "safe to push" on a commit CI then failed on formatting alone). It now runs FIRST, before
# any build — see the ordering note at the top for why, and what it cost to learn.

# Mirror the `deny` job — only if cargo-deny is installed locally (deny.toml at repo root).
# A missing tool used to fall straight through to "PREFLIGHT PASSED" with no printed line at
# all, so advisories/licenses/bans went unchecked on a machine that never noticed the gate was
# absent (docs/DEVELOPMENT_GOTCHAS.md names this exact failure class from a shipped incident).
if (-not $failed) {
    if (Get-Command cargo-deny -ErrorAction SilentlyContinue) {
        Step 'cargo-deny' { cargo deny check }
    } else {
        Write-Host ""
        Write-Host "==> cargo-deny" -ForegroundColor Cyan
        Write-Host "SKIPPED: cargo-deny is not installed (cargo install cargo-deny) — advisories, licenses and bans were NOT checked locally." -ForegroundColor Yellow
    }
}

Write-Host ""
if ($failed) {
    Write-Host "PREFLIGHT FAILED — push blocked. (bypass once with: git push --no-verify)" -ForegroundColor Red
    exit 1
}
Write-Host "PREFLIGHT PASSED — safe to push." -ForegroundColor Green
exit 0
