<#
  check-render-parity.ps1 — render the whole corpus with the PREVIOUS RELEASE and with the
  build in hand, and fail if any sample's PICTURE changed without someone meaning it.

  WHY. Every other gate in this repo asks whether something HAPPENED, never whether it was
  RIGHT: regression.ps1 asks for a non-empty PNG, the fuzzers ask that nothing panicked,
  clippy asks about syntax. None of them can see a decoder that renders successfully and
  renders the wrong thing. That is not hypothetical — it is how 2.0.0's XCF layer budget
  shipped a thumbnail of the wrong layer for every large GIMP file, with the whole gate stack
  green. Comparing pixels against the last release closes it for EVERY format at once, and
  needs no per-format work.

  The baseline is a real shipped artifact, not a stored image: dist\ keeps every released
  portable zip, so the previous release's st2k.exe is already on disk. That also means the
  comparison is against what users actually have.

      pwsh scripts\check-render-parity.ps1                  # vs the newest older release
      pwsh scripts\check-render-parity.ps1 -Baseline 2.1.0  # vs a specific one
      pwsh scripts\check-render-parity.ps1 -Accept          # report differences, exit 0

  EXIT CODES:
    0  every sample renders the same picture (or -Accept, or nothing to compare against).
    1  a sample LOST its thumbnail or its picture CHANGED. Read the list: an intentional
       decoder improvement shows up here too, and the right response to one of those is to
       re-run with -Accept, not to weaken the gate.
    2  cannot run (no python + Pillow, or no baseline release in dist\).

  NOT wired into preflight/every push on purpose: it renders the corpus TWICE, which is
  minutes, and the question it answers ("did this release change any picture") is a release
  question. It rides `verify.ps1 -Release` and the release ritual.
#>
param(
    [string]$Corpus = "$PSScriptRoot\..\..\test-corpus",
    # Version to compare against, e.g. '2.1.0'. Default: the newest release in dist\ that is
    # older than the version in Cargo.toml.
    [string]$Baseline,
    [string]$St2kPath,
    # Report differences without failing — for a release that MEANS to change a picture.
    [switch]$Accept,
    [int]$Size = 256
)
$ErrorActionPreference = 'Continue'
$root = Split-Path $PSScriptRoot -Parent

$py = (Get-Command python -EA SilentlyContinue).Source
if (-not $py) { Write-Host '[parity] python not found — SKIPPED' -ForegroundColor Yellow; exit 2 }
& $py -c "import PIL" 2>$null
if ($LASTEXITCODE -ne 0) {
    Write-Host '[parity] Pillow not installed (pip install pillow) — SKIPPED' -ForegroundColor Yellow
    exit 2
}
if (-not (Test-Path $Corpus)) { Write-Host "[parity] no corpus at $Corpus — SKIPPED" -ForegroundColor Yellow; exit 2 }

if (-not $St2kPath) { $St2kPath = Join-Path (& "$PSScriptRoot\_targetdir.ps1") 'release\st2k.exe' }
if (-not (Test-Path -LiteralPath $St2kPath)) { throw "st2k.exe not found: $St2kPath" }
$new = (Resolve-Path -LiteralPath $St2kPath).Path

# Pick the baseline release. Versions are sorted as VERSIONS, not as strings, or 2.1.10 would
# rank below 2.1.2 and the gate would quietly compare against the wrong build.
$dist = Join-Path $root 'dist'
$zips = @(Get-ChildItem $dist -Filter 'SageThumbs2K-Portable-*.zip' -EA SilentlyContinue |
    Where-Object { $_.Name -notlike '*arm64*' } |
    ForEach-Object {
        if ($_.Name -match 'Portable-(\d+\.\d+\.\d+)\.zip$') {
            [pscustomobject]@{ Ver = [version]$Matches[1]; Path = $_.FullName }
        }
    } | Sort-Object Ver -Descending)

if ($Baseline) {
    $pick = $zips | Where-Object { $_.Ver -eq [version]$Baseline } | Select-Object -First 1
    if (-not $pick) { Write-Host "[parity] no portable zip for $Baseline in dist\" -ForegroundColor Red; exit 2 }
} else {
    $current = if ((Get-Content (Join-Path $root 'Cargo.toml') -Raw) -match '(?m)^version\s*=\s*"([\d.]+)"') { [version]$Matches[1] } else { $null }
    $pick = $zips | Where-Object { -not $current -or $_.Ver -lt $current } | Select-Object -First 1
    if (-not $pick) { $pick = $zips | Select-Object -First 1 }
}
if (-not $pick) { Write-Host '[parity] no previous release in dist\ to compare against — SKIPPED' -ForegroundColor Yellow; exit 2 }

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("st2k-parity-" + [System.Diagnostics.Process]::GetCurrentProcess().Id)
$oldDir = Join-Path $work 'baseline'
New-Item -ItemType Directory -Force $oldDir | Out-Null
Expand-Archive -LiteralPath $pick.Path -DestinationPath $oldDir -Force
$old = Join-Path $oldDir 'st2k.exe'
if (-not (Test-Path $old)) { Write-Host "[parity] no st2k.exe inside $($pick.Path)" -ForegroundColor Red; exit 2 }

Write-Host ("[parity] {0}  vs  {1}" -f $pick.Ver, $new) -ForegroundColor Cyan
& $py (Join-Path $PSScriptRoot 'compare-renders.py') `
    --corpus $Corpus --out (Join-Path $work 'render') --old $old --new $new --size $Size
$code = $LASTEXITCODE

Remove-Item $work -Recurse -Force -EA SilentlyContinue

if ($code -ne 0) {
    if ($Accept) {
        Write-Host '[parity] differences ACCEPTED (-Accept): make sure every one above is intended.' -ForegroundColor Yellow
        exit 0
    }
    Write-Host "[parity] FAIL — the picture changed for at least one sample since $($pick.Ver)." -ForegroundColor Red
    Write-Host '[parity] If every difference above is an intended improvement, re-run with -Accept.' -ForegroundColor Red
    exit 1
}
Write-Host "[parity] every sample renders the same picture as $($pick.Ver)" -ForegroundColor Green
exit 0
