<#
  make-decoy-fixtures.ps1 — build corpus samples where the RIGHT answer and a plausible
  WRONG answer are different colours, so "it rendered" and "it rendered correctly" stop
  being the same question.

  WHY. Every gate this project had asks whether something HAPPENED, never whether it was
  RIGHT, and the corpus is one small sample per format chosen to prove the format parses.
  That combination shipped the 2.0.0 XCF bug: large layered GIMP files rendered a flawless
  thumbnail of the WRONG layer, and nothing anywhere could tell. The generated .xcf fixtures
  fixed it for one format. Every other multi-part format has the identical exposure, because
  a multi-page/multi-frame/multi-size file is a CHOICE, and a choice can be made wrongly
  while still producing a perfectly good picture.

  THE TRICK. Each file's first page / first frame / largest icon is BLUE, and every decoy
  behind it is RED. The correct thumbnail is therefore blue, always, for every format here.
  Pick the wrong page, the wrong frame, the last frame instead of the first, or the 16px icon
  instead of the 256px one, and the centre pixel comes out red and the gate fails. Recorded in
  <corpus>\_expected-colors.txt and checked by regression.ps1 (see compare-renders.py).

  These are also LARGE by corpus standards (6000x4000 where the format allows), which is the
  other missing axis: every other sample is a few hundred KB, so no decoder's big-image path
  was ever exercised. They stay small ON DISK because a flat colour compresses to nothing,
  which is what lets them live in a corpus that renders on every run. Byte-size limits
  (MAX_INPUT_BYTES) need incompressible data and files too big to keep around; generate those
  ad hoc with make-xcf-fixture.py --noise when testing that axis.

      pwsh scripts\make-decoy-fixtures.ps1                 # into the default corpus
      pwsh scripts\make-decoy-fixtures.ps1 -OutDir D:\tmp  # somewhere else

  DO NOT change the colours without regenerating _expected-colors.txt: the manifest is the
  half that makes them a test rather than four more files nobody looks at.
#>
param(
    [string]$OutDir = "$PSScriptRoot\..\..\test-corpus",
    [string]$MagickPath
)
$ErrorActionPreference = 'Stop'

if (-not $MagickPath) {
    $MagickPath = (Get-ChildItem 'C:\Program Files\ImageMagick*\magick.exe' -EA SilentlyContinue |
        Select-Object -First 1).FullName
}
if (-not $MagickPath -or -not (Test-Path $MagickPath)) {
    Write-Host '[decoy] ImageMagick not found - SKIPPED' -ForegroundColor Yellow
    exit 2
}
$magick = $MagickPath
New-Item -ItemType Directory -Force $OutDir | Out-Null

# The one blue every fixture must thumbnail to, and the red every decoy is painted in.
#
# HEX, not rgb(...): magick's CLI reads a bare parenthesis as its own image-stack operator, so
# `xc:rgb(30,60,210)` parses as an unterminated group and it then treats the OUTPUT PATH as a
# colour name. The failure is loud but the message points at the wrong argument entirely.
#
# And the names are deliberately not `$RIGHT`/`$right`: PowerShell variables are CASE
# INSENSITIVE, so a path named `$right` silently overwrites a colour named `$RIGHT`, and the
# only symptom is magick reporting the PNG path as an unrecognized colour.
$OkColour    = '#1E3CD2'   # rgb(30,60,210)
$DecoyColour = '#DC2828'   # rgb(220,40,40)
$BigSize     = '6000x4000'
$tmp         = Join-Path ([System.IO.Path]::GetTempPath()) ("st2k-decoy-" + $PID)
New-Item -ItemType Directory -Force $tmp | Out-Null

# Two source pages, big enough that the decoders' large-image paths actually run.
$pageOk    = Join-Path $tmp 'page-ok.png'
$pageDecoy = Join-Path $tmp 'page-decoy.png'
& $magick -size $BigSize "xc:$OkColour" $pageOk
& $magick -size $BigSize "xc:$DecoyColour" $pageDecoy

$made = @()
function Emit($name, [scriptblock]$body) {
    $path = Join-Path $OutDir $name
    try {
        & $body $path
        if ((Test-Path $path) -and (Get-Item $path).Length -gt 0) {
            $script:made += $name
            Write-Host ("  {0,-42} {1,8:N0} bytes" -f $name, (Get-Item $path).Length)
        } else {
            Write-Host ("  {0,-42} NOT WRITTEN (magick declined)" -f $name) -ForegroundColor Yellow
        }
    } catch {
        Write-Host ("  {0,-42} FAILED: {1}" -f $name, $_.Exception.Message) -ForegroundColor Yellow
    }
}

Write-Host '[decoy] first page/frame/size is BLUE, every decoy behind it is RED' -ForegroundColor Cyan

# Multi-PAGE. The thumbnail is page one; page two exists only to be wrong.
Emit 'sample-decoy-multipage.pdf'  { param($p) & $magick $pageOk $pageDecoy $p }
Emit 'sample-decoy-multipage.tif'  { param($p) & $magick $pageOk $pageDecoy $p }

# Multi-FRAME. The thumbnail is the first frame. A decoder that grabs "a" frame, or the last
# one, or composites the animation, lands on red.
Emit 'sample-decoy-frames.gif'     { param($p) & $magick -delay 20 $pageOk $pageDecoy $pageDecoy -loop 0 $p }
Emit 'sample-decoy-frames.webp'    { param($p) & $magick -delay 20 $pageOk $pageDecoy $pageDecoy -loop 0 $p }

# Multi-SIZE. An .ico holds several images; the biggest is the one worth showing, and the
# small ones are the tempting wrong answer.
Emit 'sample-decoy-sizes.ico'      { param($p)
    $b = Join-Path $tmp 'ico256.png'; $s = Join-Path $tmp 'ico16.png'
    & $magick -size 256x256 "xc:$OkColour" $b
    & $magick -size 16x16   "xc:$DecoyColour" $s
    & $magick $b $s $p
}

# PSD gets a big SINGLE-layer file and no decoy, deliberately.
#
# A decoy only tests something where the decoder makes a CHOICE. XCF needed one because it
# composites the layers itself. Our PSD path reads the baked composite the format already
# carries, so there is no layer to pick wrongly, and magick cannot author a PSD whose composite
# differs from its first layer anyway: `magick red.png blue.png out.psd` produces a file whose
# page 0 magick itself reads back as RED, so a "decoy" built that way would only ever assert
# that we agree with magick about a file neither of us composited.
Emit 'sample-big-canvas.psd'       { param($p) & $magick $pageOk -alpha off $p }

# Single big images, for the SIZE axis alone: 24 megapixels each, where every other corpus
# sample is under one.
Emit 'sample-big-canvas.png'       { param($p) & $magick -size $BigSize "xc:$OkColour" $p }
Emit 'sample-big-canvas.jpg'       { param($p) & $magick -size $BigSize "xc:$OkColour" -quality 92 $p }
Emit 'sample-big-canvas.bmp'       { param($p) & $magick -size $BigSize "xc:$OkColour" $p }

# --- Does each decoy fixture actually CONTAIN a wrong answer? -----------------
# A multi-page fixture that magick quietly wrote as one page is a test that cannot fail: it
# would pass forever while asserting nothing, which is the same trap as a fuzz seed its own
# parser rejects. So verify the shape rather than trusting the write: at least two pages, and
# the LAST one really is the decoy colour. Anything that fails this is reported and DELETED,
# because a fixture that cannot fail is worse than no fixture.
function Test-IsDecoyColour([string]$pixel) {
    # Tolerant by 6 per channel: a LOSSY container (webp, jpeg) shifts a flat fill slightly,
    # and the first run of this check rejected a perfectly good animated WebP over
    # srgb(220,41,40). Exactness here would only ever delete good fixtures.
    if ("$pixel" -notmatch '(\d+),\s*(\d+),\s*(\d+)') { return $false }
    $rgb = @([int]$Matches[1], [int]$Matches[2], [int]$Matches[3])
    $want = @(220, 40, 40)
    for ($i = 0; $i -lt 3; $i++) { if ([Math]::Abs($rgb[$i] - $want[$i]) -gt 6) { return $false } }
    return $true
}

$decoyNames = @($made | Where-Object { $_ -like 'sample-decoy-*' })
foreach ($name in $decoyNames) {
    $path = Join-Path $OutDir $name

    # PDF is counted from its own bytes, not with `magick identify`: enumerating PDF pages
    # needs Ghostscript, this project deliberately does not ship or require it, and without
    # it identify reports ZERO pages, which the naive check read as "no decoy" and deleted a
    # perfectly good fixture.
    if ($name -like '*.pdf') {
        $raw = [System.Text.Encoding]::Latin1.GetString([System.IO.File]::ReadAllBytes($path))
        $pageCount = ([regex]::Matches($raw, '/Type\s*/Page[^s]')).Count
        if ($pageCount -lt 2) {
            Write-Host ("  {0,-42} NO USABLE DECOY ({1} page object(s)) - DELETED" -f $name, $pageCount) -ForegroundColor Red
            Remove-Item $path -Force -EA SilentlyContinue
            $made = $made | Where-Object { $_ -ne $name }
        } else {
            Write-Host ("  {0,-42} ok: {1} page objects" -f $name, $pageCount) -ForegroundColor DarkGray
        }
        continue
    }

    $pages = @(& $magick identify $path 2>$null)
    $probe = Join-Path $tmp 'probe.png'
    Remove-Item $probe -Force -EA SilentlyContinue
    & $magick "$path[$($pages.Count - 1)]" -resize 8x8! $probe 2>$null
    $pixel = if (Test-Path $probe) { & $magick $probe -format '%[pixel:p{4,4}]' info: 2>$null } else { '' }
    if ($pages.Count -lt 2 -or -not (Test-IsDecoyColour $pixel)) {
        Write-Host ("  {0,-42} NO USABLE DECOY ({1} page(s), last='{2}') - DELETED" -f $name, $pages.Count, $pixel) -ForegroundColor Red
        Remove-Item $path -Force -EA SilentlyContinue
        $made = $made | Where-Object { $_ -ne $name }
    } else {
        Write-Host ("  {0,-42} ok: {1} pages, last one is the decoy" -f $name, $pages.Count) -ForegroundColor DarkGray
    }
}

Remove-Item $tmp -Recurse -Force -EA SilentlyContinue
Write-Host ("[decoy] {0} fixtures written to {1}" -f $made.Count, $OutDir) -ForegroundColor Green
Write-Host '[decoy] all of them must thumbnail to 30,60,210 - record that in _expected-colors.txt' -ForegroundColor DarkGray
