<#
.SYNOPSIS
  Prove every window actually follows the light/dark setting.

.DESCRIPTION
  QuickLook has an open bug where ONE content type — the SVG preview — stays dark
  even in light mode, because its background is painted by a path that never asked
  what the theme was (their issue #1797). That class of bug is invisible to a
  normal test suite and to anyone who only ever runs one theme.

  So this renders every window, and every Quick-preview CONTENT TYPE, twice: once
  with ST2K_THEME=dark and once with ST2K_THEME=light. For each pair it asserts

    1. the two images DIFFER — a window that ignores the theme produces identical
       bytes, which is exactly the failure mode; and
    2. each one is actually the brightness its name claims, by mean luminance —
       so a window can't "differ" by one stray pixel and pass.

  Headless: uses the app's own `--shot` modes, no desktop is driven, no window
  appears. Outputs land in a temp folder and are deleted unless -Keep.

.EXAMPLE
  pwsh scripts\check-theme-shots.ps1
  pwsh scripts\check-theme-shots.ps1 -Keep      # leave the PNGs to eyeball
#>
[CmdletBinding()]
param(
    # Resolved through _targetdir.ps1, never a hardcoded dev-machine path.
    [string]$ExePath = (Join-Path (& (Join-Path $PSScriptRoot '_targetdir.ps1')) 'release\SageThumbs2K.exe'),
    [switch]$Keep
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
if (-not (Test-Path -LiteralPath $ExePath)) {
    throw "SageThumbs2K.exe not found at '$ExePath'. Build it first (verify.ps1 -Release)."
}

$outDir = Join-Path ([IO.Path]::GetTempPath()) ("st2k-theme-" + [Guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

Add-Type -AssemblyName System.Drawing

# Mean luminance of a PNG, 0..255. Sampled every 4th pixel: a 60x60 grid was
# tried first and gave IDENTICAL means for four visibly different preview shots,
# because at that spacing it only ever landed on the window background. A check
# that cannot tell its own cases apart is worse than no check.
function Get-MeanLuma([string]$path) {
    $bmp = [System.Drawing.Bitmap]::FromFile($path)
    try {
        $sum = 0.0; $n = 0
        $stepX = 4
        $stepY = 4
        for ($y = 0; $y -lt $bmp.Height; $y += $stepY) {
            for ($x = 0; $x -lt $bmp.Width; $x += $stepX) {
                $c = $bmp.GetPixel($x, $y)
                $sum += 0.2126 * $c.R + 0.7152 * $c.G + 0.0722 * $c.B
                $n++
            }
        }
        if ($n -eq 0) { return 0 } else { return [Math]::Round($sum / $n, 1) }
    } finally { $bmp.Dispose() }
}

function Get-Sha([string]$path) { (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash }

function Invoke-Shot([string]$theme, [string]$outFile, [string[]]$shotArgs) {
    $env:ST2K_THEME = $theme
    try {
        # EVERY argument gets quoted, not just the output path. This repo lives under
        # "…\SageThumbs 2K\SageThumbs 2K", so an unquoted `--file` value splits on the
        # space and the app silently falls back to its synthetic test image — which
        # made four different cases render byte-identical output and pass. The
        # duplicate-detection below is what caught it.
        $quoted = @('--shot', "`"$outFile`"") + ($shotArgs | ForEach-Object { "`"$_`"" })
        $p = Start-Process -FilePath $ExePath -ArgumentList $quoted -NoNewWindow -Wait -PassThru
        if ($p.ExitCode -ne 0) { throw "shot failed (exit $($p.ExitCode)): $($shotArgs -join ' ')" }
        if (-not (Test-Path -LiteralPath $outFile)) { throw "shot produced no file: $outFile" }
    } finally { Remove-Item Env:ST2K_THEME -ErrorAction SilentlyContinue }
}

# Every surface that paints its own chrome. The Quick-preview entries are the
# point of the exercise: each content type takes a different paint path, and it
# only takes one of them to forget the theme.
$md = Join-Path $root 'README.md'
# The SVG case is the whole reason this script exists, so it does not depend on
# one happening to be lying around in the repo.
$svg = Join-Path $outDir 'theme-probe.svg'
@'
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 240 160" width="240" height="160">
  <circle cx="80" cy="80" r="52" fill="#3b82f6"/>
  <rect x="130" y="40" width="80" height="80" rx="10" fill="#f59e0b"/>
</svg>
'@ | Set-Content -LiteralPath $svg -Encoding UTF8
$png = Join-Path $root 'assets\logo.png'
$code = Join-Path $root 'src\bin\app\preview\highlight.rs'

$cases = @(
    @{ Name = 'settings'; Args = @('--window', 'settings') }
    @{ Name = 'settings-tab7'; Args = @('--window', 'settings', '--tab', '7') }
    @{ Name = 'convert'; Args = @('--window', 'convert') }
    @{ Name = 'about'; Args = @('--window', 'about') }
    @{ Name = 'feedback'; Args = @('--window', 'feedback') }
    @{ Name = 'ocr'; Args = @('--window', 'ocr') }
    @{ Name = 'preview-code'; Args = @('--window', 'preview', '--file', $code) }
    @{ Name = 'preview-markdown'; Args = @('--window', 'preview', '--file', $md) }
    @{ Name = 'preview-source'; Args = @('--window', 'preview', '--file', $md, '--source') }
)
if (Test-Path -LiteralPath $png) {
    $cases += @{ Name = 'preview-image'; Args = @('--window', 'preview', '--file', $png) }
}
# The exact content type QuickLook got wrong (their #1797).
$cases += @{ Name = 'preview-svg'; Args = @('--window', 'preview', '--file', $svg) }

$fail = 0
$rows = @()
$shas = @{}
foreach ($c in $cases) {
    $darkPng = Join-Path $outDir "$($c.Name)-dark.png"
    $lightPng = Join-Path $outDir "$($c.Name)-light.png"
    Invoke-Shot 'dark' $darkPng $c.Args
    Invoke-Shot 'light' $lightPng $c.Args

    $dl = Get-MeanLuma $darkPng
    $ll = Get-MeanLuma $lightPng
    $darkSha = Get-Sha $darkPng
    $shas[$c.Name] = $darkSha
    $identical = $darkSha -eq (Get-Sha $lightPng)

    $why = @()
    if ($identical) { $why += 'IDENTICAL (theme ignored)' }
    elseif ($ll -le $dl) { $why += "light ($ll) is not brighter than dark ($dl)" }
    elseif (($ll - $dl) -lt 20) { $why += "barely differs (dark $dl vs light $ll)" }

    $ok = $why.Count -eq 0
    if (-not $ok) { $fail++ }
    $rows += [pscustomobject]@{ Case = $c.Name; Dark = $dl; Light = $ll; Status = $(if ($ok) { 'OK' } else { $why -join '; ' }) }
}

# Guard the guard: if two different cases produce the same dark capture, the
# harness is not exercising what it claims to be.
$dupes = $shas.GetEnumerator() | Group-Object -Property Value | Where-Object { $_.Count -gt 1 }
foreach ($d in $dupes) {
    $names = ($d.Group | ForEach-Object { $_.Key }) -join ', '
    Write-Host " FAIL these cases rendered IDENTICAL output, so they are not distinct: $names" -ForegroundColor Red
    $fail++
}

$rows | ForEach-Object {
    $tag = if ($_.Status -eq 'OK') { '  OK  ' } else { ' FAIL ' }
    "{0}{1,-18} dark={2,-6} light={3,-6} {4}" -f $tag, $_.Case, $_.Dark, $_.Light, $_.Status
}

if ($Keep) { "`nPNGs kept in $outDir" } else { Remove-Item -Recurse -Force $outDir -EA SilentlyContinue }

if ($fail) {
    Write-Host "`n[theme] FAIL - $fail of $($cases.Count) surfaces do not follow the theme" -ForegroundColor Red
    exit 1
}
Write-Host "`n[theme] PASS - all $($cases.Count) surfaces follow light/dark" -ForegroundColor Green
