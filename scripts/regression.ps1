<#
  regression.ps1 — render every test-corpus sample through st2k and report which
  formats thumbnail successfully. Doubles as a CI/pipeline gate.

      pwsh scripts\regression.ps1                 # check against the baseline
      pwsh scripts\regression.ps1 -UpdateBaseline # accept the current pass set as the new baseline

  EXIT CODES (so this can fail a pipeline):
    0  no regression  — every extension in the baseline still renders.
    1  REGRESSION     — at least one previously-passing extension NEWLY fails
                        (a VALID sample is still in the corpus but no longer
                        thumbnails, CONFIRMED on a calm sequential retry).

  FALSE-ALARM GUARD: the parallel render can race build-corpus.ps1 while it is
  (re)writing samples, and a failed network download can leave an HTML error page
  or a truncated stub in place — either would make a good format look "broken."
  So before failing, each suspect is vetted: samples that are self-evidently junk
  (empty / tiny / HTML error page / Git-LFS pointer) are reported as "corpus
  INCOMPLETE" (re-run build-corpus.ps1), and the rest are re-rendered ONCE more
  sequentially (the corpus has settled by then). Only a valid sample that STILL
  fails is a real regression. This keeps a mid-flight corpus from crying wolf.

  UNTESTED FORMATS: build-corpus.ps1 records registered formats it has no REAL
  sample for in <corpus>\_no-real-sample.txt (before 2026-07-08 those ~90 were
  renamed-PNG fakes that falsely passed via PNG sniffing — mostly Camera RAW +
  the obscure magick-read-only tail). They are reported as UNTESTED here, are
  not in the baseline, and a PASS total does not cover them.

  BASELINE: scripts\regression-baseline.txt — the sorted list of extensions that
  are expected to render. It is the source of truth for "what passed before."
    * If it is ABSENT, the first run GENERATES it from the current corpus pass set
      and exits 0 (nothing to diff against yet).
    * NEW passing extensions (rendering now, not in the baseline) are reported but
      do NOT fail the run and do NOT auto-update the file — adding them is an
      intentional, reviewed change: re-run with -UpdateBaseline (or edit the file).
    * Likewise an intentional REMOVAL (a format you deliberately dropped) requires
      an explicit baseline edit / -UpdateBaseline; it will be flagged until then.

  Still prints the human PASS n/total summary and builds the labelled contact
  sheet (contact.png) so the thumbnails can be eyeballed at once.
#>
param(
    [string]$Corpus = "$PSScriptRoot\..\..\test-corpus",
    [int]$Size = 96,
    # Persist the current pass set as the new baseline instead of diffing.
    [switch]$UpdateBaseline
)
$ErrorActionPreference = 'Continue'

$baselineFile = "$PSScriptRoot\regression-baseline.txt"

$st2k = Join-Path (& "$PSScriptRoot\_targetdir.ps1") 'release\st2k.exe'
if (-not (Test-Path $st2k)) { throw "st2k.exe not built (cargo build --release --bin st2k)" }
$magick = (Get-ChildItem 'C:\Program Files\ImageMagick*\magick.exe' -EA SilentlyContinue | Select-Object -First 1).FullName

$render = "$Corpus\_render"
if (Test-Path $render) { Remove-Item $render -Recurse -Force }
New-Item -ItemType Directory -Force $render | Out-Null

$skipExt = '.md', '.txt'
$files = Get-ChildItem $Corpus -File | Where-Object { $_.Name -notlike '_*' -and $skipExt -notcontains $_.Extension.ToLower() } | Sort-Object Name

# Render the whole corpus in PARALLEL (PS7 ForEach -Parallel): one st2k spawn per
# file fanned out across cores, instead of 200+ sequential spawns. The ImageMagick
# tier stays memory-safe — st2k bounds concurrent magick children with a named
# (cross-process) semaphore, so even 32 st2k at once share a 4-magick cap. Each file
# writes a UNIQUE "<basename>_<ext>.png" so concurrent same-extension samples never
# race on one output path; we tally the {Ext, Ok} results afterward.
$results = $files | ForEach-Object -ThrottleLimit ([Environment]::ProcessorCount) -Parallel {
    $f = $_
    $ext = $f.Extension.TrimStart('.').ToLower()
    $out = Join-Path $using:render ("{0}_{1}.png" -f $f.BaseName, $ext)
    & $using:st2k thumbnail $f.FullName $out --size $using:Size 2>$null | Out-Null
    [pscustomobject]@{ Ext = $ext; In = $f.FullName; Out = $out; Ok = ((Test-Path $out) -and (Get-Item $out).Length -gt 0) }
}

# Sequential retry of first-pass failures. A metafile (EMF/WMF) render shells out
# to magick under a deliberately-TIGHT 3 s wall-clock timeout (decode.rs
# METAFILE_TIMEOUT — a slow vector metafile would otherwise grind ~5 s to a blank
# frame). On a fully-saturated machine the parallel fan-out (ThrottleLimit =
# ProcessorCount st2k spawns) can starve that render of CPU so a metafile that
# renders in ~300 ms unloaded blows past 3 s and spuriously "fails". Re-rendering
# the failures ONE AT A TIME (no CPU contention) clears such load flakes; a
# genuinely-unrenderable file (doc/flv/…) just fails again in a few ms. This makes
# the gate deterministic without touching the production timeout.
$retry = @($results | Where-Object { -not $_.Ok })
if ($retry.Count) {
    Write-Host ("[regression] first pass: {0} failure(s); retrying sequentially to rule out parallel-load flakes..." -f $retry.Count) -ForegroundColor DarkGray
    foreach ($r in $retry) {
        & $st2k thumbnail $r.In $r.Out --size $Size 2>$null | Out-Null
        $r.Ok = (Test-Path $r.Out) -and (Get-Item $r.Out).Length -gt 0
    }
    $recovered = @($retry | Where-Object Ok | ForEach-Object Ext | Sort-Object -Unique)
    if ($recovered.Count) { Write-Host ("[regression] recovered on sequential retry (were parallel-load flakes): {0}" -f ($recovered -join ' ')) -ForegroundColor DarkGray }
}
# An extension is "passing" if ANY sample with that extension rendered. Dedup so
# the baseline is a clean set of extensions.
$pass = @($results | Where-Object Ok | ForEach-Object Ext)
$fail = @($results | Where-Object { -not $_.Ok } | ForEach-Object Ext)
$passSet = $pass | Sort-Object -Unique
$failSet = $fail | Where-Object { $passSet -notcontains $_ } | Sort-Object -Unique

Write-Host ("[regression] PASS {0}/{1}" -f $pass.Count, $files.Count) -ForegroundColor Green
if ($failSet.Count) { Write-Host ("[regression] no-thumbnail ({0}): {1}" -f $failSet.Count, ($failSet -join ' ')) -ForegroundColor Yellow }

# Formats the corpus has no real sample for (see header): visible every run so
# the PASS number is never read as full-format coverage.
$noSampleFile = "$Corpus\_no-real-sample.txt"
if (Test-Path $noSampleFile) {
    $untested = @(Get-Content $noSampleFile | ForEach-Object { $_.Trim() } | Where-Object { $_ })
    if ($untested.Count) { Write-Host ("[regression] UNTESTED — no real sample ({0}): {1}" -f $untested.Count, ($untested -join ' ')) -ForegroundColor DarkYellow }
}

# Labelled contact sheet of everything that rendered.
if ($magick -and $pass.Count) {
    $contact = "$Corpus\contact.png"
    & $magick montage "$render\*.png" -label '%t' -tile 13x -geometry 92x92+3+3 -background '#202020' -fill '#dddddd' -pointsize 11 $contact 2>$null
    if (Test-Path $contact) { Write-Host "[regression] contact sheet: $contact" -ForegroundColor Cyan }
}

# --- Baseline persistence + diff (the part that can fail a pipeline) ---------

function Write-Baseline {
    param([string[]]$Set)
    # One extension per line, sorted, LF — stable diffs in git.
    Set-Content -Path $baselineFile -Value (($Set | Sort-Object -Unique) -join "`n") -NoNewline -Encoding ascii
}

# Is this on-disk sample self-evidently NOT a real, complete sample — i.e. the sign
# of an incomplete/failed corpus build rather than a code regression? Catches an
# empty/truncated stub (a file mid-(re)write while build-corpus.ps1 runs) and a
# failed network download left as an HTML error page or a Git-LFS pointer. Kept
# deliberately narrow so it never mislabels a REAL format: valid SVG/XML starts
# with `<?xml`/`<svg` (not matched), so a legitimately-failing SVG still counts as
# a regression. Only ever consulted for a sample that already failed to render.
function Test-CorpusSampleJunk {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) { return $true }
    if ((Get-Item -LiteralPath $Path).Length -lt 64) { return $true } # empty / truncated
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    $n = [Math]::Min($bytes.Length, 256)
    $head = [System.Text.Encoding]::ASCII.GetString($bytes, 0, $n)
    if ($head -match '(?im)^\s*<!doctype\s+html') { return $true }    # HTML error page
    if ($head -match '(?im)^\s*<html[\s>]')       { return $true }
    if ($head -match '404:?\s*Not Found' -or $head -match 'Access Denied' -or $head -match '<Error>') { return $true }
    if ($head -like 'version https://git-lfs*')   { return $true }    # un-pulled LFS pointer
    return $false
}

if ($UpdateBaseline) {
    Write-Baseline $passSet
    Write-Host ("[regression] baseline UPDATED ({0} extensions) -> {1}" -f $passSet.Count, $baselineFile) -ForegroundColor Cyan
    exit 0
}

if (-not (Test-Path $baselineFile)) {
    Write-Baseline $passSet
    Write-Host ("[regression] no baseline found — GENERATED initial baseline ({0} extensions) -> {1}" -f $passSet.Count, $baselineFile) -ForegroundColor Cyan
    Write-Host "[regression] (first run: nothing to diff against; commit the baseline.)" -ForegroundColor DarkGray
    exit 0
}

$baseline = Get-Content $baselineFile | ForEach-Object { $_.Trim().ToLower() } | Where-Object { $_ } | Sort-Object -Unique
$present  = $files | ForEach-Object { $_.Extension.TrimStart('.').ToLower() } | Sort-Object -Unique

# A true regression: an extension the baseline says should render, whose sample
# is STILL in the corpus, but which no longer produced a thumbnail.
$regressed = $baseline | Where-Object { ($present -contains $_) -and ($passSet -notcontains $_) }

# Baseline extension whose sample vanished from the corpus — a corpus change,
# not a render regression. Warn (it may be an intentional removal needing a
# baseline edit) but don't fail the gate on it.
$missingSamples = $baseline | Where-Object { $present -notcontains $_ }

# Newly-passing extensions not yet in the baseline — informational; intentional
# additions are accepted via -UpdateBaseline, not auto-merged.
$newPasses = $passSet | Where-Object { $baseline -notcontains $_ }

if ($newPasses.Count) {
    Write-Host ("[regression] NEW pass (not in baseline; run -UpdateBaseline to accept): {0}" -f ($newPasses -join ' ')) -ForegroundColor DarkCyan
}
if ($missingSamples.Count) {
    Write-Host ("[regression] baseline extensions with no corpus sample (corpus change — edit baseline if intentional): {0}" -f ($missingSamples -join ' ')) -ForegroundColor Yellow
}

if ($regressed.Count) {
    # A first-pass "regression" can be a FALSE ALARM from a corpus caught mid-flight:
    # the parallel render can race build-corpus.ps1 (re)writing a sample, or a failed
    # network download can leave an HTML error page / truncated stub. Vet each suspect
    # before failing the gate:
    #   1. skip samples that are self-evidently junk (Test-CorpusSampleJunk) — an
    #      incomplete/failed corpus, NOT a code regression;
    #   2. re-render each remaining (valid-looking) suspect SEQUENTIALLY once more —
    #      the corpus has since settled, so a file that was mid-write now renders.
    # Only a valid sample that STILL fails on the calm retry is a true regression.
    $reallyRegressed = @()
    $corpusIncomplete = @()
    foreach ($ext in $regressed) {
        $samples = @($files | Where-Object { $_.Extension.TrimStart('.').ToLower() -eq $ext })
        $rendered = $false
        $anyValidSample = $false
        foreach ($s in $samples) {
            if (Test-CorpusSampleJunk $s.FullName) { continue }
            $anyValidSample = $true
            $out = Join-Path $render ("retry_{0}_{1}.png" -f $s.BaseName, $ext)
            & $st2k thumbnail $s.FullName $out --size $Size 2>$null | Out-Null
            if ((Test-Path $out) -and (Get-Item $out).Length -gt 0) { $rendered = $true; break }
        }
        if ($rendered) { continue }                              # settled → renders now (false alarm)
        elseif (-not $anyValidSample) { $corpusIncomplete += $ext } # every sample is junk (corpus problem)
        else { $reallyRegressed += $ext }                        # valid sample, still fails (real)
    }

    if ($corpusIncomplete.Count) {
        Write-Host ("[regression] corpus INCOMPLETE ({0}): {1}" -f $corpusIncomplete.Count, ($corpusIncomplete -join ' ')) -ForegroundColor Yellow
        Write-Host "[regression] (sample is empty / an HTML error page / a truncated stub — re-run build-corpus.ps1; NOT a code regression, gate not failed.)" -ForegroundColor Yellow
    }
    $recovered = @($regressed | Where-Object { ($reallyRegressed -notcontains $_) -and ($corpusIncomplete -notcontains $_) })
    if ($recovered.Count) {
        Write-Host ("[regression] transient miss recovered on retry ({0}): {1}" -f $recovered.Count, ($recovered -join ' ')) -ForegroundColor DarkGray
        Write-Host "[regression] (rendered on a calm sequential retry — the parallel render raced a mid-flight corpus write.)" -ForegroundColor DarkGray
    }

    if ($reallyRegressed.Count) {
        Write-Host ("[regression] FAIL — {0} previously-passing extension(s) NEWLY broke: {1}" -f $reallyRegressed.Count, ($reallyRegressed -join ' ')) -ForegroundColor Red
        Write-Host "[regression] (a VALID sample is still present but no longer thumbnails, confirmed on retry — this is a real regression.)" -ForegroundColor Red
        exit 1
    }
}

# CONTENT guard (beyond render-only): .dcm must decode with the RIGHT colours, not
# just produce a non-empty thumbnail — the render sweep above can't see a hue or
# contrast regression. Run isolated (child pwsh) so its `exit` can't short-circuit
# us; it self-skips (exit 0) when ImageMagick is absent. See check-dicom.ps1.
& pwsh -NoProfile -File "$PSScriptRoot\check-dicom.ps1" | Out-Host
if ($LASTEXITCODE) {
    Write-Host "[regression] FAIL — DICOM content check failed (colour/contrast regressed)." -ForegroundColor Red
    exit 1
}

Write-Host ("[regression] OK — all {0} baseline extensions still render." -f $baseline.Count) -ForegroundColor Green
exit 0
