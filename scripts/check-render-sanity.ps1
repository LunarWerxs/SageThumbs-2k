<#
  check-render-sanity.ps1 — look at the rendered corpus and fail on a BROKEN PICTURE.

  The fourth content gate, and the one that needs no per-sample expectation. `regression.ps1`'s
  sweep asks whether a non-empty PNG appeared; `_expected-colors.txt` asks whether ~18 curated
  samples are right; `check-render-parity.ps1` asks whether anything CHANGED since the last
  release. None of them can see a thumbnail that has been wrong since the day it was written,
  and that gap shipped twice — the InDesign preview that drew a strip of page over flat grey,
  and the Kodak `.dcr` that thumbnailed pure black while ImageMagick read a real photo out of
  the same file. Both were in this corpus, rendering wrong, with every gate green.

  So this one asks the question directly: does the render carry a signature that only a broken
  decode produces (see scripts\render-sanity.py for the two, and why each is false-alarm-free).

      pwsh scripts\check-render-sanity.ps1                  # against test-corpus\_render
      pwsh scripts\check-render-sanity.ps1 -ReportOnly      # list findings, exit 0
      pwsh scripts\check-render-sanity.ps1 -Render          # render the corpus first
      pwsh scripts\check-render-sanity.ps1 -ProveItFails    # prove it still has teeth

  EXIT CODES
    0  clean (or -ReportOnly)
    1  a render carries a broken-picture signature
    2  cannot run (no python + Pillow, or nothing rendered yet)

  It reads the PNGs `regression.ps1` already wrote, so on its own it costs only the comparison.
  The ImageMagick half is what makes the second check possible at all: an independent decoder
  is the only honest answer to "should there have been detail here", and the bundled magick is
  already on this machine.
#>
param(
    [string]$Corpus = "$PSScriptRoot\..\..\test-corpus",
    [string]$Rendered,
    [string]$St2kPath,
    # Render the corpus before checking, instead of reading what is already there.
    [switch]$Render,
    # Print findings without failing — for triage.
    [switch]$ReportOnly,
    # Self-test: synthesise a broken render, prove this script returns 1 for it and 0 for a
    # healthy one, and prove the allow-list suppresses it. Same convention as
    # check-prebuild-coverage.ps1's -ProveItFails, and for the same reason: a gate nobody has
    # ever seen FAIL is indistinguishable from a gate that cannot fail.
    [switch]$ProveItFails
)

$ErrorActionPreference = 'Stop'

if ($ProveItFails) {
    Add-Type -AssemblyName System.Drawing
    $root = Join-Path ([System.IO.Path]::GetTempPath()) ("st2k-sanity-selftest-" + $PID)
    $c = Join-Path $root 'corpus'; $r = Join-Path $root 'render'
    New-Item -ItemType Directory -Force $c, $r | Out-Null
    try {
        # A tile that is real content on top and the decoder's no-data grey below: exactly
        # what a JPEG that ran out of data renders as, which is the InDesign bug's signature.
        foreach ($case in @(@{ n = 'broken'; cut = 0.5 }, @{ n = 'healthy'; cut = 1.0 })) {
            $bmp = New-Object System.Drawing.Bitmap 128, 128
            $g = [System.Drawing.Graphics]::FromImage($bmp)
            $g.Clear([System.Drawing.Color]::FromArgb(128, 128, 128))
            for ($y = 0; $y -lt [int](128 * $case.cut); $y++) {
                for ($x = 0; $x -lt 128; $x++) {
                    $bmp.SetPixel($x, $y, [System.Drawing.Color]::FromArgb(($x * 2) % 256, ($y * 2) % 256, 90))
                }
            }
            $g.Dispose()
            $bmp.Save((Join-Path $r "$($case.n)_indd.png"), [System.Drawing.Imaging.ImageFormat]::Png)
            $bmp.Dispose()
            Set-Content (Join-Path $c "$($case.n).indd") -Value 'stand-in'
        }
        $self = $MyInvocation.MyCommand.Path
        $fails = 0
        & pwsh -NoProfile -File $self -Corpus $c -Rendered $r *> $null
        if ($LASTEXITCODE -ne 1) { Write-Host '[self-test] FAIL: a truncated render did not fail the gate' -ForegroundColor Red; $fails++ }
        else { Write-Host '[self-test] OK   truncated render fails the gate' -ForegroundColor Green }

        Set-Content (Join-Path $c '_render-sanity-allow.txt') -Value 'broken.indd  # self-test'
        & pwsh -NoProfile -File $self -Corpus $c -Rendered $r *> $null
        if ($LASTEXITCODE -ne 0) { Write-Host '[self-test] FAIL: the allow-list did not suppress it' -ForegroundColor Red; $fails++ }
        else { Write-Host '[self-test] OK   allow-list suppresses a known finding' -ForegroundColor Green }

        Remove-Item -LiteralPath (Join-Path $c 'broken.indd') -Force
        Remove-Item -LiteralPath (Join-Path $r 'broken_indd.png') -Force
        Remove-Item -LiteralPath (Join-Path $c '_render-sanity-allow.txt') -Force
        & pwsh -NoProfile -File $self -Corpus $c -Rendered $r *> $null
        if ($LASTEXITCODE -ne 0) { Write-Host '[self-test] FAIL: a healthy render was flagged' -ForegroundColor Red; $fails++ }
        else { Write-Host '[self-test] OK   healthy render passes' -ForegroundColor Green }

        if ($fails) { Write-Host "[self-test] $fails case(s) wrong" -ForegroundColor Red; exit 1 }
        Write-Host '[self-test] the gate has teeth' -ForegroundColor Green
        exit 0
    } finally {
        Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
    }
}
if (-not $Rendered) { $Rendered = Join-Path $Corpus '_render' }
$allow = Join-Path $Corpus '_render-sanity-allow.txt'

if (-not (Test-Path $Corpus)) {
    Write-Host "[sanity] no corpus at $Corpus — SKIPPED" -ForegroundColor Yellow
    exit 2
}

$py = (Get-Command python -ErrorAction SilentlyContinue).Source
if (-not $py) {
    Write-Host '[sanity] needs python + Pillow — SKIPPED' -ForegroundColor Yellow
    exit 2
}

if ($Render) {
    if (-not $St2kPath) {
        # Resolved through _targetdir.ps1, never a hardcoded dev-machine path.
        $St2kPath = Join-Path (& (Join-Path $PSScriptRoot '_targetdir.ps1')) 'release\st2k.exe'
    }
    if (-not (Test-Path $St2kPath)) {
        Write-Host "[sanity] no st2k at $St2kPath — SKIPPED" -ForegroundColor Yellow
        exit 2
    }
    New-Item -ItemType Directory -Force $Rendered | Out-Null
    Get-ChildItem $Corpus -File |
        Where-Object { $_.Name -notlike '_*' -and $_.Name -ne 'contact.png' -and $_.Extension -ne '.lnk' } |
        ForEach-Object {
            $out = Join-Path $Rendered ($_.BaseName + '_' + $_.Extension.TrimStart('.') + '.png')
            & $St2kPath thumbnail $_.FullName $out 256 *> $null
        }
}

# The bundled ImageMagick is the INDEPENDENT decoder — deliberately not our code. Prefer the
# installed copy, then a build-tree stage, then whatever is on PATH; absent is a skip of that
# half only, never a failure (the whole point of this project is formats magick cannot read).
$magick = @(
    'C:\Program Files\SageThumbs2K\magick.exe',
    (Join-Path $PSScriptRoot 'packaging\stage\x64\magick.exe')
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $magick) { $magick = (Get-Command magick -ErrorAction SilentlyContinue).Source }
if (-not $magick) {
    Write-Host '[sanity] no ImageMagick — the cross-decoder half is skipped' -ForegroundColor Yellow
}

$argv = @("$PSScriptRoot\render-sanity.py", '--corpus', $Corpus, '--rendered', $Rendered)
if ($magick) { $argv += @('--magick', $magick) }
if (Test-Path $allow) { $argv += @('--allow', $allow) }
if ($ReportOnly) { $argv += '--report-only' }

$out = & $py @argv
$code = $LASTEXITCODE
$out | Write-Host
$found = @($out | Where-Object { $_ -match 'suspect render' }).Count -gt 0
if ($code -eq 1) {
    Write-Host '[sanity] FAIL — see the findings above.' -ForegroundColor Red
} elseif ($found) {
    # -ReportOnly exits 0 deliberately, but saying PASS over a list of findings is the
    # kind of lie this whole script exists to stop.
    Write-Host '[sanity] findings above, not failed (-ReportOnly).' -ForegroundColor Yellow
} elseif ($code -eq 0) {
    Write-Host '[sanity] PASS' -ForegroundColor Green
}
exit $code
