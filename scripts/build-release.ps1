<#
  build-release.ps1 - the SageThumbs 2K release pipeline.

  One command produces a distributable installer:
    1. reads the version from Cargo.toml
    2. cargo build --release  (MSVC)
    3. stages the DLL + Options EXE + docs + a curated, hardened ImageMagick
    4. compiles packaging\installer.iss with Inno Setup (ISCC)
    5. prints the resulting SageThumbs2K-Setup-<ver>.exe and its size

  Usage:  pwsh scripts\build-release.ps1            # full build + installer
          pwsh scripts\build-release.ps1 -NoImageMagick   # skip the IM bundle (small installer)
  Output: dist\SageThumbs2K-Setup-<ver>.exe
#>
[CmdletBinding()]
param(
    [switch]$NoImageMagick,
    [switch]$SkipBuild,
    # Skip the signed sparse package (the Win11 modern context menu). Use only if
    # the Windows SDK isn't installed; the classic menu still ships either way.
    [switch]$NoModernMenu
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$targetRel = Join-Path (& "$PSScriptRoot\_targetdir.ps1") 'release'

# 1) Version from Cargo.toml -------------------------------------------------
$ver = ([regex]::Match((Get-Content "$root\Cargo.toml" -Raw), '(?m)^\s*version\s*=\s*"([^"]+)"')).Groups[1].Value
if (-not $ver) { throw "Could not read version from Cargo.toml" }
Write-Host "SageThumbs 2K release pipeline - version $ver" -ForegroundColor Cyan

# 2) Build -------------------------------------------------------------------
# Statically link the MSVC CRT into the shipped binaries so the DLL has NO external
# VC++ Redistributable dependency — regsvr32/DllRegisterServer can't fail with
# 0x8007007E (ERROR_MOD_NOT_FOUND) on a clean machine missing the VC++ runtime.
# Set here (TRACKED) so every release build is reproducibly crt-static even from a
# fresh clone; the machine-local .cargo/config.toml carries the same flag for dev
# builds. (RUSTFLAGS overrides config [target] rustflags — keep them identical.)
$env:RUSTFLAGS = '-C target-feature=+crt-static'
if (-not $SkipBuild) {
    # CBR/RAR is now the pure-Rust `rars` crate (always on, no feature). `webp-lossy`
    # (libwebp, BSD — the one optional C piece) is enabled for the shipped installer;
    # the plain `cargo build` dev/clean build leaves it off (then lossy-WebP convert
    # falls back to lossless WebP).
    $feat = @('--features', 'webp-lossy')
    Write-Host "[1/4] cargo build --release $($feat -join ' ')" -ForegroundColor Green
    Push-Location $root
    try { cargo build --release @feat; if ($LASTEXITCODE) { throw "cargo build failed" } } finally { Pop-Location }

    # --- Slim shell-extension DLL ------------------------------------------------
    # WHY a SECOND build: build.rs compiles locales/*.toml into ONE static LOCALES
    # table that's `include!`d by the shared lib, and the main `cargo build` above
    # emits all three artifacts (cdylib + 2 EXEs) from a SINGLE compilation. build.rs
    # runs once per cargo invocation and can't know which artifact will consume the
    # generated table, and there's no per-crate-type cfg. So we do a separate
    # `cargo build --lib` with the `dll-i18n-subset` feature: that rebuild filters
    # the LOCALES table down to the `menu_*` keys the DLL actually looks up, shrinking
    # the cdylib by ~0.2–0.28 MB. The EXEs keep the full 36-language table (the main
    # build above). We MUST pass the SAME extra features as the main build so the slim
    # DLL is otherwise identical (webp-lossy affects linked code, not just EXEs).
    $featSlim = @('--features', 'webp-lossy,dll-i18n-subset')
    Write-Host "[1b/4] cargo build --lib --release $($featSlim -join ' ')  (slim DLL)" -ForegroundColor Green
    Push-Location $root
    try { cargo build --lib --release @featSlim; if ($LASTEXITCODE) { throw "slim DLL build failed" } } finally { Pop-Location }
}

# 3) Stage -------------------------------------------------------------------
Write-Host "[2/4] staging payload" -ForegroundColor Green
$stage = "$root\packaging\stage"
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory "$stage\magick" -Force | Out-Null
# NOTE: the slim `cargo build --lib --features dll-i18n-subset` step above rebuilt
# sagethumbs2k.dll IN PLACE at $targetRel (overwriting the full-table DLL from the
# main build), so this copy stages the SLIM (menu_*-only) cdylib. The two EXEs below
# still come from the full-table main build. (Verify: the slim DLL must NOT contain
# an app-only translated string like the German `about_tagline`, but MUST contain a
# `menu_*` value — see the script header / build.rs note.)
Copy-Item "$targetRel\sagethumbs2k.dll" $stage
Copy-Item "$targetRel\sagethumbs2k-app.exe" $stage
Copy-Item "$targetRel\st2k.exe" $stage  # the command-line / AI-agent tool
foreach ($doc in 'README.md','LICENSE','LICENSE-MIT','LICENSE-APACHE') {
    if (Test-Path "$root\$doc") { Copy-Item "$root\$doc" $stage }
}
# Branding: the app icon (installer + shortcut) and swappable logo/banner art
# (dropping these next to the EXE overrides the embedded defaults at runtime).
foreach ($asset in 'app.ico','logo.png','banner.png') {
    if (Test-Path "$root\assets\$asset") { Copy-Item "$root\assets\$asset" $stage }
}

$bundleMagick = -not $NoImageMagick
if ($bundleMagick) {
    $im = (Get-ChildItem 'C:\Program Files\ImageMagick*' -Directory -EA SilentlyContinue | Select-Object -First 1)
    if (-not $im) { throw "ImageMagick not found in Program Files. Install it or pass -NoImageMagick." }
    Write-Host "      bundling a TRIMMED ImageMagick from $($im.Name)" -ForegroundColor DarkGray
    # We only ever decode a raster image -> PNG. ImageMagick's engine is tiny
    # (MagickCore+MagickWand ~3.5 MB) but the stock install ships ~25 MB of LAZY
    # delegates we never use: the GUI's MFC runtime, HEIF/AVIF + JPEG-XL + EXR +
    # WebP (handled by the image crate / WIC tiers BEFORE ImageMagick is reached),
    # and the cairo/pango/rsvg SVG-render stack (we use resvg; SVG is policy-off).
    # Dropping them was regression-verified to lose ZERO decodable formats. The
    # glib/harfbuzz/freetype/raqm text-shaping stack stays - MagickCore HARD-links
    # it at load, so magick.exe won't start without it.
    Copy-Item "$($im.FullName)\magick.exe" "$stage\magick"
    Copy-Item "$($im.FullName)\*.dll" "$stage\magick"
    Copy-Item "$($im.FullName)\*.xml" "$stage\magick"
    if (Test-Path "$($im.FullName)\modules") { Copy-Item "$($im.FullName)\modules" "$stage\magick" -Recurse }

    # Prune the verified-unneeded delegate DLLs (~24 MB) + their dead coders.
    $dropDll = @(
        'mfc140u.dll','msvcp140.dll','msvcp140_2.dll','vcomp140.dll',           # GUI/C++ runtimes magick.exe doesn't use
        'CORE_RL_heif_.dll','CORE_RL_jpeg-xl_.dll','CORE_RL_exr_.dll',          # handled by image crate / WIC
        'CORE_RL_webp_.dll','CORE_RL_Magick++_.dll','CORE_RL_brotli_.dll',
        'CORE_RL_cairo_.dll','CORE_RL_pango_.dll','CORE_RL_rsvg_.dll',          # SVG/vector render (we use resvg)
        'CORE_RL_croco_.dll','CORE_RL_gdk-pixbuf_.dll'
    )
    foreach ($d in $dropDll) { [System.IO.File]::Delete("$stage\magick\$d") }
    $dropCoder = 'heic','heif','avif','jxl','exr','webp','svg','msvg','video','mpeg','url','clipboard'
    foreach ($c in $dropCoder) { [System.IO.File]::Delete("$stage\magick\modules\coders\IM_MOD_RL_$($c)_.dll") }

    # magick.exe (and our MSVC binaries) need the VC++ runtime - bundle it app-local
    # so the long-tail tier works even on machines without the VC++ redist installed.
    foreach ($vc in 'vcruntime140.dll','vcruntime140_1.dll') {
        $src = Join-Path $env:SystemRoot "System32\$vc"
        if (Test-Path $src) { Copy-Item $src "$stage\magick" -Force }
    }

    # Overwrite the stock policy.xml with our hardened one.
    Copy-Item "$root\packaging\imagemagick-policy.xml" "$stage\magick\policy.xml" -Force
    $magickSize = [math]::Round((Get-ChildItem "$stage\magick" -Recurse -File | Measure-Object Length -Sum).Sum / 1MB, 1)
    Write-Host "      trimmed ImageMagick bundle: $magickSize MB (raw)" -ForegroundColor DarkGray
} else {
    Remove-Item "$stage\magick" -Recurse -Force
}

# 3b) Signed sparse package for the Win11 modern context menu ----------------
# Builds + signs (self-signed, free) SageThumbs2K.msix + SageThumbs2K.cer into the
# stage dir; the installer trusts the cert and sideloads the package (no Developer
# Mode needed). Without it the install still works — only the classic menu ships.
if (-not $NoModernMenu) {
    Write-Host "[2b/4] building signed sparse package (modern menu)" -ForegroundColor Green
    & "$root\packaging\make-msix.ps1" -OutDir $stage
} else {
    Write-Host "[2b/4] -NoModernMenu: skipping the signed package (classic menu only)" -ForegroundColor Yellow
}

# 4) Compile the installer ---------------------------------------------------
Write-Host "[3/4] compiling installer (Inno Setup)" -ForegroundColor Green
$iscc = @(
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) {
    # Fall back to the registry (Inno can install to a non-standard location).
    foreach ($r in 'HKLM:\SOFTWARE\WOW6432Node','HKLM:\SOFTWARE','HKCU:\SOFTWARE') {
        $hit = Get-ChildItem "$r\Microsoft\Windows\CurrentVersion\Uninstall" -EA SilentlyContinue |
            ForEach-Object { Get-ItemProperty $_.PSPath -EA SilentlyContinue } |
            Where-Object { $_.DisplayName -match 'Inno Setup' -and $_.InstallLocation } |
            ForEach-Object { Join-Path $_.InstallLocation 'ISCC.exe' } |
            Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
        if ($hit) { $iscc = $hit; break }
    }
}
if (-not $iscc) { throw "ISCC.exe (Inno Setup) not found. Install with: winget install JRSoftware.InnoSetup" }
Write-Host "      ISCC: $iscc" -ForegroundColor DarkGray
New-Item -ItemType Directory "$root\dist" -Force | Out-Null
& $iscc "/DAppVer=$ver" "$root\packaging\installer.iss"
if ($LASTEXITCODE) { throw "Inno Setup compile failed" }

# 5) Report ------------------------------------------------------------------
$setup = Get-ChildItem "$root\dist\SageThumbs2K-Setup-*.exe" | Sort-Object LastWriteTime -Descending | Select-Object -First 1
Write-Host "[4/4] done" -ForegroundColor Green
Write-Host ("  -> {0}  ({1} MB)" -f $setup.FullName, [math]::Round($setup.Length / 1MB, 1)) -ForegroundColor Cyan
