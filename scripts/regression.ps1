<#
  regression.ps1 — render every test-corpus sample through st2k and report which
  formats thumbnail successfully. Doubles as a CI/pipeline gate.

      pwsh scripts\regression.ps1                 # check against the baseline
      pwsh scripts\regression.ps1 -UpdateBaseline # accept the current pass set as the new baseline

  EXIT CODES (so this can fail a pipeline):
    0  no regression  — every extension in the baseline still renders.
    1  REGRESSION     — at least one previously-passing extension NEWLY fails
                        (its sample is still in the corpus but no longer thumbnails).

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
    [pscustomobject]@{ Ext = $ext; Ok = ((Test-Path $out) -and (Get-Item $out).Length -gt 0) }
}
# An extension is "passing" if ANY sample with that extension rendered. Dedup so
# the baseline is a clean set of extensions.
$pass = @($results | Where-Object Ok | ForEach-Object Ext)
$fail = @($results | Where-Object { -not $_.Ok } | ForEach-Object Ext)
$passSet = $pass | Sort-Object -Unique
$failSet = $fail | Where-Object { $passSet -notcontains $_ } | Sort-Object -Unique

Write-Host ("[regression] PASS {0}/{1}" -f $pass.Count, $files.Count) -ForegroundColor Green
if ($failSet.Count) { Write-Host ("[regression] no-thumbnail ({0}): {1}" -f $failSet.Count, ($failSet -join ' ')) -ForegroundColor Yellow }

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
    Write-Host ("[regression] FAIL — {0} previously-passing extension(s) NEWLY broke: {1}" -f $regressed.Count, ($regressed -join ' ')) -ForegroundColor Red
    Write-Host "[regression] (a sample is still present but no longer thumbnails — this is a real regression.)" -ForegroundColor Red
    exit 1
}

Write-Host ("[regression] OK — all {0} baseline extensions still render." -f $baseline.Count) -ForegroundColor Green
exit 0
