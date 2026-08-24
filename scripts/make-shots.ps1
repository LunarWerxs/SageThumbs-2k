# Regenerate the SageThumbs 2K UI screenshots + the Settings walkthrough GIF — HEADLESSLY.
#
# Everything here uses the app's built-in `--shot*` modes, which build the target window
# OFF-SCREEN (invisible, never steal focus) and render it via PrintWindow. NOTHING appears on
# screen and the desktop is never driven — so this is safe to run anytime and is the way to
# refresh the README / site assets after a UI change. Re-run after `cargo build --release`.
#
#   pwsh scripts\make-shots.ps1
#   pwsh scripts\make-shots.ps1 -ExePath C:\some\other\target\release\SageThumbs2K.exe
#
# Produces (into assets\screenshots and mirrors into site\img):
#   settings.gif          — animated walkthrough cycling EVERY Settings category tab (README + site).
#                           The frame loop is `0..NCAT` in settings_dlg\shot.rs, so a page added to
#                           the nav rail appears here automatically. Don't write the count into this
#                           comment: it said "9" while the dialog had ten, because Appearance landed
#                           on 2026-08-09 and nothing here was looking.
#   convert.png           — the Convert… dialog (spare asset)
#   preview-quicklook.png — the Quick preview viewer over a syntax-highlighted source file
#                           (stable in-repo input: src\bin\app\preview\highlight.rs)
#
# NOTE: the eyedropper (`--shot <png> --window eyedropper`) captures the LIVE primary monitor,
# so it's intentionally NOT part of this automated pipeline — grab it manually when the desktop
# is staged.
param(
    # Override the built EXE's location. Defaults to Cargo's configured target-dir
    # (read from `cargo metadata`, which honors .cargo/config.toml's `build.target-dir`)
    # so this works for any contributor regardless of drive letter/checkout path,
    # falling back to the workspace-relative `target\release` if metadata can't be
    # read (e.g. offline/no cargo on PATH).
    [string]$ExePath
)
$ErrorActionPreference = 'Stop'
$root  = Split-Path -Parent $PSScriptRoot

if (-not $ExePath) {
    $targetDir = $null
    try {
        $meta = cargo metadata --no-deps --format-version 1 2>$null | ConvertFrom-Json
        if ($meta) { $targetDir = $meta.target_directory }
    } catch { }
    if (-not $targetDir) { $targetDir = Join-Path $root 'target' }
    $ExePath = Join-Path $targetDir 'release\SageThumbs2K.exe'
}
$exe = $ExePath

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

# Quick preview viewer over a real syntax-highlighted source file. Uses an in-repo input so
# the shot is reproducible on any checkout (no external fixture); `--file` renders that path
# through the same headless PrintWindow path as every other asset.
Write-Host 'Generating Quick preview viewer PNG (syntax-highlighted code)...'
$qv  = Join-Path $assets 'preview-quicklook.png'
$src = Join-Path $root 'src\bin\app\preview\highlight.rs'
Shot "--shot `"$qv`" --window preview --file `"$src`"" $qv
Copy-Item $qv (Join-Path $siteimg 'preview-quicklook.png') -Force
Write-Host "  -> mirrored to site\img\preview-quicklook.png"

# The four-up hero collage (README + the site's Quick preview section). It lives in Python
# because it composites, and it is called from here so there is ONE command that refreshes
# every generated asset. It was hand-made until 2026-08-24, and by then it was advertising
# "316 formats" over a caption toolbar that no longer existed — nothing could see either.
# Skipped, not failed, when Pillow is absent: a missing optional dep must not take down the
# assets that do not need it.
Write-Host 'Generating Quick preview hero collage...'
& python -c 'import PIL' 2>$null
if ($LASTEXITCODE -eq 0) {
    & python (Join-Path $PSScriptRoot 'make-collage.py') $exe
    if ($LASTEXITCODE -ne 0) { throw "make-collage.py failed (exit $LASTEXITCODE)" }
} else {
    Write-Host '  SKIPPED - Pillow not installed (pip install pillow)' -ForegroundColor Yellow
}

Write-Host 'Done.' -ForegroundColor Green
