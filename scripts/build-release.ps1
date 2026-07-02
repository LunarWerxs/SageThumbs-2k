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
    # Version metadata + app manifest + icon are embedded into the binaries via windres
    # in build.rs, which SILENTLY falls back to NO metadata if windres isn't on PATH.
    # Metadata-less / manifest-less binaries are classic heuristic-AV false-positive bait,
    # so FAIL the release build loudly here rather than ship flag-bait (a plain dev
    # `cargo build` stays tolerant — this guard is release-only).
    $windres = Get-Command windres, x86_64-w64-mingw32-windres, llvm-windres -EA SilentlyContinue | Select-Object -First 1
    if (-not $windres) {
        throw "windres not found on PATH. build.rs needs it to embed VERSIONINFO/manifest/icon; " +
              "without it the release binaries ship with NO version metadata (a common AV " +
              "false-positive trigger). Install binutils/LLVM (e.g. " +
              "'winget install BrechtSanders.WinLibs.POSIX.UCRT' or LLVM), then retry."
    }
    Write-Host "      windres: $($windres.Source)" -ForegroundColor DarkGray

    # CBR/RAR is now the pure-Rust `rars` crate (always on, no feature). `webp-lossy`
    # (libwebp, BSD — the one optional C piece) is enabled for the shipped installer;
    # the plain `cargo build` dev/clean build leaves it off (then lossy-WebP convert
    # falls back to lossless WebP).
    # `-p sagethumbs2k`: the rlib + the two EXEs (full 36-language i18n). The DLL is a
    # SEPARATE package (`sagethumbs2k-dll`) built slim below — so we can't `--features`
    # the whole workspace at once (cargo rejects `--features` across >1 package).
    $feat = @('-p', 'sagethumbs2k', '--features', 'webp-lossy')
    Write-Host "[1/4] cargo build --release $($feat -join ' ')  (rlib + EXEs)" -ForegroundColor Green
    Push-Location $root
    try { cargo build --release @feat; if ($LASTEXITCODE) { throw "cargo build failed" } } finally { Pop-Location }

    # --- Slim shell-extension DLL ------------------------------------------------
    # The DLL (`sagethumbs2k-dll` cdylib) is built SEPARATELY with `dll-i18n-subset`
    # forwarded to the core crate, which filters build.rs's static LOCALES table down
    # to the `menu_*` keys the DLL actually looks up (~0.2–0.28 MB smaller). The EXEs
    # (built above, full 36-language table) are a DIFFERENT package, so there's no
    # feature-unification clash — the two `-p` builds key their core-crate artifacts by
    # feature set independently. Same `webp-lossy` so the slim DLL is otherwise identical.
    $featSlim = @('-p', 'sagethumbs2k-dll', '--features', 'webp-lossy,dll-i18n-subset')
    Write-Host "[1b/4] cargo build --release $($featSlim -join ' ')  (slim DLL)" -ForegroundColor Green
    Push-Location $root
    try { cargo build --release @featSlim; if ($LASTEXITCODE) { throw "slim DLL build failed" } } finally { Pop-Location }
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
# The cargo bin target is `sagethumbs2k-app` (avoids a PDB case-collision with the DLL);
# stage it under the shipped/running name the manifest + spawn code expect.
Copy-Item "$targetRel\sagethumbs2k-app.exe" (Join-Path $stage 'SageThumbs2K.exe')
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
    # glib/harfbuzz/freetype/fribidi/raqm text-shaping stack (~5 MB) is HARD-linked by
    # MagickCore at load (magick.exe won't start without it) but is pure dead weight - we
    # only decode raster -> PNG, never render text/captions - so we STUB it below.
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

    # STUB the text-shaping stack (~5 MB raw). MagickCore hard-links glib/harfbuzz/freetype/
    # fribidi/raqm at load, so they can't just be deleted - magick.exe won't start. But we never
    # render text, so we replace each with a tiny stub DLL exporting the same symbols as no-ops:
    # the import table resolves at load, the text functions are simply never called on the
    # raster-decode path. Regenerated from the installed ImageMagick's own exports on every build,
    # so an IM upgrade adapts automatically. Regression-verified to lose ZERO decodable formats
    # (glib included). See packaging/MAGICK.md. Needs gendef + gcc (the same mingw that provides
    # windres); if they're absent we warn and ship the full stack rather than fail the release.
    $mingwBin = $null
    $wr = Get-Command windres, x86_64-w64-mingw32-windres -EA SilentlyContinue | Select-Object -First 1
    if ($wr) { $mingwBin = Split-Path $wr.Source }
    if (-not $mingwBin -or -not (Test-Path (Join-Path $mingwBin 'gendef.exe'))) {
        $c = Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\*WinLibs*\mingw64\bin\gendef.exe" -EA SilentlyContinue | Select-Object -First 1
        if ($c) { $mingwBin = Split-Path $c.FullName }
    }
    $gendef = if ($mingwBin) { Join-Path $mingwBin 'gendef.exe' } else { $null }
    $gcc = if ($mingwBin) { Join-Path $mingwBin 'gcc.exe' } else { $null }
    $windresStub = if ($mingwBin) { Join-Path $mingwBin 'windres.exe' } else { $null }
    if ($gendef -and $gcc -and (Test-Path $gendef) -and (Test-Path $gcc)) {
        $stubWork = Join-Path $stage 'magick\_stubwork'
        New-Item -ItemType Directory $stubWork -Force | Out-Null
        foreach ($t in 'glib','harfbuzz','freetype','fribidi','raqm') {
            $dll = "CORE_RL_$($t)_.dll"
            $src = Join-Path $im.FullName $dll
            if (-not (Test-Path $src)) { continue }
            Push-Location $stubWork
            try {
                & $gendef $src 2>$null | Out-Null
                $stubC = @('int __stdcall DllMainCRTStartup(void* h,unsigned r,void* x){(void)h;(void)r;(void)x;return 1;}')
                $buildDef = @('EXPORTS'); $inExports = $false
                foreach ($l in (Get-Content "CORE_RL_$($t)_.def")) {
                    if ($l -match '^EXPORTS') { $inExports = $true; continue }
                    if (-not $inExports) { continue }
                    if ($l -match '^([A-Za-z_]\S*)') {
                        $n = $matches[1]
                        if ($l -match '\bDATA\b') { $stubC += "void* $n=0;"; $buildDef += "$n DATA" }
                        else { $stubC += "int $n(void){return 0;}"; $buildDef += "$n" }
                    }
                }
                Set-Content 'stub.c' $stubC -Encoding ascii
                Set-Content 'build.def' $buildDef -Encoding ascii
                # Embed a VERSIONINFO resource so the stub looks like a legit (versioned) DLL,
                # NOT a hollow metadata-less one — hollow DLLs are heuristic-AV false-positive
                # bait (verified: a stub WITHOUT this scored 6/64 on VirusTotal, WITH it 1/69).
                # Same principle the Rust binaries already follow via build.rs/windres.
                $rcObj = $null
                if ($windresStub -and (Test-Path $windresStub)) {
                    $rc = @(
                        '1 VERSIONINFO',
                        'FILEVERSION 7,1,2,0', 'PRODUCTVERSION 7,1,2,0',
                        'FILEFLAGSMASK 0x3fL', 'FILEOS 0x40004L', 'FILETYPE 0x2L',
                        'BEGIN',
                        '  BLOCK "StringFileInfo"', '  BEGIN', '    BLOCK "040904b0"', '    BEGIN',
                        '      VALUE "CompanyName", "SageThumbs 2K"',
                        '      VALUE "FileDescription", "ImageMagick text-shaping shim (no-op; raster-only build)"',
                        '      VALUE "FileVersion", "7.1.2.0"',
                        "      VALUE ""InternalName"", ""CORE_RL_$($t)_""",
                        "      VALUE ""OriginalFilename"", ""$dll""",
                        '      VALUE "ProductName", "SageThumbs 2K"',
                        '      VALUE "ProductVersion", "7.1.2.0"',
                        '      VALUE "LegalCopyright", "Shipped with SageThumbs 2K"',
                        '    END', '  END',
                        '  BLOCK "VarFileInfo"', '  BEGIN', '    VALUE "Translation", 0x409, 1200', '  END',
                        'END'
                    )
                    Set-Content 'version.rc' $rc -Encoding ascii
                    & $windresStub 'version.rc' -O coff -o 'version.o' 2>$null
                    if (Test-Path 'version.o') { $rcObj = 'version.o' }
                }
                $gccArgs = @('-O2', '-shared', '-nostdlib', '-o', (Join-Path "$stage\magick" $dll), 'stub.c', 'build.def', '-e', 'DllMainCRTStartup')
                if ($rcObj) { $gccArgs += $rcObj }
                & $gcc @gccArgs 2>$null
            } finally { Pop-Location }
        }
        Remove-Item $stubWork -Recurse -Force -EA SilentlyContinue
        Write-Host "      stubbed the magick text stack (glib/harfbuzz/freetype/fribidi/raqm, ~5 MB)" -ForegroundColor DarkGray
    } else {
        Write-Host "      WARNING: gendef/gcc not found (need mingw); shipping the FULL magick text stack (+5 MB)" -ForegroundColor Yellow
    }

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
