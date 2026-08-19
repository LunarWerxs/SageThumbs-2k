<#
  build-speed-corpus.ps1 — generate a SIZE-TIERED corpus: the same picture written into every
  format we can write, at small / medium / large, so decode speed can be read as a function of
  pixel count instead of "whatever size that one sample happened to be".

      pwsh scripts\build-speed-corpus.ps1              # build (skips what already exists)
      pwsh scripts\build-speed-corpus.ps1 -Rebuild     # regenerate everything
      pwsh scripts\build-speed-corpus.ps1 -Only png,avif

  WHY THIS EXISTS. The correctness corpus (`test-corpus`) and the real-content corpus
  (`test-corpus-real`) both hold ONE sample per format, at whatever size that sample happens
  to be. That is right for "does it render" and "is the picture correct", and useless for
  "how does this format scale": a 320 px PCX and a 6 MP JPEG are not comparable numbers, and
  a format's cost per megapixel is exactly what tells you whether a decoder is doing something
  stupid. So this builds a matrix instead — one identical source image, three sizes, every
  format that can hold it.

  THE SOURCE IS PHOTOGRAPHIC ON PURPOSE. A flat colour or a synthetic gradient compresses to
  almost nothing and decodes far faster than any real file, which would flatter every codec
  and hide exactly the differences worth seeing. This uses `plasma:` + added noise, which has
  real high-frequency content in every block.

  SELF-CHECKING, the fuzzseed discipline: a generated file is KEPT only if our own decoder can
  actually read it back. ImageMagick will happily write formats that then decode to nothing
  useful, and a sample that cannot decode is not a speed measurement — it is a silent hole in
  the matrix. Rejects are reported, never left lying around to be timed as "missing".

  Output: `..\..\test-corpus-speed\<ext>-<tier>.<ext>` (a sibling of the repo, like the other
  corpora, and NOT in git — it is fully regenerable and large).
#>
param(
    [string]$OutDir = "$PSScriptRoot\..\..\test-corpus-speed",
    [string[]]$Only,
    [switch]$Rebuild
)
$ErrorActionPreference = 'Stop'

$st2k = Join-Path (& "$PSScriptRoot\_targetdir.ps1") 'release\st2k.exe'
if (-not (Test-Path $st2k)) { throw "st2k.exe not built (cargo build --release --bin st2k)" }
if (-not (Get-Command magick -ErrorAction SilentlyContinue)) { throw "ImageMagick (magick) not on PATH" }

# The three tiers. Chosen to span the range that actually occurs in a user's folders: an icon
# or web asset, a phone/camera photo, and a modern high-resolution camera or scan. Each is
# ~6-24x the pixels of the one below, which is enough separation that a per-megapixel cost
# shows up clearly against measurement noise. `large` is 12 MP rather than something bigger
# because the slow ENCODERS (AVIF, JPEG XL, JPEG 2000) dominate build time well before decode
# time gets more interesting, and a 12 MP file is already past every camera this has to serve.
$tiers = [ordered]@{
    small  = @{ w = 320;  h = 240 }   # 0.08 MP
    medium = @{ w = 1600; h = 1200 }  # 1.9 MP
    large  = @{ w = 4000; h = 3000 }  # 12.0 MP
}

New-Item -ItemType Directory -Force $OutDir | Out-Null

# Formats we can BOTH write (ImageMagick) and read (st2k), minus the pseudo-formats that are
# not real files: raw pixel dumps need an explicit -size to read back at all, and the rest are
# generators/sinks rather than image containers.
$notReal = @(
    'aai', 'avs', 'null', 'clipboard', 'group4', 'inline', 'map', 'mask', 'mvg', 'six',
    'sixel', 'strimg', 'txt', 'ftxt', 'data', 'bayer', 'bayera', 'bgr', 'bgra', 'bgro',
    'cmyk', 'cmyka', 'gray', 'graya', 'mono', 'rgb', 'rgba', 'rgbo', 'ycbcr', 'ycbcra',
    'yuv', 'uyvy', 'pal', 'vid', 'xc', 'pango', 'label', 'caption', 'histogram',
    'msvg', 'rsvg', 'clip'
)
$supported = & $st2k formats 2>$null |
    Select-String '^\s+\.(\S+)' | ForEach-Object { $_.Matches[0].Groups[1].Value.ToLower() }
$writable = magick -list format 2>$null |
    Where-Object { $_ -match '^\s*\S+\*?\s+\S+\s+rw' } |
    ForEach-Object { ($_ -split '\s+')[1].TrimEnd('*').ToLower() } | Sort-Object -Unique
$exts = $writable | Where-Object { $supported -contains $_ -and $notReal -notcontains $_ }
if ($Only) {
    $want = $Only | ForEach-Object { $_.ToLowerInvariant().TrimStart('.') }
    $exts = $exts | Where-Object { $want -contains $_ }
}
Write-Host ("[speed-corpus] {0} formats x {1} tiers" -f $exts.Count, $tiers.Count) -ForegroundColor Cyan

# One photographic source per tier, reused for every format so the CONTENT is identical and
# only the container changes.
$src = @{}
foreach ($tier in $tiers.Keys) {
    $t = $tiers[$tier]
    $p = Join-Path $OutDir "_source-$tier.png"
    if ($Rebuild -or -not (Test-Path $p)) {
        & magick -size "$($t.w)x$($t.h)" 'plasma:fractal' `
            -attenuate 0.4 +noise Gaussian -quality 100 $p 2>$null
    }
    if (-not (Test-Path $p)) { throw "could not build the $tier source image" }
    $src[$tier] = $p
}

# PER-FORMAT OVERRIDES, for the handful where ImageMagick's DEFAULT output is not what the
# world actually ships. A speed baseline built from unrepresentative files answers the wrong
# question, and this one already did once: magick writes AVIF as 12-bit, profile 2, with no
# colour box at all — a shape no encoder in real use produces, and one that lands in the single
# documented slow path, so the AVIF row read "2.6x Windows" while every real AVIF is faster
# than Windows. ffmpeg's plain default (8-bit 4:2:0 with colour signalling) is what an actual
# AVIF looks like, so AVIF is generated with that instead.
#
# `ffmpegPix` = generate with ffmpeg at this pixel format instead of magick. `magickArgs` =
# extra arguments for the magick call, for a format where magick is right but its defaults
# are not.
$overrides = @{
    avif = @{ ffmpegPix = 'yuv420p' }   # the mainstream shape: 8-bit 4:2:0, nclx present
}
$ffmpeg = (Get-Command ffmpeg -ErrorAction SilentlyContinue).Source

$made = 0; $skipped = 0; $rejected = @()
foreach ($ext in $exts) {
    foreach ($tier in $tiers.Keys) {
        $out = Join-Path $OutDir "$ext-$tier.$ext"
        if (-not $Rebuild -and (Test-Path $out) -and (Get-Item $out).Length -gt 0) {
            $skipped++
            continue
        }
        Remove-Item -LiteralPath $out -Force -ErrorAction SilentlyContinue
        # `-quality 92` only binds on the lossy targets; everything else ignores it. Explicit
        # `$ext:` prefix so a format whose extension magick would otherwise sniff differently
        # still gets the coder we asked for.
        $ov = $overrides[$ext]
        if ($ov -and $ov.ffmpegPix -and $ffmpeg) {
            & $ffmpeg -hide_banner -loglevel error -y -i $src[$tier] -c:v libaom-av1 `
                -still-picture 1 -cpu-used 8 -crf 30 -pix_fmt $ov.ffmpegPix $out 2>$null
        } elseif ($ov -and $ov.magickArgs) {
            & magick $src[$tier] @($ov.magickArgs) -quality 92 "${ext}:$out" 2>$null
        } else {
            & magick $src[$tier] -quality 92 "${ext}:$out" 2>$null
        }
        if (-not (Test-Path $out) -or (Get-Item $out).Length -eq 0) {
            $rejected += "$ext-$tier (magick wrote nothing)"
            Remove-Item -LiteralPath $out -Force -ErrorAction SilentlyContinue
            continue
        }
        # SELF-CHECK: a sample we cannot decode is not a measurement.
        $probe = Join-Path $env:TEMP ("st2k-speedprobe-{0}.png" -f $PID)
        & $st2k thumbnail $out $probe --size 64 2>$null | Out-Null
        $ok = ($LASTEXITCODE -eq 0) -and (Test-Path $probe) -and ((Get-Item $probe).Length -gt 0)
        Remove-Item -LiteralPath $probe -Force -ErrorAction SilentlyContinue
        if ($ok) {
            $made++
        } else {
            $rejected += "$ext-$tier (we cannot decode it back)"
            Remove-Item -LiteralPath $out -Force -ErrorAction SilentlyContinue
        }
    }
}

Write-Host ("[speed-corpus] {0} written, {1} already present, {2} rejected" -f
    $made, $skipped, $rejected.Count) -ForegroundColor Green
if ($rejected) {
    # Reported, not hidden: a format missing from the matrix must be a KNOWN gap, not a
    # silently absent row that reads as "nothing to see here".
    Write-Host "[speed-corpus] rejected (kept out of the matrix on purpose):" -ForegroundColor Yellow
    $rejected | ForEach-Object { "    $_" }
}
$total = (Get-ChildItem $OutDir -File | Where-Object { -not $_.Name.StartsWith('_') }).Count
$mb = [int](((Get-ChildItem $OutDir -File | Measure-Object Length -Sum).Sum) / 1MB)
Write-Host ("[speed-corpus] {0} samples, {1} MB in {2}" -f $total, $mb, $OutDir) -ForegroundColor Green
