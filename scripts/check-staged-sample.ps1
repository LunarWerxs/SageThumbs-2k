<#
  check-staged-sample.ps1 — render NAMED files through the exact payload we ship, in seconds.

      pwsh scripts\check-staged-sample.ps1 real.svg real.pes
      pwsh scripts\check-staged-sample.ps1 *.svg
      pwsh scripts\check-staged-sample.ps1 "D:\somewhere\weird-file.m2p"

  WHY (2026-09-17). test-staged-regression.ps1 is the only gate that renders with the SHIPPED
  ImageMagick bundle instead of whatever ImageMagick this box has in Program Files, and it is
  the gate that caught both bugs the 3.1.0 run shipped into: an SVG our own tier stopped
  recognising, and .pes, which needs an RSVG stack the bundle deliberately omits. But it sweeps
  the whole corpus with all four baseline gates, so it is a ~3-minute answer that lives ~25
  minutes deep inside release.ps1. Finding a broken sample therefore cost a whole release run.

  Almost every time, the question is much smaller: "this file is new / I just touched its
  decoder — does the PRODUCT render it?" That question is one process spawn. This script
  flattens the staged payload the way the installer does (stage keeps magick in a subdirectory;
  the installed app has it beside the exe), renders only the files you name, and says so.
  No baselines, no corpus sweep, no comparison — just the truth about those files.

  ⚠ IT USES THE LAST STAGED BUILD. Run it after scripts\build-release.ps1 (or after any
  release run) so the stage matches your source. It says which stage it used and how old it is.

  RULE OF THUMB, earned the hard way: every NEW pinned corpus sample gets run through here
  before it is committed, and so does any file whose decode tier just changed. Ten seconds
  there replaces a 40-minute release that ends in a failed gate.
#>
# PositionalBinding=$false so every bare argument lands in -Sample: without it PowerShell
# quietly binds the SECOND file name to -StagePath and the third to -Size.
[CmdletBinding(PositionalBinding = $false)]
param(
    # File names, globs, or full paths. Bare names/globs are looked up in the corpus.
    [Parameter(Position = 0, ValueFromRemainingArguments = $true)]
    [string[]]$Sample,
    [string]$StagePath,
    [string]$Corpus = "$PSScriptRoot\..\..\test-corpus",
    [int]$Size = 96,
    # Leave the flattened runtime in place and print its path (handy for poking at it by hand).
    [switch]$KeepRuntime
)
$ErrorActionPreference = 'Stop'

if (-not $Sample -or -not $Sample.Count) {
    throw "name at least one file, e.g.: pwsh scripts\check-staged-sample.ps1 real.svg"
}

$root = Split-Path $PSScriptRoot -Parent
if (-not $StagePath) { $StagePath = Join-Path $root 'scripts\packaging\stage\x64' }
$stage = (Resolve-Path -LiteralPath $StagePath -ErrorAction Stop).Path
$stagedCli = Join-Path $stage 'st2k.exe'
$stagedMagick = Join-Path $stage 'magick'
if (-not (Test-Path -LiteralPath $stagedCli -PathType Leaf)) {
    throw "staged CLI not found: $stagedCli (build it first: pwsh scripts\build-release.ps1)"
}
if (-not (Test-Path -LiteralPath (Join-Path $stagedMagick 'magick.exe') -PathType Leaf)) {
    throw "staged ImageMagick bundle not found: $stagedMagick (build it first: pwsh scripts\build-release.ps1)"
}
$stagedAge = [int]((Get-Date) - (Get-Item -LiteralPath $stagedCli).LastWriteTime).TotalMinutes
Write-Host ("[staged-sample] payload: {0} (staged {1} minute(s) ago)" -f $stage, $stagedAge) -ForegroundColor DarkGray

# Resolve each argument: an existing path wins, otherwise treat it as a name/glob in the corpus.
$corpusPath = if (Test-Path -LiteralPath $Corpus) { (Resolve-Path -LiteralPath $Corpus).Path } else { $null }
$targets = @()
foreach ($s in $Sample) {
    if (Test-Path -LiteralPath $s -PathType Leaf) {
        $targets += (Resolve-Path -LiteralPath $s).Path
        continue
    }
    if (-not $corpusPath) { throw "no corpus at $Corpus and '$s' is not a file path" }
    $hits = @(Get-ChildItem -LiteralPath $corpusPath -File -Filter $s -ErrorAction SilentlyContinue)
    if (-not $hits.Count) { throw "no corpus file matches '$s' (looked in $corpusPath)" }
    $targets += @($hits | ForEach-Object { $_.FullName })
}
$targets = @($targets | Sort-Object -Unique)

# Reproduce the INSTALLED layout: st2k.exe with the Magick runtime flattened beside it. The
# stage keeps magick in a subdirectory for Inno Setup, and a subdirectory is invisible to
# magick_exe(), which would then fall back to a Program Files ImageMagick — the exact masking
# this script exists to defeat.
$runtime = Join-Path ([IO.Path]::GetTempPath()) ("st2k-staged-sample-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $runtime | Out-Null
$failed = @()
try {
    Copy-Item -LiteralPath $stagedCli -Destination (Join-Path $runtime 'st2k.exe')
    Copy-Item -Path (Join-Path $stagedMagick '*') -Destination $runtime -Recurse
    $st2k = Join-Path $runtime 'st2k.exe'
    $out = Join-Path $runtime '_out'
    New-Item -ItemType Directory -Path $out | Out-Null

    foreach ($t in $targets) {
        $name = Split-Path $t -Leaf
        $png = Join-Path $out ("{0}.png" -f $name)
        & $st2k thumbnail $t $png --size $Size 2>$null | Out-Null
        $bytes = if (Test-Path -LiteralPath $png) { (Get-Item -LiteralPath $png).Length } else { 0 }
        if ($bytes -gt 0) {
            Write-Host ("  PASS  {0}  ({1} bytes of PNG)" -f $name, $bytes) -ForegroundColor Green
        } else {
            Write-Host ("  FAIL  {0}  - no thumbnail from the shipped payload" -f $name) -ForegroundColor Red
            $failed += $name
        }
    }
} finally {
    if ($KeepRuntime) {
        Write-Host ("[staged-sample] runtime kept: {0}" -f $runtime) -ForegroundColor DarkGray
    } elseif (Test-Path -LiteralPath $runtime) {
        Remove-Item -LiteralPath $runtime -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if ($failed.Count) {
    Write-Host ''
    Write-Host ("[staged-sample] {0} of {1} file(s) produce NO thumbnail in a real install." -f $failed.Count, $targets.Count) -ForegroundColor Red
    Write-Host '  This box renders them anyway if it has a full ImageMagick in Program Files - that' -ForegroundColor Red
    Write-Host '  fallback is exactly what hides the fault. Either fix the tier, or stop advertising' -ForegroundColor Red
    Write-Host '  the format (formats::REMOVED_EXTENSIONS) rather than shipping a stock icon.' -ForegroundColor Red
    exit 1
}

Write-Host ("[staged-sample] OK - all {0} file(s) render through the shipped payload." -f $targets.Count) -ForegroundColor Green
exit 0
