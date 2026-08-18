<#
  verify.ps1 — the ONE verification entry point (owner directive, 2026-07-20).

  Exists because the verify phase of a small change kept ballooning into hours of
  ad-hoc loops. This script encodes the whole ladder ONCE, fail-fast, with per-stage
  timing, so a session runs ONE command instead of improvising five.

      pwsh scripts\verify.ps1 -Fast                       # check + lib/app unit tests   (~20 s)
      pwsh scripts\verify.ps1                             # + debug build + ALL tests    (~1 min)
      pwsh scripts\verify.ps1 -Lint                       # + rustfmt, clippy -D warnings,
                                                          #   cargo-deny, cargo-machete, and
                                                          #   ALL 10 of ci.yml's `consistency`
                                                          #   job scripts (mirrors CI in full)
      pwsh scripts\verify.ps1 -Samples "archive-*"        # + render matching corpus samples,
                                                          #   asserting _expected-fail.txt
      pwsh scriptserify.ps1 -Perf                       # + are we slower than Windows' own
                                                          #   codec on any format? (needs the
                                                          #   release st2k + test-corpus-real)
      pwsh scripts\verify.ps1 -Release                    # + the same 3 builds ci.yml runs (~3 min)
      pwsh scripts\verify.ps1 -Release -Install           # + elevated dev install + hash check

  Rules this encodes (so nobody re-learns them):
   * tests\com_roundtrip.rs asserts the DEBUG cdylib exists -> `cargo build` MUST
     precede `cargo test` (a bare `cargo test` after only `cargo check` fails all 5
     with "cdylib not built").
   * -Release is the same THREE --locked, package-split builds ci.yml's build-test
     job runs, in the same order: `-p sagethumbs2k --features webp-lossy,html-preview,
     hdr-capture`, `-p sagethumbs2k-dll --features webp-lossy,dll-i18n-subset`, then
     `-p sagethumbs2k-dlghook`. A bare `cargo build --release` (default features, no
     -p split) exercises none of those feature combinations or the dll/dlghook
     packages, so a compile error reachable only under one of them used to pass this
     ladder locally and only surface after a CI round-trip.
   * -Samples asserts EXPECTATIONS, not just exit codes: samples listed in
     <corpus>\_expected-fail.txt MUST fail (stock icon is their correct result);
     everything else matched MUST render. No more hand-written per-file loops.
   * -Perf answers the ONE question the correctness gates structurally cannot: not "did it
     render" or "did it render the right picture", but "is it SLOW". A shipped AVIF regression
     (2026-08-18) passed regression.ps1's three gates AND perf.ps1's flat 3000 ms threshold
     while costing ~5x Microsoft's own AV1 codec, because nothing ever compared us to the OS.
     check-decode-speed.ps1 does. NOT in CI: it needs test-corpus-real, a sibling of the
     repo that is not in git, so it is a local/pre-release gate like perf.ps1.
   * Full-corpus regression.ps1 is NOT part of this ladder on purpose: run it only
     when FORMATS/decoders change broadly or before a release. For a scoped change,
     -Samples over the affected files is the whole point.
#>

param(
    # check + library/app unit tests only — the inner-loop gate while iterating.
    [switch]$Fast,
    # Corpus filename wildcard (e.g. "archive-*", "*.psd") to render + assert.
    [string]$Samples,
    # The 3 --locked, package-split release builds CI's build-test job runs (exactly
    # once, correct order) — see the header comment above.
    [switch]$Release,
    # Elevated dev install (scripts\install.ps1) + installed==built hash check.
    [switch]$Install,
    # Static analysis, mirroring CI's gates locally IN FULL (run before any push):
    # rustfmt --check, clippy -D warnings (all targets), cargo-deny (advisories/
    # licenses per deny.toml), cargo-machete (unused deps), plus all 10 of ci.yml's
    # `consistency`-job scripts (check-consistency, the 3 architecture/freshness
    # contracts, email-rule, registration-symmetry, release-size, release-pipeline,
    # installer-lint, msix-integrity, vendored-exr) and check-locale-keys (which
    # verify covers but ci.yml's consistency job still does not). ~1-2 min warm.
    [switch]$Lint,
    # Decode SPEED, measured against the OS: for every format WIC can also decode, is our
    # decode materially slower than Windows'? Needs the RELEASE st2k and test-corpus-real.
    [switch]$Perf
)

$ErrorActionPreference = 'Stop'
$root   = Split-Path $PSScriptRoot -Parent                     # project root (Cargo.toml)
$corpus = Join-Path (Split-Path $root -Parent) 'test-corpus'   # sibling of project root
$target = 'D:\st2k-target'                                     # fixed in .cargo\config.toml
Set-Location $root

$script:timings = @()
function Stage([string]$name, [scriptblock]$body) {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    Write-Host "[verify] $name..." -ForegroundColor Cyan
    & $body
    if ($LASTEXITCODE -ne 0 -and $null -ne $LASTEXITCODE) {
        Write-Host "[verify] FAILED at: $name (exit $LASTEXITCODE)" -ForegroundColor Red
        exit 1
    }
    $sw.Stop()
    $script:timings += "{0,-28} {1,7:n1} s" -f $name, $sw.Elapsed.TotalSeconds
}

# ---- ladder ---------------------------------------------------------------
Stage 'cargo check' { cargo check --quiet 2>&1 | Where-Object { $_ -match 'error|warning' } | Write-Host }

# ---- static analysis (mirrors CI's clippy/deny gates; catch it BEFORE the push) ----
if ($Lint) {
    # Formatting. Cheapest gate there is and it keeps diffs about the CHANGE, not about
    # whitespace — `cargo fmt --all` fixes anything this reports.
    Stage 'cargo fmt --check' {
        # CAPTURE FIRST, then trim. Piping cargo straight into `Select-Object -First N` stops the
        # pipeline as soon as N objects arrive, which kills cargo early and loses its exit code —
        # so this gate reported GREEN precisely when the diff was longer than the cap, i.e. when
        # it mattered. That shipped a formatting break past the local gate and into CI (1.4.0).
        $out = cargo fmt --all --check 2>&1
        $code = $LASTEXITCODE
        $out | Select-Object -First 40 | Write-Host
        $global:LASTEXITCODE = $code
    }
    # CI gates clippy in this same debug profile so it shares the test ladder's
    # cache. -D warnings: the tree is kept warning-clean,
    # intentional exceptions carry a local #[allow] with a reason.
    Stage 'clippy -D warnings' {
        cargo clippy --workspace --all-targets --quiet -- -D warnings 2>&1 |
            Where-Object { $_ -match 'warning|error' } | Write-Host
    }
    # Advisories + licenses + bans against deny.toml — same check the `deny` CI job
    # runs, minus the round-trip to GitHub.
    Stage 'cargo deny' {
        # Same capture-then-trim discipline as the fmt stage above: a trailing
        # `Select-Object -First N` in the pipeline would swallow cargo-deny's exit code on a
        # long report, turning a real advisory into a silent pass.
        $out = cargo deny check --hide-inclusion-graph 2>&1
        $code = $LASTEXITCODE
        $out | Where-Object { $_ -match 'error|warning\[|advisories|licenses|bans|sources' } |
            Select-Object -First 12 | Write-Host
        $global:LASTEXITCODE = $code
    }
    # Unused dependencies (a dep that compiles in but nothing references).
    Stage 'cargo machete' {
        cargo machete 2>&1 | Where-Object { $_ -notmatch '^Analyzing|^Done' } | Write-Host
        if ($LASTEXITCODE -eq 1) { Write-Host '[verify] unused dependencies found' -ForegroundColor Red }
        else { $global:LASTEXITCODE = 0 }
    }
    # Same script the `consistency` CI job runs (and release.ps1 runs before tagging):
    # untracked referenced assets, format counts, AppxManifest version, and locale key
    # parity. Cheap (no build) and catches the classes that only surface on a clean
    # checkout or in another language.
    Stage 'check-consistency' {
        & (Join-Path $PSScriptRoot 'check-consistency.ps1')
    }
    # Dedicated locale key-parity gate: a locale missing keys vs en.toml (or carrying
    # keys en.toml doesn't have) compiles fine and just falls back to English at
    # runtime, so nothing else in the build catches it.
    Stage 'check-locale-keys' {
        & (Join-Path $PSScriptRoot 'check-locale-keys.ps1')
    }
    # Cheap offline PowerShell contract suites added alongside the ARM64 and
    # dependency-maintenance paths. CI runs the same scripts in its consistency
    # job; keeping them here prevents a local green -Lint from missing script drift.
    Stage 'architecture + freshness contracts' {
        foreach ($scriptName in @(
            'test-architecture-release-contract.ps1',
            'test-dev-architecture.ps1',
            'test-magick-dependency-freshness.ps1'
        )) {
            & pwsh -NoProfile -File (Join-Path $PSScriptRoot $scriptName)
            if ($LASTEXITCODE -ne 0) { throw "$scriptName failed" }
        }
    }
    # The reply-address rule is implemented once per runtime (Pascal / Rust / JS); this runs
    # each against the one shared table so they cannot drift apart unnoticed. Locally ISCC is
    # usually present, so unlike CI this covers the Pascal leg too.
    Stage 'email-rule agreement' {
        & pwsh -NoProfile -File (Join-Path $PSScriptRoot 'check-email-rule.ps1')
        if ($LASTEXITCODE -ne 0) { throw 'check-email-rule.ps1 failed' }
    }
    # Taking a shared `shellex` slot without recording who held it wipes another product's
    # thumbnail registration for good. Nothing else can see that: it compiles, it tests green,
    # and it only shows up on a stranger's machine. See check-registration-symmetry.ps1.
    Stage 'registration symmetry' {
        & pwsh -NoProfile -File (Join-Path $PSScriptRoot 'check-registration-symmetry.ps1')
        if ($LASTEXITCODE -ne 0) { throw 'check-registration-symmetry.ps1 failed' }
    }
    # The remaining 5 of ci.yml's 10 `consistency`-job scripts that used to be missing
    # from -Lint entirely: a green local -Lint said nothing about them, so a regression
    # only surfaced after a CI round-trip (release-size policy, release provenance/
    # curated-notes, installer cleanup/uninstaller safety, MSIX signature/signer/
    # identity, and the vendored-exr drift check). All five are dependency-free —
    # they build their own scratch fixtures rather than needing a prior release build.
    Stage 'release/installer/MSIX consistency contracts' {
        foreach ($scriptName in @(
            'test-release-size.ps1',
            'test-release-pipeline.ps1',
            'test-installer-lint.ps1',
            'test-msix-integrity.ps1',
            'check-vendored-exr.ps1'
        )) {
            & pwsh -NoProfile -File (Join-Path $PSScriptRoot $scriptName)
            if ($LASTEXITCODE -ne 0) { throw "$scriptName failed" }
        }
    }
}

if ($Fast) {
    Stage 'cargo test --lib --bin SageThumbs2K' {
        cargo test --lib --bin SageThumbs2K --quiet 2>&1 |
            Where-Object { $_ -match 'test result|FAILED|error' } | Write-Host
    }
} else {
    # Debug build FIRST: the COM round-trip tests load the debug cdylib from disk.
    Stage 'cargo build (debug)' { cargo build --quiet 2>&1 | Write-Host }
    Stage 'cargo test (all)' { cargo test --quiet 2>&1 | Where-Object { $_ -match 'test result|FAILED|error' } | Write-Host }
}

# ---- corpus samples with expectations -------------------------------------
if ($Samples) {
    Stage "samples: $Samples" {
        # The renders below use $LASTEXITCODE per file; the stage-level check is
        # driven by our own $bad counter (exit 1 at the end on any mismatch).
        if (-not (Test-Path (Join-Path $target 'debug\st2k.exe'))) {
            cargo build --quiet --bin st2k 2>&1 | Write-Host
        }
        $expectFail = @{}
        $sizeGated  = @{}
        # These pinned libheif fixtures specifically guard transparency routing,
        # not merely format recognition. A rendered-but-opaque PNG is a regression.
        $expectAlpha = @{
            'sample-heic-alpha.heic' = $true
            'sample-avif-alpha.avif' = $true
        }
        $manifest = Join-Path $corpus '_expected-fail.txt'
        if (Test-Path $manifest) {
            Get-Content $manifest | ForEach-Object {
                $line = $_.Trim()
                if (-not $line -or $line.StartsWith('#')) { return }
                # `size:<name>` = "must not render ONLY because it exceeds the MaxSize setting".
                # That expectation is machine state, not code, so it's asserted conditionally.
                if ($line.StartsWith('size:')) {
                    $line = $line.Substring(5).Trim()
                    $sizeGated[$line] = $true
                }
                $expectFail[$line] = $true
            }
        }
        # The effective "skip files bigger than this" limit (settings.rs DEFAULT_MAX_FILE_MB = 256,
        # overridable per user in HKCU). A size-gated negative sample only proves anything while
        # the machine's limit is actually below the sample — otherwise rendering it is CORRECT,
        # and asserting a failure would just cry wolf on whoever raised the setting.
        $maxMb = 256
        $reg = Get-ItemProperty 'HKCU:\Software\SageThumbs2K' -ErrorAction SilentlyContinue
        if ($null -ne $reg -and $null -ne $reg.MaxSize) { $maxMb = [int]$reg.MaxSize }
        $maxBytes = if ($maxMb -le 0) { [long]::MaxValue } else { [long]$maxMb * 1MB }
        $files = Get-ChildItem $corpus -Filter $Samples -File |
                 Where-Object { $_.Name -notlike '_*' -and $_.Name -ne 'contact.png' }
        if (-not $files) { Write-Host "[verify] no corpus files match '$Samples'" -ForegroundColor Red; exit 1 }
        $out = Join-Path ([System.IO.Path]::GetTempPath()) 'st2k-verify'
        New-Item -ItemType Directory -Force $out | Out-Null
        $st2k = Join-Path $target 'debug\st2k.exe'
        $bad = 0
        foreach ($f in $files) {
            if ($sizeGated.ContainsKey($f.Name) -and $f.Length -le $maxBytes) {
                Write-Host ("  SKIP      {0}  [MaxSize is {1} MB — sample is under the limit, so it SHOULD render]" -f $f.Name, $maxMb) -ForegroundColor DarkGray
                continue
            }
            $png = Join-Path $out ($f.BaseName + '-' + $f.Extension.TrimStart('.') + '.png')
            & $st2k thumbnail $f.FullName $png 256 *> $null
            $rendered = ($LASTEXITCODE -eq 0) -and (Test-Path $png)
            $wantFail = $expectFail.ContainsKey($f.Name)
            $alphaOk = $true
            if ($rendered -and $expectAlpha.ContainsKey($f.Name)) {
                Add-Type -AssemblyName System.Drawing
                $bitmap = [System.Drawing.Bitmap]::FromFile($png)
                try {
                    $hasTransparent = $false
                    $hasOpaque = $false
                    for ($y = 0; $y -lt $bitmap.Height -and -not ($hasTransparent -and $hasOpaque); $y++) {
                        for ($x = 0; $x -lt $bitmap.Width -and -not ($hasTransparent -and $hasOpaque); $x++) {
                            $alpha = $bitmap.GetPixel($x, $y).A
                            if ($alpha -lt 255) { $hasTransparent = $true }
                            if ($alpha -eq 255) { $hasOpaque = $true }
                        }
                    }
                    $alphaOk = $hasTransparent -and $hasOpaque
                } finally {
                    $bitmap.Dispose()
                }
            }
            if ($rendered -and -not $wantFail -and $alphaOk) { Write-Host ("  OK        {0}" -f $f.Name) }
            elseif ($rendered -and -not $wantFail)  { Write-Host ("  BAD       {0}  rendered but lost its alpha range" -f $f.Name) -ForegroundColor Red; $bad++ }
            elseif (-not $rendered -and $wantFail)  { Write-Host ("  OK (fail) {0}  [expected no thumbnail]" -f $f.Name) }
            elseif ($rendered -and $wantFail)       { Write-Host ("  BAD       {0}  rendered but is expected-fail" -f $f.Name) -ForegroundColor Red; $bad++ }
            else                                    { Write-Host ("  BAD       {0}  failed to render" -f $f.Name) -ForegroundColor Red; $bad++ }
        }
        if ($bad -gt 0) { Write-Host "[verify] $bad sample expectation(s) violated" -ForegroundColor Red; exit 1 }
        $global:LASTEXITCODE = 0
    }
}

# ---- release pair ----------------------------------------------------------
# The exact three feature-gated, package-split builds ci.yml runs (build-test job),
# in the same order, all --locked. A bare `cargo build --release` (default features,
# no -p split) does NOT exercise webp-lossy/html-preview/hdr-capture/dll-i18n-subset,
# nor the separate dll/dlghook packages — a compile error reachable only under one of
# those would pass this ladder locally and only surface after a CI round-trip.
if ($Perf) {
    Stage 'decode speed (vs Windows, and vs ourselves)' {
        & pwsh -NoProfile -File (Join-Path $PSScriptRoot 'check-decode-speed.ps1')
        if ($LASTEXITCODE -ne 0) { throw 'check-decode-speed.ps1 failed' }
    }
}

if ($Release) {
    Stage 'release: production EXEs' {
        cargo build --release --locked --quiet -p sagethumbs2k --features webp-lossy,html-preview,hdr-capture 2>&1 | Write-Host
    }
    Stage 'release: production slim DLL' {
        cargo build --release --locked --quiet -p sagethumbs2k-dll --features webp-lossy,dll-i18n-subset 2>&1 | Write-Host
    }
    Stage 'release: dialog hook DLL' {
        cargo build --release --locked --quiet -p sagethumbs2k-dlghook 2>&1 | Write-Host
    }
    # Every window/content-type actually follows the light/dark setting (the QuickLook-#1797
    # class of bug: one paint path never asks the theme, and only ever shows up in one theme).
    # Needs the just-built release EXE, so it rides -Release rather than every plain `verify.ps1`.
    Stage 'theme shots' {
        & pwsh -NoProfile -File (Join-Path $PSScriptRoot 'check-theme-shots.ps1')
        if ($LASTEXITCODE -ne 0) { throw 'check-theme-shots.ps1 failed' }
    }
    # Did this release change any PICTURE? Nothing else in the ladder can ask that: the
    # render sweep only wants a non-empty PNG, so a decoder that succeeds at drawing the
    # wrong thing passes everything (2.0.0's XCF layer budget did exactly that). Compares
    # every corpus sample against the previous RELEASED build in dist\. Exit 2 = could not
    # run (no python/Pillow, no baseline release), which is a skip, not a failure.
    Stage 'render parity vs the last release' {
        & pwsh -NoProfile -File (Join-Path $PSScriptRoot 'check-render-parity.ps1')
        if ($LASTEXITCODE -eq 1) { throw 'check-render-parity.ps1 failed' }
    }
}

# ---- elevated install + installed==built proof -----------------------------
if ($Install) {
    Stage 'elevated install' {
        $dll = Join-Path $target 'release\sagethumbs2k.dll'
        if (-not (Test-Path $dll)) { Write-Host '[verify] no release DLL — run with -Release' -ForegroundColor Red; exit 1 }
        # The whole §6 recipe in ONE elevation: the DLL is LOCKED while Explorer /
        # the thumbnail (dllhost) + preview (prevhost) hosts have it loaded, and a
        # locked copy makes install.ps1 die invisibly in its elevated window while
        # regsvr32 re-registers the OLD dll (exactly the failure the hash check
        # below exists to catch). So: kill the hosts, install, clear the thumbnail
        # cache (needs Explorer dead anyway), all elevated — then restart Explorer
        # from THIS unelevated session so the shell doesn't come back elevated.
        # The project path contains a SPACE: never thread it through a nested
        # -Command string (quoting mangles it into a pwsh usage error, exit 64).
        # Write the elevated payload to a temp SCRIPT FILE and -File that instead.
        $installPs1 = Join-Path $root 'scripts\install.ps1'
        $log = Join-Path ([System.IO.Path]::GetTempPath()) 'st2k-install.log'
        $payload = Join-Path ([System.IO.Path]::GetTempPath()) 'st2k-install-inner.ps1'
        @"
taskkill /f /im explorer.exe 2>`$null | Out-Null
taskkill /f /im dllhost.exe 2>`$null | Out-Null
taskkill /f /im prevhost.exe 2>`$null | Out-Null
# The screenshot-hotkey daemon runs as the INSTALLED SageThumbs2K.exe and locks it.
taskkill /f /im SageThumbs2K.exe 2>`$null | Out-Null
taskkill /f /im st2k.exe 2>`$null | Out-Null
& pwsh -NoProfile -File '$installPs1' *> '$log'
`$code = `$LASTEXITCODE
Remove-Item "`$env:LOCALAPPDATA\Microsoft\Windows\Explorer\thumbcache_*.db" -Force -ErrorAction SilentlyContinue
exit `$code
"@ | Set-Content $payload -Encoding UTF8
        $p = Start-Process pwsh -Verb RunAs -Wait -PassThru -ArgumentList @('-NoProfile', '-File', $payload)
        Start-Process explorer.exe   # unelevated shell restart
        # Bring the resident helper back, exactly as installer.iss does after a real setup
        # (its `--heal-hotkeys` [Run] entry). The install killed it to replace the EXE, and
        # nothing else restarts it until the next logon.
        #
        # UNELEVATED on purpose: a daemon that inherits this script's elevation is UIPI-deaf
        # to a normal Settings window, so later hotkey changes silently stop applying. Same
        # reason installer.iss marks its entry runasoriginaluser.
        #
        # This used to be gated on the SageThumbs2KScreenshot autostart entry, which only
        # exists when the screenshot HOTKEY is on — but the daemon also owns the Space hook
        # that Quick preview needs (`daemon_wanted` = screenshots OR custom hotkey OR
        # preview). Anyone running Quick preview with the hotkey off therefore got no daemon
        # at all after a dev reinstall, and Space stayed dead until they opened Settings,
        # which calls heal_if_wanted itself. Letting the app decide covers every case and is
        # a silent no-op when nothing wants it.
        $appExe = 'C:\Program Files\SageThumbs2K\SageThumbs2K.exe'
        if (Test-Path $appExe) {
            Start-Process -FilePath $appExe -ArgumentList '--heal-hotkeys' -WindowStyle Hidden
        }
        if ($p.ExitCode -ne 0) {
            Write-Host "[verify] elevated install exited $($p.ExitCode) — log:" -ForegroundColor Red
            if (Test-Path $log) { Get-Content $log | Select-Object -Last 10 | Write-Host }
            exit 1
        }
        $installed = 'C:\Program Files\SageThumbs2K\sagethumbs2k.dll'
        $a = (Get-FileHash $dll).Hash
        $b = (Get-FileHash $installed).Hash
        if ($a -ne $b) {
            Write-Host "[verify] installed DLL != built DLL — install did not take" -ForegroundColor Red
            exit 1
        }
        Write-Host "  installed == built (SHA256 $($a.Substring(0,12))...)"
        $global:LASTEXITCODE = 0
    }
}

Write-Host "`n[verify] ALL GREEN" -ForegroundColor Green
$script:timings | ForEach-Object { Write-Host "  $_" }
