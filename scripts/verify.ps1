<#
  verify.ps1 — the ONE verification entry point (owner directive, 2026-07-20).

  Exists because the verify phase of a small change kept ballooning into hours of
  ad-hoc loops. This script encodes the whole ladder ONCE, fail-fast, with per-stage
  timing, so a session runs ONE command instead of improvising five.

      pwsh scripts\verify.ps1 -Fast                       # check + lib tests            (~15 s)
      pwsh scripts\verify.ps1                             # + debug build + ALL tests    (~1 min)
      pwsh scripts\verify.ps1 -Lint                       # + clippy -D warnings, cargo-deny,
                                                          #   cargo-machete (mirrors CI locally)
      pwsh scripts\verify.ps1 -Samples "archive-*"        # + render matching corpus samples,
                                                          #   asserting _expected-fail.txt
      pwsh scripts\verify.ps1 -Release                    # + the one §6.0 release pair  (~3 min)
      pwsh scripts\verify.ps1 -Release -Install           # + elevated dev install + hash check

  Rules this encodes (so nobody re-learns them):
   * tests\com_roundtrip.rs asserts the DEBUG cdylib exists -> `cargo build` MUST
     precede `cargo test` (a bare `cargo test` after only `cargo check` fails all 5
     with "cdylib not built").
   * The release pair is exactly TWO builds in THIS order: bare `cargo build
     --release` (DLL + EXEs), then `--bin SageThumbs2K --features html-preview`
     (a bare release build alone silently drops the EXE's html-preview tab rows).
   * -Samples asserts EXPECTATIONS, not just exit codes: samples listed in
     <corpus>\_expected-fail.txt MUST fail (stock icon is their correct result);
     everything else matched MUST render. No more hand-written per-file loops.
   * Full-corpus regression.ps1 is NOT part of this ladder on purpose: run it only
     when FORMATS/decoders change broadly or before a release. For a scoped change,
     -Samples over the affected files is the whole point.
#>

param(
    # check + `cargo test --lib` only — the inner-loop gate while iterating.
    [switch]$Fast,
    # Corpus filename wildcard (e.g. "archive-*", "*.psd") to render + assert.
    [string]$Samples,
    # The §6.0 release build pair (exactly once, correct order).
    [switch]$Release,
    # Elevated dev install (scripts\install.ps1) + installed==built hash check.
    [switch]$Install,
    # Static analysis, mirroring CI's gates locally (run before any push):
    # clippy -D warnings (all targets), cargo-deny (advisories/licenses per
    # deny.toml), cargo-machete (unused deps). ~1 min warm.
    [switch]$Lint
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
    # CI gates clippy on --release; debug clippy catches the same lints faster and
    # shares the ladder's debug cache. -D warnings: the tree is kept warning-clean,
    # intentional exceptions carry a local #[allow] with a reason.
    Stage 'clippy -D warnings' {
        cargo clippy --workspace --all-targets --quiet -- -D warnings 2>&1 |
            Where-Object { $_ -match 'warning|error' } | Write-Host
    }
    # Advisories + licenses + bans against deny.toml — same check the `deny` CI job
    # runs, minus the round-trip to GitHub.
    Stage 'cargo deny' {
        cargo deny check --hide-inclusion-graph 2>&1 |
            Where-Object { $_ -match 'error|warning\[|advisories|licenses|bans|sources' } |
            Select-Object -First 12 | Write-Host
    }
    # Unused dependencies (a dep that compiles in but nothing references).
    Stage 'cargo machete' {
        cargo machete 2>&1 | Where-Object { $_ -notmatch '^Analyzing|^Done' } | Write-Host
        if ($LASTEXITCODE -eq 1) { Write-Host '[verify] unused dependencies found' -ForegroundColor Red }
        else { $global:LASTEXITCODE = 0 }
    }
}

if ($Fast) {
    Stage 'cargo test --lib' { cargo test --lib --quiet 2>&1 | Select-Object -Last 3 | Write-Host }
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
        $manifest = Join-Path $corpus '_expected-fail.txt'
        if (Test-Path $manifest) {
            Get-Content $manifest | ForEach-Object {
                $line = $_.Trim()
                if ($line -and -not $line.StartsWith('#')) { $expectFail[$line] = $true }
            }
        }
        $files = Get-ChildItem $corpus -Filter $Samples -File |
                 Where-Object { $_.Name -notlike '_*' -and $_.Name -ne 'contact.png' }
        if (-not $files) { Write-Host "[verify] no corpus files match '$Samples'" -ForegroundColor Red; exit 1 }
        $out = Join-Path ([System.IO.Path]::GetTempPath()) 'st2k-verify'
        New-Item -ItemType Directory -Force $out | Out-Null
        $st2k = Join-Path $target 'debug\st2k.exe'
        $bad = 0
        foreach ($f in $files) {
            $png = Join-Path $out ($f.BaseName + '-' + $f.Extension.TrimStart('.') + '.png')
            & $st2k thumbnail $f.FullName $png 256 *> $null
            $rendered = ($LASTEXITCODE -eq 0) -and (Test-Path $png)
            $wantFail = $expectFail.ContainsKey($f.Name)
            if ($rendered -and -not $wantFail)      { Write-Host ("  OK        {0}" -f $f.Name) }
            elseif (-not $rendered -and $wantFail)  { Write-Host ("  OK (fail) {0}  [expected no thumbnail]" -f $f.Name) }
            elseif ($rendered -and $wantFail)       { Write-Host ("  BAD       {0}  rendered but is expected-fail" -f $f.Name) -ForegroundColor Red; $bad++ }
            else                                    { Write-Host ("  BAD       {0}  failed to render" -f $f.Name) -ForegroundColor Red; $bad++ }
        }
        if ($bad -gt 0) { Write-Host "[verify] $bad sample expectation(s) violated" -ForegroundColor Red; exit 1 }
        $global:LASTEXITCODE = 0
    }
}

# ---- release pair ----------------------------------------------------------
if ($Release) {
    Stage 'release: DLL + EXEs' { cargo build --release --quiet 2>&1 | Write-Host }
    Stage 'release: EXE html-preview' { cargo build --release --quiet --bin SageThumbs2K --features html-preview 2>&1 | Write-Host }
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
        # Restart the screenshot-hotkey daemon we killed, UNELEVATED (an elevated
        # daemon is deaf to Settings per UIPI) and only if its autostart entry exists.
        $run = Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -ErrorAction SilentlyContinue
        if ($run -and $run.SageThumbs2KScreenshot) {
            Start-Process cmd -WindowStyle Hidden -ArgumentList '/c', $run.SageThumbs2KScreenshot
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
