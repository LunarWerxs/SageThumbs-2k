# Regenerate the SageThumbs 2K UI screenshots + the Settings walkthrough GIF — HEADLESSLY.
#
# Everything here uses the app's built-in `--shot*` modes, which build the target window
# OFF-SCREEN (invisible, never steal focus) and render it via PrintWindow. NOTHING appears on
# screen and the desktop is never driven — so this is safe to run anytime and is the way to
# refresh the README / site assets after a UI change. Re-run after `cargo build --release`.
#
#   pwsh scripts\make-shots.ps1
#
# Produces (into assets\screenshots and mirrors the GIF into site\img):
#   settings.gif  — animated walkthrough cycling all 8 Settings category tabs (README + site)
#   convert.png   — the Convert… dialog (spare asset)
#
# NOTE: the eyedropper (`--shot <png> --window eyedropper`) captures the LIVE primary monitor,
# so it's intentionally NOT part of this automated pipeline — grab it manually when the desktop
# is staged.
$ErrorActionPreference = 'Stop'
$root  = Split-Path -Parent $PSScriptRoot
$exe   = 'D:\st2k-target\release\SageThumbs2K.exe'

if (-not (Test-Path $exe)) {
    Write-Host 'Release EXE missing — building...' -ForegroundColor Yellow
    Push-Location $root
    cargo build --release --bin SageThumbs2K
    Pop-Location
}

$assets  = Join-Path $root 'assets\screenshots'
$siteimg = Join-Path $root 'site\img'
New-Item -ItemType Directory -Force -Path $assets, $siteimg | Out-Null

# The install path has a SPACE, and Start-Process's array ArgumentList mis-splits quoted
# paths — so build ONE command-line string with each path explicitly double-quoted.
function Shot([string]$argline, [string]$out) {
    if (Test-Path $out) { Remove-Item $out -Force }
    $p = Start-Process $exe -ArgumentList $argline -PassThru -Wait
    if ($p.ExitCode -ne 0 -or -not (Test-Path $out)) {
        throw "shot failed (exit $($p.ExitCode)): $argline"
    }
    Write-Host ("  {0}  ({1:N0} bytes)" -f (Split-Path $out -Leaf), (Get-Item $out).Length)
}

Write-Host 'Generating Settings walkthrough GIF (cycles all tabs)...'
$gif = Join-Path $assets 'settings.gif'
Shot "--shot-gif `"$gif`"" $gif
Copy-Item $gif (Join-Path $siteimg 'settings.gif') -Force
Write-Host "  -> mirrored to site\img\settings.gif"

Write-Host 'Generating Convert dialog PNG...'
$cvt = Join-Path $assets 'convert.png'
Shot "--shot `"$cvt`" --window convert" $cvt
Copy-Item $cvt (Join-Path $siteimg 'convert.png') -Force
Write-Host "  -> mirrored to site\img\convert.png"

Write-Host 'Done.' -ForegroundColor Green
