<#
  build-release.ps1 - the SageThumbs 2K release pipeline.

  One command produces a distributable installer:
    1. reads the version from Cargo.toml
    2. cargo build --release  (MSVC)
    3. stages the DLL + Options EXE + docs + a curated, hardened ImageMagick
    4. compiles packaging\installer.iss with Inno Setup (ISCC)
    5. prints the resulting SageThumbs2K-Setup-<ver>.exe and its size

  Usage:  pwsh scripts\build-release.ps1                         # x64 Full
          pwsh scripts\build-release.ps1 -NoImageMagick          # x64 Compact
          pwsh scripts\build-release.ps1 -Architecture arm64     # ARM64 Compact
          pwsh scripts\build-release.ps1 -Portable               # x64 portable zip
  Output: dist\SageThumbs2K-Setup-<ver>[-arm64].exe
          dist\SageThumbs2K-Portable-<ver>[-arm64].zip   (with -Portable)
#>
[CmdletBinding()]
param(
    [ValidateSet('x64', 'arm64')]
    [string]$Architecture = 'x64',
    [switch]$NoImageMagick,
    [switch]$SkipBuild,
    # Skip the signed sparse package (the Win11 modern context menu). Use only if
    # the Windows SDK isn't installed; the classic menu still ships either way.
    [switch]$NoModernMenu,
    # Produce the no-install zip instead of the installer. Ships the two EXEs, the same
    # curated ImageMagick payload, and a marker `SageThumbs2K.ini` that switches settings
    # storage from HKCU to that file. Deliberately does NOT ship the shell extension: a
    # thumbnail/context-menu handler only loads if its COM class is registered, so there is
    # no such thing as a portable one. See PORTABLE.txt (written below) for the full scope.
    [switch]$Portable
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'release-manifest-lib.ps1')
$targetRoot = & "$PSScriptRoot\_targetdir.ps1"
$targetTriple = if ($Architecture -eq 'arm64') {
    'aarch64-pc-windows-msvc'
} else {
    'x86_64-pc-windows-msvc'
}
$targetRel = if ($Architecture -eq 'arm64') {
    Join-Path $targetRoot "$targetTriple\release"
} else {
    Join-Path $targetRoot 'release'
}
# -Portable stages into its OWN directory. It runs the same staging code over the same inputs,
# so the two payloads still cannot drift; what it must never do is share the DIRECTORY, because
# staging wipes and rebuilds it and the portable pass deliberately omits the DLL. Sharing would
# let a portable build run inside a release silently gut the stage that
# check-release-manifest.ps1 validates the installer against.
$stage = if ($Portable) {
    Join-Path $root "packaging\stage\portable-src-$Architecture"
} else {
    Join-Path $root "packaging\stage\$Architecture"
}
$stageRelative = "stage\$Architecture"
$outputSuffix = if ($Architecture -eq 'arm64') { '-arm64' } else { '' }
# ARM64 used to be forced Compact here because there was no approved ImageMagick payload
# for it. There is now: packaging\imagemagick-source-arm64.json pins the SAME upstream
# 7.1.2-29 release as x64, so both architectures build Full unless -NoImageMagick is passed.

function Import-Arm64BuildEnvironment {
    $vcvarsCandidates = @()
    if ($env:VSINSTALLDIR) {
        $vcvarsCandidates += Join-Path $env:VSINSTALLDIR 'VC\Auxiliary\Build\vcvarsall.bat'
    }
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path -LiteralPath $vswhere -PathType Leaf) {
        # Collect the FULL output first, then take the first line. Piping a native
        # command straight into `Select-Object -First 1` stops the pipeline early, and
        # PowerShell then never sets $LASTEXITCODE at all — which under the StrictMode
        # that release-manifest-lib.ps1 turns on is a hard "cannot be retrieved" error,
        # not a zero. That made the whole ARM64 build path fail on the first native
        # command of a fresh shell, and appear to work in any session that had already
        # run one. Capture the code immediately, before anything can truncate it.
        $vswhereOutput = @(& $vswhere -latest -products * `
            -requires Microsoft.VisualStudio.Component.VC.Tools.ARM64 `
            -property installationPath)
        $vswhereExit = $LASTEXITCODE
        $installPath = $vswhereOutput | Select-Object -First 1
        if ($vswhereExit -eq 0 -and $installPath) {
            $vcvarsCandidates += Join-Path $installPath 'VC\Auxiliary\Build\vcvarsall.bat'
        }
    }
    $vcvarsCandidates += Join-Path ${env:ProgramFiles(x86)} `
        'Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat'
    $vcvars = $vcvarsCandidates | Where-Object {
        Test-Path -LiteralPath $_ -PathType Leaf
    } | Select-Object -First 1
    if (-not $vcvars) {
        throw 'Visual Studio with Microsoft.VisualStudio.Component.VC.Tools.ARM64 is required'
    }
    $cmdLine = '"{0}" amd64_arm64 >nul && set' -f $vcvars
    $environment = @(& $env:COMSPEC /d /s /c $cmdLine)
    if ($LASTEXITCODE -ne 0) {
        throw 'vcvarsall amd64_arm64 failed; install Microsoft.VisualStudio.Component.VC.Tools.ARM64'
    }
    foreach ($line in $environment) {
        $equals = $line.IndexOf('=')
        if ($equals -le 0) { continue }
        $name = $line.Substring(0, $equals)
        $value = $line.Substring($equals + 1)
        Set-Item -LiteralPath "Env:$name" -Value $value
    }
    $link = Get-Command link.exe -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $link -or $link.Source -notmatch 'Hostx64\\arm64\\link\.exe$' -or
        $env:LIB -notmatch '\\lib\\arm64(?:;|$)') {
        throw 'vcvarsall did not expose the Hostx64->ARM64 linker/libraries'
    }
    Write-Host "      ARM64 linker: $($link.Source)" -ForegroundColor DarkGray
}

function Resolve-RcExe {
    # Windows SDK rc.exe. Needed by STAGING as well as by compiling (the magick bundling step
    # re-versions stubbed DLLs with it), so both the build path and the -SkipBuild path resolve
    # it through here. It used to be resolved only while building, which left `-SkipBuild`
    # reaching that staging code with $rcExe undefined.
    $rc = Get-Command rc.exe -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $rc) {
        $rc = Get-ChildItem (Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin') `
            -Filter rc.exe -File -Recurse -ErrorAction SilentlyContinue |
            Where-Object FullName -Match '\\(x64|arm64)\\rc\.exe$' |
            Sort-Object FullName -Descending | Select-Object -First 1
    }
    if (-not $rc) {
        throw 'Windows SDK rc.exe is required for ARM64 manifest/icon/version resources'
    }
    $rc.Source ?? $rc.FullName
}

# 1) Version from Cargo.toml -------------------------------------------------
$ver = ([regex]::Match((Get-Content "$root\Cargo.toml" -Raw), '(?m)^\s*version\s*=\s*"([^"]+)"')).Groups[1].Value
if (-not $ver) { throw "Could not read version from Cargo.toml" }
Write-Host "SageThumbs 2K release pipeline - version $ver ($Architecture)" -ForegroundColor Cyan

# 2) Build -------------------------------------------------------------------
# Statically link the MSVC CRT into the shipped binaries so the DLL has NO external
# VC++ Redistributable dependency — regsvr32/DllRegisterServer can't fail with
# 0x8007007E (ERROR_MOD_NOT_FOUND) on a clean machine missing the VC++ runtime.
# Set here (TRACKED) so every release build is reproducibly crt-static even from a
# fresh clone; the machine-local .cargo/config.toml carries the same flag for dev
# builds. (RUSTFLAGS overrides config [target] rustflags — keep them identical.)
$env:RUSTFLAGS = '-C target-feature=+crt-static'
$exeBuildArgs = @(Get-ReleaseCargoBuildArguments -Architecture $Architecture -Package sagethumbs2k -Features 'webp-lossy,html-preview,hdr-capture')
$dllBuildArgs = @(Get-ReleaseCargoBuildArguments -Architecture $Architecture -Package sagethumbs2k-dll -Features 'webp-lossy,dll-i18n-subset')
# The ARM64 toolchain environment is needed by STAGING, not just by compiling: the magick
# bundling step inspects ARM64 PEs with MSVC `dumpbin` and re-versions stub DLLs with `rc.exe`,
# and both arrive via vcvars. It used to be set up only inside the `-not $SkipBuild` block
# below, so `-SkipBuild` reached that staging code with neither tool resolved and died on
# whichever one it touched first. That is precisely the invocation release.ps1 makes at [4a/6]
# for the ARM64 portable zip, so it would have failed a release after main was pushed and CI
# was green. Hoisted here, before the branch, and idempotent.
if ($Architecture -eq 'arm64' -and $SkipBuild) {
    Write-Host "[1/4] -SkipBuild: importing the ARM64 toolchain anyway (staging needs rc/dumpbin)" -ForegroundColor DarkGray
    Import-Arm64BuildEnvironment
    $rcExe = Resolve-RcExe
}
if (-not $SkipBuild) {
    # Version metadata + app manifest + icon are embedded into the binaries via windres
    # in build.rs, which SILENTLY falls back to NO metadata if windres isn't on PATH.
    # Metadata-less / manifest-less binaries are classic heuristic-AV false-positive bait,
    # so FAIL the release build loudly here rather than ship flag-bait (a plain dev
    # `cargo build` stays tolerant — this guard is release-only).
    if ($Architecture -eq 'arm64') {
        Import-Arm64BuildEnvironment
        $rcExe = Resolve-RcExe
        Write-Host "      rc.exe: $rcExe" -ForegroundColor DarkGray
    } else {
        $windres = Get-Command windres, x86_64-w64-mingw32-windres, llvm-windres -EA SilentlyContinue | Select-Object -First 1
        if (-not $windres) {
            throw "windres not found on PATH. build.rs needs it to embed VERSIONINFO/manifest/icon; " +
                  "without it the release binaries ship with NO version metadata (a common AV " +
                  "false-positive trigger). Install binutils/LLVM (e.g. " +
                  "'winget install BrechtSanders.WinLibs.POSIX.UCRT' or LLVM), then retry."
        }
        Write-Host "      windres: $($windres.Source)" -ForegroundColor DarkGray
    }

    # CBR/RAR is now the pure-Rust `rars` crate (always on, no feature). `webp-lossy`
    # (libwebp, BSD — the one optional C piece) is enabled for the shipped installer;
    # the plain `cargo build` dev/clean build leaves it off (then lossy-WebP convert
    # falls back to lossless WebP).
    # `-p sagethumbs2k`: the rlib + the two EXEs (full 36-language i18n). The DLL is a
    # SEPARATE package (`sagethumbs2k-dll`) built slim below — so we can't `--features`
    # the whole workspace at once (cargo rejects `--features` across >1 package).
    # `html-preview` links webview2-com into the EXEs only (the slim DLL build never requests it,
    # so the shell-extension cdylib stays free of it — verify with `cargo tree -p sagethumbs2k-dll`).
    Write-Host "[1/4] cargo build $($exeBuildArgs -join ' ')  (rlib + EXEs)" -ForegroundColor Green
    Push-Location $root
    try { cargo build @exeBuildArgs; if ($LASTEXITCODE) { throw "cargo build failed" } } finally { Pop-Location }

    # --- Slim shell-extension DLL ------------------------------------------------
    # The DLL (`sagethumbs2k-dll` cdylib) is built SEPARATELY with `dll-i18n-subset`
    # forwarded to the core crate, which filters build.rs's static LOCALES table down
    # to the `menu_*` keys the DLL actually looks up (~0.2–0.28 MB smaller). The EXEs
    # (built above, full 36-language table) are a DIFFERENT package, so there's no
    # feature-unification clash — the two `-p` builds key their core-crate artifacts by
    # feature set independently. Same `webp-lossy` so the slim DLL is otherwise identical.
    # A portable drop ships no DLL (nothing registers it), so this whole ~90 s pass is dead
    # weight there — and worse, it would overwrite the target dir's DLL with the slim
    # menu_*-only build as a side effect of producing a zip that doesn't contain one.
    if ($Portable) {
        Write-Host "[1b/4] -Portable: skipping the slim DLL build (the zip ships no DLL)" -ForegroundColor Yellow
    } else {
        Write-Host "[1b/4] cargo build $($dllBuildArgs -join ' ')  (slim DLL)" -ForegroundColor Green
        Push-Location $root
        try { cargo build @dllBuildArgs; if ($LASTEXITCODE) { throw "slim DLL build failed" } } finally { Pop-Location }
    }
}

# 3) Stage -------------------------------------------------------------------
Write-Host "[2/4] staging payload" -ForegroundColor Green
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory $stage -Force | Out-Null
# NOTE: the slim `cargo build --lib --features dll-i18n-subset` step above rebuilt
# sagethumbs2k.dll IN PLACE at $targetRel (overwriting the full-table DLL from the
# main build), so this copy stages the SLIM (menu_*-only) cdylib. The two EXEs below
# still come from the full-table main build. (Verify: the slim DLL must NOT contain
# an app-only translated string like the German `about_tagline`, but MUST contain a
# `menu_*` value — see the script header / build.rs note.)
# (-Portable skips this: the slim DLL build was skipped too, so whatever sits at $targetRel
# is some earlier build's leftover — staging it would put a stale, unasked-for DLL in the
# tree even though the zip filters it out again below.)
if (-not $Portable) { Copy-Item "$targetRel\sagethumbs2k.dll" $stage }
# The cargo bin target is `SageThumbs2K`, so it builds as `SageThumbs2K.exe` directly
# (build.rs redirects its PDB to avoid the case-collision with the DLL — see Cargo.toml).
Copy-Item "$targetRel\SageThumbs2K.exe" $stage
Copy-Item "$targetRel\st2k.exe" $stage  # the command-line / AI-agent tool
foreach ($doc in 'README.md','LICENSE','LICENSE-MIT','LICENSE-APACHE') {
    if (Test-Path "$root\$doc") { Copy-Item "$root\$doc" $stage }
}
# Always ship the hardened policy with the core app. Compact installs can still
# use an explicitly installed Program Files ImageMagick fallback; it must receive
# the same restrictions even when the curated engine component is not selected.
Copy-Item "$root\packaging\imagemagick-policy.xml" "$stage\policy.xml" -Force
# Branding: the app icon (installer + shortcut) and swappable logo/banner art
# (dropping these next to the EXE overrides the embedded defaults at runtime).
foreach ($asset in 'app.ico','logo.png','banner.png') {
    if (Test-Path "$root\assets\$asset") { Copy-Item "$root\assets\$asset" $stage }
}

$bundleMagick = -not $NoImageMagick
if ($bundleMagick) {
    New-Item -ItemType Directory "$stage\magick" -Force | Out-Null
    # Release input is PINNED. Never package whichever ImageMagick directory happens to
    # sort first: patch releases change imports/exports and can make a previously safe trim
    # silently incomplete. check-magick-source verifies the reported identity plus a
    # deterministic inventory hash of all 195 files eligible to enter this bundle.
    # One pin PER ARCHITECTURE. Both describe the same upstream 7.1.2-29 release and the
    # same 195-file set, so only the bundle bytes differ; the inventory algorithm is shared.
    $magickPinPath = if ($Architecture -eq 'arm64') {
        Join-Path $root 'packaging\imagemagick-source-arm64.json'
    } else {
        Join-Path $root 'packaging\imagemagick-source.json'
    }
    $magickPin = Get-Content -LiteralPath $magickPinPath -Raw | ConvertFrom-Json
    $imPath = Join-Path $env:ProgramFiles ([string]$magickPin.identity.installDirectoryName)
    if (-not (Test-Path -LiteralPath $imPath -PathType Container)) {
        throw "Pinned ImageMagick '$($magickPin.identity.displayName)' not found at '$imPath'. " +
              "Provide that exact $Architecture Q16-HDRI build or pass -NoImageMagick."
    }
    & "$PSScriptRoot\check-magick-source.ps1" -SourcePath $imPath -PinPath $magickPinPath -Architecture $Architecture
    if ($LASTEXITCODE) { throw "Pinned ImageMagick source validation failed" }
    $im = Get-Item -LiteralPath $imPath
    Write-Host "      bundling a TRIMMED, PINNED ImageMagick from $($im.Name)" -ForegroundColor DarkGray

    # A full production build always emits the same stubbed payload. Falling back to the
    # stock +5 MiB text stack made installer size and hashes depend on the build machine.
    # The same MinGW distribution provides all four tools.
    $mingwBin = $null
    $wr = Get-Command windres, x86_64-w64-mingw32-windres -EA SilentlyContinue | Select-Object -First 1
    if ($wr) { $mingwBin = Split-Path $wr.Source }
    if (-not $mingwBin -or -not (Test-Path (Join-Path $mingwBin 'gendef.exe'))) {
        $candidate = Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\*WinLibs*\mingw64\bin\gendef.exe" -EA SilentlyContinue |
            Select-Object -First 1
        if ($candidate) { $mingwBin = Split-Path $candidate.FullName }
    }
    $gendef = if ($mingwBin) { Join-Path $mingwBin 'gendef.exe' } else { $null }
    $gcc = if ($mingwBin) { Join-Path $mingwBin 'gcc.exe' } else { $null }
    $windresStub = if ($mingwBin) { Join-Path $mingwBin 'windres.exe' } else { $null }
    $objdump = if ($mingwBin) { Join-Path $mingwBin 'objdump.exe' } else { $null }
    # MinGW objdump only understands x86 PEs. For an ARM64 bundle use MSVC's dumpbin,
    # which reads every machine type and comes with the same VS BuildTools the ARM64
    # toolchain already needs. x64 deliberately keeps objdump so its proven path is
    # untouched; the two were checked to agree exactly on the x64 bundle's dependencies.
    $peInspector = $objdump
    if ($Architecture -eq 'arm64') {
        # `-ExpandProperty`, NOT `(...).Source`: when Get-Command finds nothing the pipeline is
        # EMPTY, and dotting a property off an empty result is a terminating error under the
        # StrictMode release-manifest-lib.ps1 turns on. That error fired before the filesystem
        # fallback below could run - so the fallback written for exactly this case was
        # unreachable. It only shows with `-SkipBuild`, which skips Import-Arm64BuildEnvironment
        # and therefore never puts dumpbin on PATH; that is the invocation release.ps1 makes at
        # [4a/6] for the ARM64 portable zip.
        $peInspector = Get-Command dumpbin.exe -CommandType Application -ErrorAction SilentlyContinue |
            Select-Object -First 1 -ExpandProperty Source -ErrorAction SilentlyContinue
        if (-not $peInspector) {
            $vsRoot = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC'
            $peInspector = (Get-ChildItem $vsRoot -Filter dumpbin.exe -File -Recurse -ErrorAction SilentlyContinue |
                Where-Object FullName -Match '\\Hostx64\\x64\\dumpbin\.exe$' |
                Sort-Object FullName -Descending | Select-Object -First 1).FullName
        }
        if (-not $peInspector) {
            throw 'ARM64 bundling needs MSVC dumpbin.exe to inspect ARM64 PE imports (MinGW objdump cannot read them)'
        }
        Write-Host "      PE inspector: $peInspector" -ForegroundColor DarkGray
    }
    $missingStubTools = @(
        @{ Name = 'gendef'; Path = $gendef },
        @{ Name = 'gcc'; Path = $gcc },
        @{ Name = 'windres'; Path = $windresStub },
        @{ Name = 'objdump'; Path = $objdump }
    ) | Where-Object { -not $_.Path -or -not (Test-Path -LiteralPath $_.Path -PathType Leaf) }
    if ($missingStubTools) {
        throw "Production ImageMagick packaging requires gendef, gcc, windres, and objdump " +
              "from WinLibs/MinGW; missing: $(($missingStubTools.Name) -join ', '). " +
              "Install with 'winget install BrechtSanders.WinLibs.POSIX.UCRT' or use -NoImageMagick."
    }

    # SageThumbs uses ImageMagick only for bounded raster decoding and explicit
    # image-output writers. Its core engine (MagickCore+MagickWand) is small, but
    # the stock install ships ~25 MB of LAZY
    # delegates we never use: the GUI's MFC runtime, the WebP CODER (handled by
    # the image crate), and the cairo/pango/rsvg SVG-render stack (we use resvg;
    # SVG is policy-off). HEIF/AVIF and JPEG-XL MUST stay even though earlier tiers
    # decode them: the Convert dialog advertises those ImageMagick-backed OUTPUT
    # formats and needs their encoders. EXR output is now native via `image`.
    # CORE_RL_webp_.dll itself stays: the retained TIFF delegate hard-imports it, even
    # when decoding a non-WebP TIFF. Dropping it made TIFF fail on a clean machine.
    # Dropping the other entries was regression-verified to lose ZERO decodable formats.
    # glib/harfbuzz/freetype/fribidi/raqm text-shaping stack (~5 MB) is HARD-linked by
    # MagickCore at load (magick.exe won't start without it) but is pure dead weight - we
    # only process raster pixels and never render text/captions - so we STUB it below.
    Copy-Item "$($im.FullName)\magick.exe" "$stage\magick"
    Copy-Item "$($im.FullName)\*.dll" "$stage\magick"
    Copy-Item "$($im.FullName)\*.xml" "$stage\magick"
    Copy-Item "$($im.FullName)\License.txt" "$stage\magick"
    Copy-Item "$($im.FullName)\NOTICE.txt" "$stage\magick"
    Copy-Item "$($im.FullName)\modules" "$stage\magick" -Recurse

    # Prune the verified-unneeded delegate DLLs (~24 MB) + their dead coders.
    # msvcp140.dll and vcomp140.dll are LOAD-BEARING dependencies of RAW/MagickCore;
    # keep them app-local because neither is guaranteed on clean Windows.
    $dropDll = @(
        'mfc140u.dll','msvcp140_2.dll',                                        # unreferenced GUI/C++ runtime pieces
        'CORE_RL_Magick++_.dll','CORE_RL_exr_.dll',                            # EXR encode/decode is native Rust
        'CORE_RL_cairo_.dll','CORE_RL_pango_.dll','CORE_RL_rsvg_.dll',          # SVG/vector render (we use resvg)
        'CORE_RL_croco_.dll','CORE_RL_gdk-pixbuf_.dll'
    )
    foreach ($d in $dropDll) { [System.IO.File]::Delete("$stage\magick\$d") }

    # PANGO is not an advertised file extension and the shipped security policy denies
    # the synthetic PANGO text-render input coder. Prove both invariants before removing
    # its module; this is what allows the otherwise-unused cairo/pango delegate DLLs above
    # to stay out without leaving an unresolved import.
    [xml]$magickPolicy = Get-Content -LiteralPath "$root\packaging\imagemagick-policy.xml" -Raw
    $deniedCoderAliases = [System.Collections.Generic.HashSet[string]]::new(
        [System.StringComparer]::OrdinalIgnoreCase
    )
    foreach ($policy in $magickPolicy.policymap.policy) {
        if ($policy.domain -ne 'coder' -or $policy.rights -ne 'none') { continue }
        $tokens = ([string]$policy.pattern).Trim('{}').Split(',') | ForEach-Object { $_.Trim() }
        foreach ($token in $tokens) { [void]$deniedCoderAliases.Add($token) }
    }
    if (-not $deniedCoderAliases.Contains('PANGO')) {
        throw 'Refusing to prune the PANGO coder: packaging/imagemagick-policy.xml no longer denies PANGO'
    }
    # This asks OUR CLI which formats we advertise, purely to prove we are not about to
    # prune a coder we actually expose. The answer comes from `formats::FORMATS`, which is
    # the same table in every build, so ANY host-native st2k.exe answers it correctly.
    # That matters because cross-building ARM64 on an x64 host produces an st2k.exe this
    # machine cannot execute at all; querying the native one keeps the safety check real
    # instead of skipping it whenever the architectures differ.
    $hostArchNow = if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq 'Arm64') { 'arm64' } else { 'x64' }
    $formatsProbe = "$targetRel\st2k.exe"
    if ($Architecture -cne $hostArchNow) {
        $nativeProbe = @(
            (Join-Path $targetRoot 'release\st2k.exe')
            (Join-Path $targetRoot 'debug\st2k.exe')
        ) | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
        if (-not $nativeProbe) {
            throw "Cross-building $Architecture on an $hostArchNow host: need a host-native " +
                  "st2k.exe to read the advertised format list before pruning ImageMagick. " +
                  "Run `cargo build` (or `cargo build --release`) first."
        }
        $formatsProbe = $nativeProbe
        Write-Host "      format list read from the host-native $([System.IO.Path]::GetFileName((Split-Path $formatsProbe -Parent)))\st2k.exe" -ForegroundColor DarkGray
    }
    $advertisedFormats = @(& $formatsProbe formats 2>&1)
    if ($LASTEXITCODE -ne 0) { throw 'Could not query st2k formats before ImageMagick pruning' }
    if (($advertisedFormats -join "`n") -match '(?im)^\s*\.pango\s') {
        throw 'Refusing to prune the PANGO coder: .pango is now an advertised SageThumbs input format'
    }

    # These modules expose only coder aliases that our policy already disables.
    # Removing them physically makes that security boundary independent of an
    # additive Program Files policy search and trims dead code. Fail closed if a
    # future policy edit re-enables even one alias.
    $policyOnlyCoderModules = [ordered]@{
        'caption'    = @('CAPTION')
        'ept'        = @('EPT','EPT2','EPT3')
        'html'       = @('HTM','HTML','SHTML')
        'inline'     = @('DATA','INLINE')
        'label'      = @('LABEL')
        'msl'        = @('MSL')
        'mvg'        = @('MVG')
        'pdf'        = @('AI','EPDF','PDF','PDFA','POCKETMOD')
        'ps'         = @('PS','EPI','EPS','EPSF','EPSI')
        'ps2'        = @('EPS2','PS2')
        'ps3'        = @('EPS3','PS3')
        'screenshot' = @('SCREENSHOT')
        'ttf'        = @('DFONT','OTF','PFA','PFB','TTC','TTF')
        'txt'        = @('SPARSE-COLOR','TEXT','TXT')
        'xps'        = @('XPS')
    }
    foreach ($module in $policyOnlyCoderModules.GetEnumerator()) {
        foreach ($alias in $module.Value) {
            if (-not $deniedCoderAliases.Contains($alias)) {
                throw "Refusing to prune the $($module.Key) coder: policy no longer denies alias $alias"
            }
        }
    }

    # EXR/HDR/Farbfeld input + output are native Rust tiers now. PAM itself is
    # native too, but PFM shares ImageMagick's PNM module, so that module must stay.
    $dropCoder = @(
        'exr','hdr','farbfeld','webp','svg','msvg','video','mpeg','url','clipboard','pango'
    ) + @($policyOnlyCoderModules.Keys)
    foreach ($c in $dropCoder) { [System.IO.File]::Delete("$stage\magick\modules\coders\IM_MOD_RL_$($c)_.dll") }

    # STUB the text-shaping stack (~5 MB raw). MagickCore hard-links glib/harfbuzz/freetype/
    # fribidi/raqm at load, so they can't just be deleted - magick.exe won't start. But we never
    # render text, so we replace each with a tiny stub DLL exporting the same symbols as no-ops:
    # the import table resolves at load, the text functions are simply never called on the
    # raster-decode path. Regenerated from the installed ImageMagick's own exports on every build,
    # so an IM upgrade adapts automatically after the source pin + regression corpus are
    # deliberately updated. We compare the generated stub's export inventory to upstream
    # before accepting it. See docs/MAGICK.md.
    # STUBS ARE x86-ONLY. gendef/gcc/dlltool come from MinGW, which emits x86_64 PEs no
    # matter what we are targeting: the first ARM64 Full build replaced GENUINE ARM64
    # freetype/glib/raqm with x64 stubs, which an ARM64 process cannot load at all. Until
    # someone builds these stubs with the ARM64 toolchain, ARM64 ships the real upstream
    # text-stack DLLs. That costs a few MB and is strictly correct; a broken bundle is not
    # a trade worth making for size. The staged-architecture assertion below is what caught
    # this, and it stays regardless.
    # Export extraction (gendef) is architecture-independent; only the compile/link half
    # is toolchain-specific, so ARM64 stubs with MSVC and x64 keeps gcc/windres.
    if ($true) {
    $stubWork = Join-Path $stage 'magick\_stubwork'
    New-Item -ItemType Directory $stubWork -Force | Out-Null
    try {
        foreach ($t in 'glib','harfbuzz','freetype','fribidi','raqm') {
            $dll = "CORE_RL_$($t)_.dll"
            $src = Join-Path $im.FullName $dll
            if (-not (Test-Path -LiteralPath $src -PathType Leaf)) {
                throw "Pinned ImageMagick is missing required stub source: $src"
            }
            Push-Location $stubWork
            try {
                Remove-Item -LiteralPath "CORE_RL_$($t)_.def" -Force -ErrorAction SilentlyContinue
                & $gendef $src 2>$null | Out-Null
                if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath "CORE_RL_$($t)_.def")) {
                    throw "gendef failed for $src"
                }
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
                if ($buildDef.Count -le 1) { throw "gendef found no exports in $src" }
                $expectedExports = @($buildDef | Select-Object -Skip 1 | Sort-Object -CaseSensitive)
                Set-Content 'stub.c' $stubC -Encoding ascii
                Set-Content 'build.def' $buildDef -Encoding ascii
                # Embed a VERSIONINFO resource so the stub looks like a legit (versioned) DLL,
                # NOT a hollow metadata-less one — hollow DLLs are heuristic-AV false-positive
                # bait (verified: a stub WITHOUT this scored 6/64 on VirusTotal, WITH it 1/69).
                # Same principle the Rust binaries already follow via build.rs/windres.
                # Version literals come from the PIN, never a hard-coded string: a stub has to
                # claim the version of the DLL it stands in for, and an ImageMagick bump must
                # not need an edit here to stay truthful.
                $stubVersion = [string]$magickPin.identity.fileVersion
                $stubRcVersion = ($stubVersion -replace '[-.]', ',')
                $rc = @(
                    '1 VERSIONINFO',
                    "FILEVERSION $stubRcVersion", "PRODUCTVERSION $stubRcVersion",
                    'FILEFLAGSMASK 0x3fL', 'FILEOS 0x40004L', 'FILETYPE 0x2L',
                    'BEGIN',
                    '  BLOCK "StringFileInfo"', '  BEGIN', '    BLOCK "040904b0"', '    BEGIN',
                    '      VALUE "CompanyName", "SageThumbs 2K"',
                    '      VALUE "FileDescription", "ImageMagick text-shaping shim (no-op; raster-only build)"',
                    "      VALUE ""FileVersion"", ""$stubVersion""",
                    "      VALUE ""InternalName"", ""CORE_RL_$($t)_""",
                    "      VALUE ""OriginalFilename"", ""$dll""",
                    '      VALUE "ProductName", "SageThumbs 2K"',
                    "      VALUE ""ProductVersion"", ""$stubVersion""",
                    '      VALUE "LegalCopyright", "Shipped with SageThumbs 2K"',
                    '    END', '  END',
                    '  BLOCK "VarFileInfo"', '  BEGIN', '    VALUE "Translation", 0x409, 1200', '  END',
                    'END'
                )
                Set-Content 'version.rc' $rc -Encoding ascii
                if ($Architecture -eq 'arm64') {
                    & $rcExe /nologo 'version.rc' 2>&1 | Out-Null
                    if (-not (Test-Path -LiteralPath 'version.res')) { throw "rc failed while versioning $dll" }
                } else {
                    & $windresStub 'version.rc' -O coff -o 'version.o' 2>$null
                    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath 'version.o')) {
                        throw "windres failed while versioning $dll"
                    }
                }

                $stubPath = Join-Path "$stage\magick" $dll
                if ($Architecture -eq 'arm64') {
                    # MSVC ARM64: /NODEFAULTLIB plus an explicit entry keeps the stub as
                    # hollow as the MinGW one, and /DEF carries the exact export set.
                    & cl /nologo /c /O2 /GS- 'stub.c' 2>&1 | Out-Null
                    if (-not (Test-Path -LiteralPath 'stub.obj')) { throw "cl failed while building $dll" }
                    # /IMPLIB is REQUIRED: link emits <name>.lib and <name>.exp beside /OUT,
                    # which drops build artifacts straight into the shipped magick bundle.
                    # The installer lint caught exactly that. Send them to the temp dir.
                    & link /nologo /DLL /MACHINE:ARM64 /NODEFAULTLIB /ENTRY:DllMainCRTStartup `
                        /DEF:build.def 'stub.obj' 'version.res' `
                        "/IMPLIB:$(Join-Path $stubWork ""CORE_RL_$($t)_.lib"")" "/OUT:$stubPath" 2>&1 | Out-Null
                    if (-not (Test-Path -LiteralPath $stubPath -PathType Leaf)) {
                        throw "link failed while building $dll"
                    }
                } else {
                    $gccArgs = @('-O2', '-shared', '-nostdlib', '-o', $stubPath, 'stub.c', 'build.def', '-e', 'DllMainCRTStartup', 'version.o')
                    & $gcc @gccArgs 2>$null
                    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $stubPath -PathType Leaf)) {
                        throw "gcc failed while building $dll"
                    }
                }

                # Re-extract from the finished stub and compare exact name/DATA shape.
                Remove-Item -LiteralPath "CORE_RL_$($t)_.def" -Force
                & $gendef $stubPath 2>$null | Out-Null
                if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath "CORE_RL_$($t)_.def")) {
                    throw "gendef could not inspect generated stub $stubPath"
                }
                $actualExports = [System.Collections.Generic.List[string]]::new()
                $inActualExports = $false
                foreach ($line in (Get-Content "CORE_RL_$($t)_.def")) {
                    if ($line -match '^EXPORTS') { $inActualExports = $true; continue }
                    if (-not $inActualExports) { continue }
                    if ($line -match '^([A-Za-z_]\S*)') {
                        $entry = $Matches[1]
                        if ($line -match '\bDATA\b') { $entry += ' DATA' }
                        $actualExports.Add($entry)
                    }
                }
                $actualSorted = @($actualExports | Sort-Object -CaseSensitive)
                if ([string]::Join("`n", $actualSorted) -cne [string]::Join("`n", $expectedExports)) {
                    throw "Generated stub export inventory does not exactly match upstream $dll"
                }
            } finally { Pop-Location }
        }
    } finally {
        Remove-Item $stubWork -Recurse -Force -EA SilentlyContinue
    }
    Write-Host "      stubbed + export-verified the magick text stack (glib/harfbuzz/freetype/fribidi/raqm)" -ForegroundColor DarkGray
    }

    # These pinned-build companions become unreachable after pruning/stubbing above.
    # The helper refuses each deletion unless no staged PE import and no ASCII/UTF-16
    # LoadLibrary/configuration literal refers to it. Keep this an explicit reviewed list;
    # never turn it into a broad "delete every zero-indegree DLL" sweep because Magick
    # discovers coder modules dynamically.
    $unreferencedRuntime = @(
        'msvcp140_1.dll',
        'msvcp140_atomic_wait.dll',
        'msvcp140_codecvt_ids.dll',
        'vcruntime140_threads.dll',
        'CORE_RL_fribidi_.dll',
        'CORE_RL_harfbuzz_.dll'
    )
    # This candidate list is only unreferenced BECAUSE stubbing removed the code that
    # imported it. ARM64 does not stub (MinGW stubs are x86-only), so those DLLs are
    # genuinely still referenced there and the helper correctly refuses to delete them.
    # Run the prune only where the precondition it was written for actually holds.
    if ($true) {
        & "$PSScriptRoot\prune-magick-unreferenced.ps1" -BundlePath "$stage\magick" -ObjdumpPath $peInspector -Candidate $unreferencedRuntime
    } else {
        Write-Host "      runtime prune SKIPPED for $Architecture (its candidates stay referenced without stubbing)" -ForegroundColor Yellow
        $global:LASTEXITCODE = 0
    }
    if ($LASTEXITCODE) { throw "Mechanically verified ImageMagick runtime pruning failed" }

    # Overwrite the stock policy.xml with our hardened one.
    Copy-Item "$root\packaging\imagemagick-policy.xml" "$stage\magick\policy.xml" -Force

    # Authoritative final gate: reject any third-party PE import not present in the
    # bundle, then execute the exact flattened layout with bundle-local modules/config.
    # Cross-architecture: inspect with the arch-capable tool, and SKIP only the part that
# actually executes magick.exe (impossible for an ARM64 binary on an x64 host). The
# arm64 CI job runs the real smoke test natively on ARM hardware.
    # x64-ONLY CRT COMPONENT. vcruntime140_1.dll carries the x64 exception-handling
    # helpers and has no ARM64 counterpart - yet upstream ImageMagick 7.1.2-29 ships an
    # x64 copy of it inside its ARM64 installer. Staging it puts a foreign-machine DLL in
    # an ARM64 bundle. Nothing on ARM64 imports it, so it is dropped.
    if ($Architecture -cne 'x64') {
        $strayCrt = Join-Path $stage 'magick\vcruntime140_1.dll'
        if (Test-Path -LiteralPath $strayCrt) {
            [System.IO.File]::Delete($strayCrt)
            Write-Host "      dropped x64-only vcruntime140_1.dll from the $Architecture bundle" -ForegroundColor DarkGray
        }
    }

    # EVERY staged PE must be the architecture we are building. This guard caught MinGW
    # emitting x64 stubs into an ARM64 bundle, and upstream's stray x64 CRT; either would
    # have shipped an installer whose DLLs the loader simply refuses to load.
    $expectedMachine = if ($Architecture -eq 'arm64') { 0xAA64 } else { 0x8664 }
    $foreign = @(Get-ChildItem $stage -Recurse -File |
        Where-Object { $_.Extension -in '.exe', '.dll' } |
        Where-Object {
            $bytes = [System.IO.File]::ReadAllBytes($_.FullName)
            if ($bytes.Length -lt 64) { return $false }
            $peOffset = [BitConverter]::ToInt32($bytes, 0x3c)
            if ($peOffset -lt 0 -or $peOffset + 6 -gt $bytes.Length) { return $true }
            [BitConverter]::ToUInt16($bytes, $peOffset + 4) -ne $expectedMachine
        })
    if ($foreign) {
        throw ("Staged $Architecture payload contains $($foreign.Count) non-$Architecture PE(s): " +
               (($foreign.Name | Sort-Object) -join ', '))
    }
    Write-Host "      every staged PE is $Architecture" -ForegroundColor DarkGray

$bundleCheckArgs = @{ BundlePath = "$stage\magick"; ObjdumpPath = $peInspector }
if ($Architecture -cne $hostArchNow) { $bundleCheckArgs['SkipSmoke'] = $true }
& "$PSScriptRoot\check-magick-bundle.ps1" @bundleCheckArgs
    if ($LASTEXITCODE) { throw "Staged ImageMagick dependency/smoke validation failed" }

    # The staged regression RUNS the staged st2k.exe over the corpus, so it can only
    # execute when the staged binaries match the host. Cross-building ARM64 on an x64
    # host would report all ~260 formats "broken" purely because the process cannot
    # start. Skipping it here does not drop the gate: the arm64 CI job runs on native
    # ARM hardware and exercises the same binaries there. Never let this skip apply to
    # a same-architecture build, which is the case that catches real staging breakage.
    if ($Architecture -cne $hostArchNow) {
        Write-Host "      staged corpus regression DEFERRED: $Architecture payload on an $hostArchNow host (runs natively in the arm64 CI job)" -ForegroundColor Yellow
    } else {
        & "$PSScriptRoot\test-staged-regression.ps1" -StagePath $stage
        if ($LASTEXITCODE) { throw "Exact staged full-corpus regression failed" }
    }

    $magickSize = [math]::Round((Get-ChildItem "$stage\magick" -Recurse -File | Measure-Object Length -Sum).Sum / 1MB, 1)
    Write-Host "      trimmed ImageMagick bundle: $magickSize MB (raw)" -ForegroundColor DarkGray
} else {
    Remove-Item "$stage\magick" -Recurse -Force -ErrorAction SilentlyContinue
}

# 3a-portable) The no-install zip -------------------------------------------
# Reuses the stage verbatim, so the portable drop and the installer are built from the
# SAME binaries and the SAME curated/pruned ImageMagick — there is no second payload
# recipe to drift. Everything below is assembly: flatten, add the marker ini, zip, exit.
if ($Portable) {
    Write-Host "[2c/3] assembling portable zip" -ForegroundColor Green
    $portableStage = Join-Path $root "packaging\stage\portable-$Architecture"
    if (Test-Path $portableStage) { Remove-Item $portableStage -Recurse -Force }
    New-Item -ItemType Directory $portableStage -Force | Out-Null

    # The shell extension and its sideloading apparatus are install-only by nature and must
    # not travel in a zip: a DLL nothing registered is dead weight, and the .msix/.cer pair
    # only means anything to an installer that trusts the cert and calls Add-AppxPackage.
    $installOnly = 'sagethumbs2k.dll', 'SageThumbs2K.msix', 'SageThumbs2K.cer', 'AppxManifest.xml'
    Get-ChildItem $stage -File |
        Where-Object { $installOnly -notcontains $_.Name } |
        Copy-Item -Destination $portableStage
    # magick lives beside the EXEs, not in a subfolder: the lookup in decode/magick.rs probes
    # the running module's OWN directory, which is what makes the bundle travel at all.
    if (Test-Path "$stage\magick") {
        Copy-Item "$stage\magick\*" $portableStage -Recurse -Force
    }

    # The marker IS the config file. Its presence next to the EXE is the entire portable
    # switch (src/settings.rs `store`), so an empty one means "factory defaults, stored here".
    #
    # That also makes the filename load-bearing across two languages, and getting it wrong
    # fails SILENTLY: the app finds no marker, quietly uses HKCU, and the zip looks fine while
    # doing the one thing it promised not to. So take the name from the Rust const rather than
    # trusting a literal here to stay in sync with it.
    $iniConst = [regex]::Match(
        (Get-Content "$root\src\settings.rs" -Raw),
        '(?m)^\s*pub const INI_NAME:\s*&str\s*=\s*"([^"]+)"'
    )
    if (-not $iniConst.Success) {
        throw "couldn't read INI_NAME out of src\settings.rs - the portable marker name is " +
              "defined there and must not be duplicated as a literal in this script"
    }
    $iniName = $iniConst.Groups[1].Value
    Write-Host "      portable marker: $iniName (from settings.rs)" -ForegroundColor DarkGray
    Set-Content -LiteralPath "$portableStage\$iniName" -Encoding utf8 -Value @(
        '; SageThumbs 2K portable settings.'
        '; Delete this file to go back to storing settings in the registry.'
    )

    Set-Content -LiteralPath "$portableStage\PORTABLE.txt" -Encoding utf8 -Value @(
        "SageThumbs 2K $ver - portable"
        ''
        'Extract anywhere and run SageThumbs2K.exe. Nothing is installed, nothing is'
        'written to the registry, and no administrator rights are needed. Settings live'
        'in SageThumbs2K.ini next to the exe; delete that file and the app goes back to'
        'storing them in the registry like the installed build does.'
        ''
        'WHAT YOU GET'
        '  SageThumbs2K.exe   settings, convert/resize, quick preview, screenshots, OCR,'
        '                     the colour picker, and the folder tools'
        '  st2k.exe           the command line tool and MCP server (run: st2k --help)'
        ''
        'WHAT YOU DO NOT GET'
        '  Explorer thumbnails and the right-click menu. Windows only loads a shell'
        '  extension whose COM class is registered, so those cannot work from a zip by'
        '  definition - not a limitation we chose. Install the normal build for those.'
        ''
        'The screenshot tool runs while the app is open but does not add itself to logon'
        'startup, since this copy can be moved or unplugged at any time.'
        ''
        'ONE EXCEPTION TO THE NO-REGISTRY RULE'
        '  If you sign in to settings sync, the sign-in it saves is stored by Windows for'
        '  your user account rather than in the ini. It has to be: Windows ties it to the'
        '  account so it could not be read on another machine anyway. Do not sign in if you'
        '  want this copy to leave nothing at all behind. Everything else stays in the ini.'
    )

    New-Item -ItemType Directory "$root\dist" -Force | Out-Null
    $zipPath = "$root\dist\SageThumbs2K-Portable-$ver$outputSuffix.zip"
    Remove-Item -LiteralPath $zipPath -Force -ErrorAction SilentlyContinue
    Compress-Archive -Path "$portableStage\*" -DestinationPath $zipPath -CompressionLevel Optimal
    if (-not (Test-Path -LiteralPath $zipPath -PathType Leaf)) {
        throw "portable zip was not produced: $zipPath"
    }

    # Prove the drop actually runs before calling it an artifact. `st2k formats` exercises
    # the real binary from its final location; a payload missing a VC runtime or a CORE_RL
    # DLL fails here rather than in a user's hands.
    #
    # Only when the host can actually execute it. An x64 box cannot run ARM64 binaries at
    # all (the emulation goes the other way), so cross-building the ARM64 zip would fail this
    # check for a reason that says nothing about the payload. Skipping is the honest outcome,
    # but say so loudly: an unsmoked zip is exactly the one to hand to an ARM64 machine first.
    $hostArch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' }
    if ($hostArch -eq $Architecture) {
        $smoke = & "$portableStage\st2k.exe" formats 2>&1 | Select-Object -First 1
        if ($LASTEXITCODE -or $smoke -notmatch '^\d+\s') {
            throw "portable payload failed its smoke test (st2k formats): $smoke"
        }
        Write-Host "      smoke test: $smoke" -ForegroundColor DarkGray
    } else {
        Write-Host "      SMOKE TEST SKIPPED - $Architecture payload can't run on this $hostArch host." `
            -ForegroundColor Yellow
        Write-Host "      Run 'st2k.exe formats' from the extracted zip on a real $Architecture machine before shipping it." `
            -ForegroundColor Yellow
    }

    $zip = Get-Item -LiteralPath $zipPath
    Write-Host "[3/3] done" -ForegroundColor Green
    Write-Host ("  -> {0}  ({1} MB zipped, {2} MB extracted)" -f $zip.FullName,
        [math]::Round($zip.Length / 1MB, 1),
        [math]::Round((Get-ChildItem $portableStage -Recurse -File | Measure-Object Length -Sum).Sum / 1MB, 1)
    ) -ForegroundColor Cyan
    return
}

# 3b) Signed sparse package for the Win11 modern context menu ----------------
# Builds + signs (self-signed, free) SageThumbs2K.msix + SageThumbs2K.cer into the
# stage dir; the installer trusts the cert and sideloads the package (no Developer
# Mode needed). Without it the install still works — only the classic menu ships.
if (-not $NoModernMenu) {
    Write-Host "[2b/4] building signed sparse package (modern menu)" -ForegroundColor Green
    & "$root\packaging\make-msix.ps1" -OutDir $stage -Architecture $Architecture
} else {
    Write-Host "[2b/4] -NoModernMenu: skipping the signed package (classic menu only)" -ForegroundColor Yellow
}

# 4) Compile the installer ---------------------------------------------------
Write-Host "[3/4] compiling installer (Inno Setup)" -ForegroundColor Green
# Static lint of installer.iss [Code] FIRST: ISCC compiles uninstaller-only runtime bugs
# happily (they only fire in unins000.exe, a path our dev loop never runs), so a green
# compile can still ship a broken uninstaller - that's how issue #3 (TSetupForm.Create ->
# "Resource TSetupForm not found") escaped. Fail the build before wasting a compile on it.
$installerCheckArgs = @{
    IssPath = "$root\packaging\installer.iss"
    CorePolicyPath = "$stage\policy.xml"
}
if ($bundleMagick) {
    $installerCheckArgs.ManagedPayloadPath = "$stage\magick"
}
& "$PSScriptRoot\check-installer.ps1" @installerCheckArgs
if ($LASTEXITCODE) { throw "installer.iss [Code] lint failed (see above)" }
# The uninstall survey's email rule is one of THREE copies of the same rule (Pascal here, Rust
# in the app, JS on the server). This is the only place with ISCC, so it is the only place the
# Pascal copy can actually be EXECUTED against the shared table - CI reports that leg as SKIP.
& "$PSScriptRoot\check-email-rule.ps1"
if ($LASTEXITCODE) { throw "email-rule implementations disagree (see above)" }
$iscc = @(
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) {
    # Fall back to the registry (Inno can install to a non-standard location).
    # Most Uninstall keys have NO DisplayName/InstallLocation at all, and
    # release-manifest-lib.ps1 turns on StrictMode, under which touching a missing
    # property is a terminating error rather than $null. So probe the property bag
    # instead of dotting straight into it: the un-guarded version crashed here before
    # it could ever reach the per-user install this machine actually has, which would
    # have taken out the x64 release build too, not just ARM64.
    foreach ($r in 'HKLM:\SOFTWARE\WOW6432Node','HKLM:\SOFTWARE','HKCU:\SOFTWARE') {
        $hit = Get-ChildItem "$r\Microsoft\Windows\CurrentVersion\Uninstall" -EA SilentlyContinue |
            ForEach-Object { Get-ItemProperty $_.PSPath -EA SilentlyContinue } |
            Where-Object {
                $props = $_.PSObject.Properties
                $props['DisplayName'] -and $props['InstallLocation'] -and
                    $props['DisplayName'].Value -match 'Inno Setup' -and
                    $props['InstallLocation'].Value
            } |
            ForEach-Object { Join-Path $_.InstallLocation 'ISCC.exe' } |
            Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
        if ($hit) { $iscc = $hit; break }
    }
}
if (-not $iscc) { throw "ISCC.exe (Inno Setup) not found. Install with: winget install JRSoftware.InnoSetup" }
Write-Host "      ISCC: $iscc" -ForegroundColor DarkGray
New-Item -ItemType Directory "$root\dist" -Force | Out-Null
# Derive the LIVE format count from the just-built CLI and hand it to the installer
# (never hardcode the count — it's whatever FORMATS.len() returns; the old literal
# "316" in installer.iss was a drift bomb waiting for the next format addition).
$fmtCount = ''
if ($Architecture -eq 'x64') {
    $fmtLine = & "$targetRel\st2k.exe" formats 2>$null | Select-Object -First 1
    if ($fmtLine -match '^(\d+)\s') { $fmtCount = $Matches[1] }
}
$compactOnly = if ($NoImageMagick) { '1' } else { '0' }
$isccArgs = @(
    "/DAppVer=$ver",
    "/DArchitecture=$Architecture",
    "/DStageDir=$stageRelative",
    "/DCompactOnly=$compactOnly",
    "/DOutputSuffix=$outputSuffix"
)
if ($fmtCount) { $isccArgs += "/DFmtCount=$fmtCount" }
$expectedSetupPath = "$root\dist\SageThumbs2K-Setup-$ver$outputSuffix.exe"
# A stale same-version artifact must not survive an odd ISCC "success" and then be
# mistaken for the installer produced from this stage.
Remove-Item -LiteralPath $expectedSetupPath -Force -ErrorAction SilentlyContinue
& $iscc @isccArgs "$root\packaging\installer.iss"
if ($LASTEXITCODE) { throw "Inno Setup compile failed" }
if (-not (Test-Path -LiteralPath $expectedSetupPath -PathType Leaf)) {
    throw "Inno Setup exited successfully but did not create the expected installer: $expectedSetupPath"
}

# 5) Report ------------------------------------------------------------------
# Report the artifact for THIS version explicitly. A stale/newer installer left in dist must
# never be mistaken for the file this invocation just produced.
$setup = Get-Item -LiteralPath $expectedSetupPath -EA Stop
$sizeCheck = Join-Path $PSScriptRoot 'check-release-size.ps1'
& $sizeCheck -InstallerPath $setup.FullName -StagePath $stage -Architecture $Architecture
if ($LASTEXITCODE) { throw "release size budget check failed" }

# 6) Optionally refresh the local marketing-site checkout from the just-built truth.
# `site/` is deliberately local/ignored and is not an installer input or release-provenance
# source. Non-fatal: a missing checkout or site-generation hiccup must not fail a release.
$genSite = Join-Path $root 'scripts\gen-site.mjs'
if ($Architecture -eq 'x64' -and (Get-Command node -EA SilentlyContinue) -and
    (Test-Path -LiteralPath $genSite -PathType Leaf) -and
    (Test-Path -LiteralPath (Join-Path $root 'site\index.html') -PathType Leaf)) {
    Write-Host "[site] regenerating site\index.html from live formats" -ForegroundColor Green
    try { & node $genSite "$targetRel\st2k.exe" }
    catch { Write-Host "  (site regen skipped: $_)" -ForegroundColor DarkYellow }
}

# Last successful build action: bind installer, exact stage, source tree, build recipe,
# and feature switches into a release manifest. Publishing revalidates this rather than
# trusting a same-named artifact or a -SkipBuild invocation.
& "$PSScriptRoot\write-release-manifest.ps1" `
    -InstallerPath $setup.FullName `
    -StagePath $stage `
    -Version $ver `
    -ImageMagickBundled:$bundleMagick `
    -ModernMenuBundled:$(-not $NoModernMenu) `
    -RustBuildPerformed:$(-not $SkipBuild) `
    -ExeCargoArguments $exeBuildArgs `
    -DllCargoArguments $dllBuildArgs `
    -Architecture $Architecture
if ($LASTEXITCODE) { throw "release manifest generation failed" }

Write-Host "[4/4] done" -ForegroundColor Green
Write-Host ("  -> {0}  ({1} MB)" -f $setup.FullName, [math]::Round($setup.Length / 1MB, 1)) -ForegroundColor Cyan
