# Builds a test corpus: one sample file per supported format, so thumbnail
# rendering can be regression-checked (see regression.ps1) without hunting for
# files. Most formats are generated from a distinctive base image via the FULL
# ImageMagick; containers/project files are built synthetically or downloaded.
#
#   pwsh scripts\build-corpus.ps1                 # build into ..\test-corpus
#   pwsh scripts\build-corpus.ps1 -SkipDownloads  # generated/synthetic only
param(
    [string]$OutDir = "$PSScriptRoot\..\..\test-corpus",
    [switch]$SkipDownloads
)
$ErrorActionPreference = 'Continue'
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.IO.Compression.FileSystem

$magick = (Get-ChildItem 'C:\Program Files\ImageMagick*\magick.exe' -EA SilentlyContinue | Select-Object -First 1).FullName
if (-not $magick) { $magick = (Get-Command magick -EA SilentlyContinue).Source }
if (-not $magick) { throw "Full ImageMagick not found (needed to generate samples)." }
$st2k = @("D:\st2k-target\release\st2k.exe", "$PSScriptRoot\..\target\release\st2k.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
New-Item -ItemType Directory -Force $OutDir | Out-Null

# --- 1) A distinctive base image: corner color blocks (catch flips/mirrors),
#        an up-arrow (catch vertical flip), and a label (catch garbage). --------
$base = "$OutDir\_base.png"
$b = New-Object System.Drawing.Bitmap 512, 384
$g = [System.Drawing.Graphics]::FromImage($b)
$g.Clear([System.Drawing.Color]::FromArgb(245, 245, 250))
$g.FillRectangle([System.Drawing.Brushes]::Red, 0, 0, 80, 80)
$g.FillRectangle([System.Drawing.Brushes]::LimeGreen, 432, 0, 80, 80)
$g.FillRectangle([System.Drawing.Brushes]::Blue, 0, 304, 80, 80)
$g.FillRectangle([System.Drawing.Brushes]::Magenta, 432, 304, 80, 80)
$g.FillPolygon([System.Drawing.Brushes]::Black, @(
        (New-Object System.Drawing.Point(256, 110)),
        (New-Object System.Drawing.Point(316, 200)),
        (New-Object System.Drawing.Point(196, 200))))
$g.DrawString('SAGETHUMBS 2K', (New-Object System.Drawing.Font('Arial', 28.0, [System.Drawing.FontStyle]::Bold)), [System.Drawing.Brushes]::DarkSlateBlue, 90.0, 230.0)
$g.Dispose(); $b.Save($base, [System.Drawing.Imaging.ImageFormat]::Png); $b.Dispose()

# --- 2) Supported extensions, straight from the binary (stays in sync) --------
$exts = @()
if (Test-Path $st2k) {
    $exts = (& $st2k formats) | ForEach-Object { if ($_ -match '^\s*\.(\S+)\s') { $matches[1] } } | Where-Object { $_ }
}
if (-not $exts) { Write-Host "  (st2k not built — generating a default format set)"; $exts = @('png','jpg','gif','bmp','tiff','webp','ico','tga','qoi','heic','avif','jxl','pnm','pbm','pgm','ppm','pcx','dds','hdr','exr','svg','jp2','psd') }

# Formats handled specially below (not a plain `magick base.png out.ext`).
# eps is special: magick writes PLAIN EPS (readable only with Ghostscript); we
# synthesize the DOS-EPS-with-TIFF-preview flavor container/eps.rs extracts.
$special = 'epub','mobi','azw','azw3','fb2','fbz','cbz','cb7','cbr','cbt','kra','ora','3mf','fcstd','gcode','gco','clip','afphoto','afdesign','afpub','af','blend','psd','psb','djvu','djv','pdf','eps'

# --- 3) Generate every magick-writable supported format from the base ---------
$gen = 0; $fail = @()
foreach ($e in ($exts | Where-Object { $special -notcontains $_ } | Sort-Object -Unique)) {
    $out = "$OutDir\sample.$e"
    & $magick $base $out 2>$null
    if ((Test-Path $out) -and (Get-Item $out).Length -gt 0) { $gen++ } else { $fail += $e }
}
Write-Host "[corpus] magick-generated $gen formats; magick can't write: $($fail -join ' ')"

# --- 4) Synthetic containers (zip/text with the preview where we extract it) --
function New-Zip($path, $entries) {
    if (Test-Path $path) { Remove-Item $path -Force }
    $z = [System.IO.Compression.ZipFile]::Open($path, 'Create')
    foreach ($n in $entries.Keys) {
        $en = $z.CreateEntry($n); $w = $en.Open()
        $bytes = if ($entries[$n] -is [byte[]]) { $entries[$n] } else { [System.Text.Encoding]::UTF8.GetBytes($entries[$n]) }
        $w.Write($bytes, 0, $bytes.Length); $w.Close()
    }
    $z.Dispose()
}
$png = [System.IO.File]::ReadAllBytes($base)
New-Zip "$OutDir\sample.cbz" @{ '001.png' = $png; '002.png' = $png }
New-Zip "$OutDir\sample.kra" @{ 'mimetype' = 'application/x-krita'; 'mergedimage.png' = $png }
New-Zip "$OutDir\sample.ora" @{ 'mimetype' = 'image/openraster'; 'Thumbnails/thumbnail.png' = $png }
New-Zip "$OutDir\sample.3mf" @{ '3D/3dmodel.model' = '<model/>'; 'Metadata/thumbnail.png' = $png }
New-Zip "$OutDir\sample.fcstd" @{ 'Document.xml' = '<doc/>'; 'thumbnails/Thumbnail.png' = $png }
New-Zip "$OutDir\sample.epub" @{ 'mimetype' = 'application/epub+zip'; 'META-INF/container.xml' = '<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>'; 'content.opf' = '<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0"><manifest><item id="c" href="cover.png" media-type="image/png" properties="cover-image"/></manifest></package>'; 'cover.png' = $png }
# G-code: a slicer-style base64 PNG thumbnail block
$b64 = [Convert]::ToBase64String($png)
$gc = "; generated by test`n; thumbnail begin 512x384 $($png.Length)`n"
foreach ($chunk in ($b64 -split '(.{1,78})' | Where-Object { $_ })) { $gc += "; $chunk`n" }
$gc += "; thumbnail end`nG28`n"
[System.IO.File]::WriteAllText("$OutDir\sample.gcode", $gc)

# DOS-EPS: the 30-byte binary header (PS + TIFF-preview offsets) around a
# magick-written TIFF — the flavor container/eps.rs extracts without Ghostscript.
& $magick $base -resize 256x192 "$OutDir\_eps_preview.tif" 2>$null
if (Test-Path "$OutDir\_eps_preview.tif") {
    $tif = [System.IO.File]::ReadAllBytes("$OutDir\_eps_preview.tif")
    $ps = [System.Text.Encoding]::ASCII.GetBytes("%!PS-Adobe-3.0 EPSF-3.0`n%%BoundingBox: 0 0 512 384`nshowpage`n")
    $ms = New-Object System.IO.MemoryStream
    $w = New-Object System.IO.BinaryWriter $ms
    $w.Write([byte[]](0xC5, 0xD0, 0xD3, 0xC6))
    $w.Write([uint32]30); $w.Write([uint32]$ps.Length)                   # PS offset/len
    $w.Write([uint32]0); $w.Write([uint32]0)                             # WMF (none)
    $w.Write([uint32](30 + $ps.Length)); $w.Write([uint32]$tif.Length)   # TIFF offset/len
    $w.Write([uint16]0xFFFF)                                             # checksum unused
    $w.Write($ps); $w.Write($tif); $w.Flush()
    [System.IO.File]::WriteAllBytes("$OutDir\sample.eps", $ms.ToArray())
    Remove-Item "$OutDir\_eps_preview.tif" -Force -EA SilentlyContinue
}

# PDF + SVG via magick / text
& $magick $base "$OutDir\sample.pdf" 2>$null
[System.IO.File]::WriteAllText("$OutDir\sample.svg", '<svg xmlns="http://www.w3.org/2000/svg" width="240" height="180"><rect width="240" height="180" fill="#eef"/><circle cx="120" cy="90" r="60" fill="teal"/><text x="40" y="95" font-size="20">SVG</text></svg>')

# --- 5) Real-world downloads for formats we can't synthesize -------------------
if (-not $SkipDownloads) {
    $dls = @{
        'sample.psd'      = 'https://raw.githubusercontent.com/Agamnentzar/psd-thumbnail-provider/master/Test/test.psd'
        'sample.psb'      = 'https://raw.githubusercontent.com/Agamnentzar/psd-thumbnail-provider/master/Test/test7.psb'
        'sample.afdesign' = 'https://raw.githubusercontent.com/NickBeeuwsaert/AFDesignLoad/master/testDesigns/raster_test.afdesign'
        'sample.blend'    = 'https://raw.githubusercontent.com/mewspring/blend/master/testdata/block.blend'
        'sample.clip'     = 'https://raw.githubusercontent.com/dobrokot/clip_to_psd/master/tests/test_export_all_features.clip'
        # Camera RAW (decode-only — magick can't write it): real small samples.
        'sample.dng'      = 'https://raw.githubusercontent.com/rawpy/rawpy/v0.18.1/tests/iss115.DNG'
    }
    foreach ($n in $dls.Keys) {
        if (Test-Path "$OutDir\$n") { continue }
        try { Invoke-WebRequest $dls[$n] -OutFile "$OutDir\$n" -UseBasicParsing -TimeoutSec 60 } catch { Write-Host "  download failed: $n" }
    }
}

$count = (Get-ChildItem $OutDir -File | Where-Object { $_.Name -notlike '_*' }).Count
Write-Host "[corpus] $count sample files in $OutDir" -ForegroundColor Green
