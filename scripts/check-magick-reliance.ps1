<#
  check-magick-reliance.ps1 — render the whole corpus with ImageMagick switched OFF and
  compare the survivors against a committed baseline.

      pwsh scripts\check-magick-reliance.ps1                 # gate (exit 1 on regression)
      pwsh scripts\check-magick-reliance.ps1 -UpdateBaseline # accept the current set

  WHY THIS EXISTS (2026-09-17, the day the 3.1.0 release run found it the expensive way).
  decode\magick.rs::magick_exe() prefers an ImageMagick bundled beside the running binary
  and FALLS BACK to any C:\Program Files\ImageMagick*. Every developer box has that full
  install; the shipped product does not — our bundle deliberately omits the whole
  rsvg/cairo/pango stack (docs\MAGICK.md "Reviewed omissions": resvg replaces it). So a
  file that is supposed to be served by one of OUR decoders can quietly start falling
  through to ImageMagick instead, look perfect on every dev machine and in every
  `cargo test`, and show the stock icon on every install and every portable zip.

  That is exactly how an SVG whose root element sits behind a licence comment shipped
  broken: looks_like_svg() scanned only the first 1 KB, the corpus's Apache Batik sample
  puts `<svg` at byte 3460, resvg therefore never saw it, and the dev box's ImageMagick
  covered the hole. The only gate that could see it was test-staged-regression.ps1, which
  lives ~25 minutes deep inside release.ps1 — so the fault cost an entire release run to
  find, twice in one evening.

  This asks the same question in about a minute, with no bundle, no staging and no
  installer: with ST2K_NO_MAGICK=1, does every sample that used to stand on its own still
  stand on its own? A file that leaves that set has silently changed WHICH TIER SERVES IT,
  and in the shipped bundle that is a coin flip at best.

  IT DOES NOT REPLACE test-staged-regression.ps1, which also proves the bundle's own
  coders and delegates actually work (that is what caught .pes). This is the fast half.

  BASELINE: scripts\magick-free-baseline.txt — sorted sample file names that thumbnail
  with no ImageMagick at all. A file LEAVING the set is a regression (exit 1). A file
  JOINING it is good news and only asks for -UpdateBaseline (exit 0).
#>
[CmdletBinding()]
param(
    [string]$Corpus = "$PSScriptRoot\..\..\test-corpus",
    [int]$Size = 96,
    # Override the executable under test; defaults to the release st2k.exe, then debug.
    [string]$St2kPath,
    [switch]$UpdateBaseline
)
$ErrorActionPreference = 'Stop'

$baselineFile = "$PSScriptRoot\magick-free-baseline.txt"

if (-not $St2kPath) {
    $targetDir = & "$PSScriptRoot\_targetdir.ps1"
    $St2kPath = Join-Path $targetDir 'release\st2k.exe'
    if (-not (Test-Path -LiteralPath $St2kPath -PathType Leaf)) {
        $St2kPath = Join-Path $targetDir 'debug\st2k.exe'
    }
}
if (-not (Test-Path -LiteralPath $St2kPath -PathType Leaf)) {
    throw "st2k.exe not found: $St2kPath (build it first: cargo build --release -p sagethumbs2k)"
}
$st2k = (Resolve-Path -LiteralPath $St2kPath).Path
# Say which binary answered and how old it is: this gate is only as honest as the build under
# it, and "I ran it" on a half-hour-old st2k.exe is the one way it could mislead.
$st2kAge = [int]((Get-Date) - (Get-Item -LiteralPath $st2k).LastWriteTime).TotalMinutes
Write-Host ("[magick-free] binary: {0} (built {1} minute(s) ago)" -f $st2k, $st2kAge) -ForegroundColor DarkGray

if (-not (Test-Path -LiteralPath $Corpus)) {
    # INCONCLUSIVE, which is NOT a pass (regression.ps1's finding F37, 2026-09-05: a checker
    # that could not run must never read as green). Exit 2 so a caller can tell a skip from a
    # clean run; nothing in this repo treats 2 as success.
    Write-Host ("[magick-free] INCONCLUSIVE - no corpus at {0} (build it: pwsh scripts\build-corpus.ps1)" -f $Corpus) -ForegroundColor Yellow
    exit 2
}
$corpusPath = (Resolve-Path -LiteralPath $Corpus).Path

$skipExt = '.md', '.txt'
$files = @(
    Get-ChildItem -LiteralPath $corpusPath -File |
        Where-Object { $_.Name -notlike '_*' -and $skipExt -notcontains $_.Extension.ToLower() } |
        Sort-Object Name
)
if (-not $files.Count) { throw "corpus is empty: $corpusPath" }
$present = @($files | ForEach-Object Name)

$baseline = @()
if (Test-Path -LiteralPath $baselineFile) {
    $baseline = @(Get-Content -LiteralPath $baselineFile | ForEach-Object { $_.Trim() } | Where-Object { $_ })
}

# Own render directory, never the corpus's _render: this gate is meant to be runnable while
# regression.ps1 is using that one.
$render = Join-Path ([IO.Path]::GetTempPath()) ("st2k-magick-free-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $render | Out-Null
try {
    # ST2K_NO_MAGICK=1 is read by magick_exe() and short-circuits BEFORE any lookup, so this
    # holds for an st2k with a magick bundled beside it just as well as for a bare one.
    $results = $files | ForEach-Object -ThrottleLimit ([Environment]::ProcessorCount) -Parallel {
        $f = $_
        $env:ST2K_NO_MAGICK = '1'
        $out = Join-Path $using:render ("{0}_{1}.png" -f $f.BaseName, $f.Extension.TrimStart('.').ToLower())
        & $using:st2k thumbnail $f.FullName $out --size $using:Size 2>$null | Out-Null
        [pscustomobject]@{
            Name = $f.Name
            In   = $f.FullName
            Out  = $out
            Ok   = ((Test-Path -LiteralPath $out) -and (Get-Item -LiteralPath $out).Length -gt 0)
        }
    }

    # Same false-alarm guard regression.ps1 uses: a metafile render shells out under a tight
    # 3 s wall clock and can starve under a ProcessorCount-wide fan-out. Only BASELINED misses
    # are retried — everything else is expected to fail here and retrying hundreds of them
    # would cost more than the gate.
    $retry = @($results | Where-Object { -not $_.Ok -and ($baseline -contains $_.Name) })
    if ($retry.Count) {
        Write-Host ("[magick-free] {0} baselined sample(s) missed under parallel load; retrying sequentially..." -f $retry.Count) -ForegroundColor DarkGray
        $env:ST2K_NO_MAGICK = '1'
        try {
            foreach ($r in $retry) {
                & $st2k thumbnail $r.In $r.Out --size $Size 2>$null | Out-Null
                $r.Ok = (Test-Path -LiteralPath $r.Out) -and (Get-Item -LiteralPath $r.Out).Length -gt 0
            }
        } finally {
            Remove-Item Env:\ST2K_NO_MAGICK -ErrorAction SilentlyContinue
        }
    }
} finally {
    if (Test-Path -LiteralPath $render) { Remove-Item -LiteralPath $render -Recurse -Force -ErrorAction SilentlyContinue }
}

$passSet = @($results | Where-Object Ok | ForEach-Object Name | Sort-Object)

if ($UpdateBaseline) {
    # LF, like every other baseline in scripts\ — Set-Content would write CRLF and make the
    # first commit on a fresh checkout a whole-file diff.
    [IO.File]::WriteAllText($baselineFile, (($passSet -join "`n") + "`n"), (New-Object Text.UTF8Encoding $false))
    Write-Host ("[magick-free] baseline updated: {0} of {1} samples thumbnail with no ImageMagick at all." -f $passSet.Count, $files.Count) -ForegroundColor Green
    exit 0
}
if (-not $baseline.Count) {
    throw "no baseline at $baselineFile - create one with -UpdateBaseline while the tree is known good"
}

# A sample that used to render without ImageMagick and no longer does. Only judged for files
# that are actually present, so a partial local corpus cannot manufacture a regression.
$lost    = @($baseline | Where-Object { ($present -contains $_) -and ($passSet -notcontains $_) })
$missing = @($baseline | Where-Object { $present -notcontains $_ })
$gained  = @($passSet  | Where-Object { $baseline -notcontains $_ })

if ($missing.Count) {
    Write-Host ("[magick-free] {0} baselined sample(s) are not in this corpus (run build-corpus.ps1, or -UpdateBaseline if they were retired): {1}" -f $missing.Count, ($missing -join ' ')) -ForegroundColor DarkGray
}
if ($gained.Count) {
    Write-Host ("[magick-free] {0} sample(s) newly stand on their own - good news, record it with -UpdateBaseline: {1}" -f $gained.Count, ($gained -join ' ')) -ForegroundColor Yellow
}

if ($lost.Count) {
    Write-Host ''
    Write-Host ("[magick-free] REGRESSION - {0} sample(s) now need ImageMagick to render:" -f $lost.Count) -ForegroundColor Red
    foreach ($l in $lost) { Write-Host ("    {0}" -f $l) -ForegroundColor Red }
    Write-Host ''
    Write-Host '  These used to be served by one of our own decoders and now fall through to the' -ForegroundColor Red
    Write-Host '  ImageMagick tier. They still look fine here because this machine has a full' -ForegroundColor Red
    Write-Host '  ImageMagick in Program Files; the shipped bundle omits the rsvg/cairo/pango stack' -ForegroundColor Red
    Write-Host '  on purpose, so in a real install this is a stock icon. Fix the decoder, or, if the' -ForegroundColor Red
    Write-Host '  handover is deliberate, prove it against the SHIPPED payload first:' -ForegroundColor Red
    Write-Host '      pwsh scripts\test-staged-regression.ps1' -ForegroundColor Red
    exit 1
}

Write-Host ("[magick-free] OK - all {0} baselined samples still render with no ImageMagick ({1} of {2} corpus files stand on their own)." -f $baseline.Count, $passSet.Count, $files.Count) -ForegroundColor Green
exit 0
