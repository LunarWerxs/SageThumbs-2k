# Builds a test corpus: one sample file per supported format, so thumbnail
# rendering can be regression-checked (see regression.ps1) without hunting for
# files. Most formats are generated from a distinctive base image via the FULL
# ImageMagick; containers/project files are built synthetically or downloaded.
#
# HONESTY RULE (2026-07-08): when magick can't ENCODE a target format it still
# writes the INPUT's bytes to the output name — a PNG renamed to .arw — with a
# non-fatal "no encode delegate" warning (read-only coders) or SILENTLY, exit 0
# (extensions magick doesn't know, e.g. .icns). ~90 formats' samples used to be
# such fakes, so regression "passed" them by PNG-sniffing. Generation now
# magic-checks every output and deletes fakes; formats with no real sample are
# recorded in <corpus>\_no-real-sample.txt so regression reports them UNTESTED.
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
$st2k = Join-Path (& "$PSScriptRoot\_targetdir.ps1") 'release\st2k.exe'
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
$special = @(
    'epub', 'mobi', 'azw', 'azw3', 'fb2', 'fbz', 'cbz', 'cb7', 'cbr', 'cbt', 'kra', 'ora', '3mf', 'fcstd', 'gcode', 'gco', 'clip', 'afphoto', 'afdesign', 'afpub', 'af', 'blend', 'psd', 'psb', 'djvu', 'djv', 'pdf', 'eps', 'emf', 'emz', 'wmf', 'sketch', 'procreate', 'key', 'pages', 'numbers', 'cdr', 'skp', 'dwg', '3dm', 'xd', 'cdt', 'indd', 'vsdx', 'vsdm', 'max', 'vsd', 'pub',
    # magick can't WRITE these (it faked them as renamed PNGs before 2026-07-08);
    # real samples come from the synth/download/alias sections below:
    'dng', 'kdc',                                                # real RAW downloads (the fakes used to pre-empt them)
    'heic', 'heif', 'heics', 'heifs', 'hif', 'avci',             # HEIF family: real download + content-sniffed aliases
    'icns', 'dcm', 'xcf',                                        # synthesized icns; DICOM + GIMP downloads
    'jfif', 'mpo', 'bw',                                         # aliases of the real jpg/sgi samples
    'odt', 'ods', 'odp', 'odg', 'odf', 'ott', 'ots', 'otp',      # ODF: synthesized zip + Thumbnails/thumbnail.png
    'docx', 'docm', 'dotx', 'dotm', 'xlsx', 'xlsm', 'xlsb', 'xltx', 'xltm', 'pptx', 'pptm', 'ppsx', 'ppsm', 'potx', 'potm',  # OOXML: synthesized OPC zip + docProps thumbnail
    'mp3', 'flac', 'ogg', 'oga', 'opus', 'spx', 'm4a', 'm4b', 'ape', 'wv', 'wav', 'aiff', 'aif', 'aifc', 'aac', 'mpc', 'dsf' # audio: real minimal files + embedded covers
)

# --- 3) Generate every magick-writable supported format from the base ---------
# See the HONESTY RULE up top: magick "succeeds" on unwritable formats by writing
# the input PNG's bytes under the target name (warning for read-only coders,
# SILENT for unknown extensions — verified both). So: write to a temp name, treat
# a "no encode delegate" warning OR a PNG-signature output under a non-PNG
# extension as failure, and only then replace the target — a pre-existing real
# sample (e.g. a hand-added .wma) is never clobbered by a fake.
function Test-IsPng([string]$path) {
    $fs = [System.IO.File]::OpenRead($path)
    $b = New-Object byte[] 4; $n = $fs.Read($b, 0, 4); $fs.Close()
    ($n -eq 4) -and ($b[0] -eq 0x89) -and ($b[1] -eq 0x50) -and ($b[2] -eq 0x4E) -and ($b[3] -eq 0x47)
}

# Heal a corpus poisoned by the old behavior FIRST: any leftover sample.<ext>
# that is really a renamed PNG gets dropped, so the loop's "kept pre-existing"
# report is honest and the download section (which skips existing files) isn't
# pre-empted by a stale fake.
$purged = @()
Get-ChildItem $OutDir -File -Filter 'sample.*' | Where-Object { $_.Extension -notin '.png', '.apng' } | ForEach-Object {
    if (Test-IsPng $_.FullName) { Remove-Item $_.FullName -Force; $purged += $_.Extension.TrimStart('.') }
}
if ($purged.Count) { Write-Host "[corpus] purged $($purged.Count) stale renamed-PNG fakes: $($purged -join ' ')" }

$gen = 0; $fail = @(); $kept = @()
foreach ($e in ($exts | Where-Object { $special -notcontains $_ } | Sort-Object -Unique)) {
    $out = "$OutDir\sample.$e"
    $tmp = "$OutDir\_gen.$e"   # _-prefixed: ignored by the harness even if left behind
    Remove-Item $tmp -Force -EA SilentlyContinue
    $err = (& $magick $base $tmp 2>&1) -join ' '
    $fake = ($err -match 'no encode delegate') -or
            (($e -notin 'png', 'apng') -and (Test-Path $tmp) -and (Test-IsPng $tmp))
    if (-not $fake -and (Test-Path $tmp) -and (Get-Item $tmp).Length -gt 0) {
        Move-Item $tmp $out -Force; $gen++
    }
    else {
        Remove-Item $tmp -Force -EA SilentlyContinue
        if (Test-Path $out) { $kept += $e } else { $fail += $e }
    }
}
Write-Host "[corpus] magick-generated $gen formats; magick can't write (no fake emitted): $($fail -join ' ')"
if ($kept.Count) { Write-Host "[corpus] kept pre-existing real samples magick can't regenerate: $($kept -join ' ')" }

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
# More comic/ebook containers so the regression guards their distinct backends:
# CBT (tar, via the in-box tar.exe), CB7 (7-Zip if installed), FB2 (+ zipped FBZ).
$pngTmp = "$OutDir\001.png"; [System.IO.File]::WriteAllBytes($pngTmp, $png)
$pngTmp2 = "$OutDir\002.png"; [System.IO.File]::WriteAllBytes($pngTmp2, $png)
$tarExe = (Get-Command tar.exe -EA SilentlyContinue).Source
if ($tarExe) { & $tarExe -cf "$OutDir\sample.cbt" -C $OutDir 001.png 002.png 2>$null }
$7z = @('C:\Program Files\7-Zip\7z.exe', 'C:\Program Files (x86)\7-Zip\7z.exe') | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $7z) { $7z = (Get-Command 7z.exe -EA SilentlyContinue).Source }
if ($7z) { & $7z a -t7z "$OutDir\sample.cb7" "$pngTmp" "$pngTmp2" *>$null }
Remove-Item $pngTmp, $pngTmp2 -Force -EA SilentlyContinue
# FB2: a FictionBook with a coverpage referencing a base64 <binary> cover.
$fb2b64 = [Convert]::ToBase64String($png)
$fb2 = '<?xml version="1.0" encoding="utf-8"?><FictionBook xmlns:l="http://www.w3.org/1999/xlink"><description><title-info><coverpage><image l:href="#cover.png"/></coverpage></title-info></description><binary id="cover.png" content-type="image/png">' + $fb2b64 + '</binary></FictionBook>'
[System.IO.File]::WriteAllText("$OutDir\sample.fb2", $fb2)
New-Zip "$OutDir\sample.fbz" @{ 'book.fb2' = $fb2 }
New-Zip "$OutDir\sample.kra" @{ 'mimetype' = 'application/x-krita'; 'mergedimage.png' = $png }
New-Zip "$OutDir\sample.ora" @{ 'mimetype' = 'image/openraster'; 'Thumbnails/thumbnail.png' = $png }
New-Zip "$OutDir\sample.3mf" @{ '3D/3dmodel.model' = '<model/>'; 'Metadata/thumbnail.png' = $png }
New-Zip "$OutDir\sample.fcstd" @{ 'Document.xml' = '<doc/>'; 'thumbnails/Thumbnail.png' = $png }
# Autodesk Fusion 360 .f3d — a ZIP whose preview PNG is ZSTD-compressed (Fusion's real
# layout), so it exercises the pure-Rust `ruzstd` decode path. PowerShell's ZipFile can't
# WRITE zstd, so build it via Python (zipfile.ZIP_ZSTANDARD); if Python/zstd isn't
# available, fall back to a deflate ZIP at the same path (still renders via the normal read).
$f3dPng = "$OutDir\_f3d.png"; [System.IO.File]::WriteAllBytes($f3dPng, $png)
$f3dPy = @"
import zipfile
png = open(r'$f3dPng','rb').read()
with zipfile.ZipFile(r'$OutDir\sample.f3d','w') as z:
    z.writestr('Components/part.brep', b'\x00'*200)
    zi = zipfile.ZipInfo('FusionAssetName[Active]/Previews/small.png')
    zi.compress_type = zipfile.ZIP_ZSTANDARD
    z.writestr(zi, png)
"@
Remove-Item "$OutDir\sample.f3d" -EA SilentlyContinue
foreach ($py in 'python', 'python3') {
    $exe = Get-Command $py -EA SilentlyContinue
    if ($exe) { & $exe.Source -c $f3dPy 2>$null; if (Test-Path "$OutDir\sample.f3d") { break } }
}
if (-not (Test-Path "$OutDir\sample.f3d")) {
    New-Zip "$OutDir\sample.f3d" @{ 'Components/part.brep' = [byte[]](1, 2, 3); 'FusionAssetName[Active]/Previews/small.png' = $png }
}
Remove-Item $f3dPng -EA SilentlyContinue
New-Zip "$OutDir\sample.epub" @{ 'mimetype' = 'application/epub+zip'; 'META-INF/container.xml' = '<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>'; 'content.opf' = '<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0"><manifest><item id="c" href="cover.png" media-type="image/png" properties="cover-image"/></manifest></package>'; 'cover.png' = $png }
# Design-app project files (ZIP + embedded preview; same trick as kra/ora):
# Sketch, Procreate, and Apple iWork (Keynote/Pages/Numbers).
$jpgPrev = "$OutDir\_prev.jpg"; & $magick $base -resize 256x192 $jpgPrev 2>$null
# NOTE: assign directly, NOT `$jpg = if (...) {...}` — a statement-expression goes
# through the pipeline and unrolls byte[] to Object[], which New-Zip then writes
# as decimal TEXT ("255 216 …"), silently corrupting the iWork previews.
$jpg = $png
if (Test-Path $jpgPrev) { $jpg = [System.IO.File]::ReadAllBytes($jpgPrev) }
New-Zip "$OutDir\sample.sketch"    @{ 'document.json' = '{}'; 'previews/preview.png' = $png }
New-Zip "$OutDir\sample.procreate" @{ 'Document.archive' = [byte[]](1, 2, 3); 'QuickLook/Thumbnail.png' = $png }
New-Zip "$OutDir\sample.key"       @{ 'Index/Document.iwa' = [byte[]](1); 'preview.jpg' = $jpg; 'QuickLook/Thumbnail.jpg' = $jpg }
New-Zip "$OutDir\sample.pages"     @{ 'Index/Document.iwa' = [byte[]](1); 'preview.jpg' = $jpg }
New-Zip "$OutDir\sample.numbers"   @{ 'Index/Document.iwa' = [byte[]](1); 'preview.jpg' = $jpg }
# CorelDRAW X4+ (ZIP/OPC): preview at metadata/thumbnails/thumbnail.bmp.
$bmpPrev = "$OutDir\_prev.bmp"; & $magick $base -resize 256x192 "BMP3:$bmpPrev" 2>$null
if (Test-Path $bmpPrev) {
    $bmp = [System.IO.File]::ReadAllBytes($bmpPrev)
    # CorelDRAW drawing + template share the same package layout.
    New-Zip "$OutDir\sample.cdr" @{ 'content/riffData.cdr' = [byte[]](1, 2, 3); 'metadata/thumbnails/thumbnail.bmp' = $bmp }
    New-Zip "$OutDir\sample.cdt" @{ 'content/riffData.cdr' = [byte[]](1, 2, 3); 'metadata/thumbnails/thumbnail.bmp' = $bmp }
    Remove-Item $bmpPrev -Force -EA SilentlyContinue
}
# Adobe XD: ZIP keyed off the "sparkler" mimetype, with a top-level thumbnail.png.
New-Zip "$OutDir\sample.xd" @{ 'mimetype' = 'application/vnd.adobe.sparkler.project+dcxucf'; 'thumbnail.png' = $png }
if (Test-Path $jpgPrev) { Remove-Item $jpgPrev -Force -EA SilentlyContinue }
# Office documents (container/office.rs — magick faked all of these before):
# ODF detect = a `mimetype` entry containing "opendocument", preview at the
# spec-mandated Thumbnails/thumbnail.png; OOXML detect = [Content_Types].xml,
# preview = the docProps thumbnail part resolved via _rels/.rels. The variant
# extensions share the container byte-for-byte (decode content-sniffs), so the
# per-app/template/macro flavors are byte-copies of one donor each.
New-Zip "$OutDir\sample.odt" @{ 'mimetype' = 'application/vnd.oasis.opendocument.text'; 'Thumbnails/thumbnail.png' = $png; 'content.xml' = '<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"/>' }
foreach ($v in 'ods', 'odp', 'odg', 'odf', 'ott', 'ots', 'otp') { Copy-Item "$OutDir\sample.odt" "$OutDir\sample.$v" -Force }
$ooxmlCt = '<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="png" ContentType="image/png"/><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/></Types>'
$ooxmlRels = '<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/thumbnail" Target="docProps/thumbnail.png"/></Relationships>'
foreach ($o in 'pptx', 'docx', 'xlsx') {
    New-Zip "$OutDir\sample.$o" @{ '[Content_Types].xml' = $ooxmlCt; '_rels/.rels' = $ooxmlRels; 'docProps/thumbnail.png' = $png }
}
# Apple .icns — hand-assembled chunk list: "icns" magic + BE total length, then
# an ic08 member whose payload is a literal 256x256 PNG (exactly the layout
# container/icns.rs slices; macOS 10.7+ writes large sizes as PNG members).
$icnsPng = "$OutDir\_icns.png"; & $magick $base -resize '256x256!' $icnsPng 2>$null
if (Test-Path $icnsPng) {
    $ip = [System.IO.File]::ReadAllBytes($icnsPng)
    function BE32([uint32]$v) { $b = [BitConverter]::GetBytes($v); [Array]::Reverse($b); , $b }
    $icns = New-Object System.Collections.Generic.List[byte]
    $icns.AddRange([System.Text.Encoding]::ASCII.GetBytes('icns'))
    $icns.AddRange((BE32 ([uint32](16 + $ip.Length))))
    $icns.AddRange([System.Text.Encoding]::ASCII.GetBytes('ic08'))
    $icns.AddRange((BE32 ([uint32](8 + $ip.Length))))
    $icns.AddRange([byte[]]$ip)
    [System.IO.File]::WriteAllBytes("$OutDir\sample.icns", $icns.ToArray())
    Remove-Item $icnsPng -Force -EA SilentlyContinue
}
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
# Plain-text EPSI: an Adobe-standard, hex-encoded greyscale preview. This is
# intentionally synthesized because ImageMagick emits preview-less EPS here.
$epsiBitmap = New-Object System.Drawing.Bitmap $base
try {
    $epsiWidth = 128; $epsiHeight = 96
    $epsi = New-Object System.Drawing.Bitmap $epsiWidth, $epsiHeight
    $graphics = [System.Drawing.Graphics]::FromImage($epsi)
    $graphics.DrawImage($epsiBitmap, 0, 0, $epsiWidth, $epsiHeight); $graphics.Dispose()
    $epsiLineBytes = 32
    $epsiLines = [int][Math]::Ceiling(($epsiWidth * $epsiHeight) / $epsiLineBytes)
    $epsiText = "%!PS-Adobe-3.0 EPSF-3.0`n%%BoundingBox: 0 0 512 384`n%%BeginPreview: $epsiWidth $epsiHeight 8 $epsiLines`n"
    $epsiPacked = New-Object byte[] ($epsiWidth * $epsiHeight)
    for ($y = 0; $y -lt $epsiHeight; $y++) {
        for ($x = 0; $x -lt $epsiWidth; $x++) {
            # EPSI rows run bottom-up and its samples are 0=white, 255=black.
            $pixel = $epsi.GetPixel($x, $epsiHeight - 1 - $y)
            $grey = [int](0.299 * $pixel.R + 0.587 * $pixel.G + 0.114 * $pixel.B)
            $epsiPacked[$y * $epsiWidth + $x] = 255 - $grey
        }
    }
    for ($offset = 0; $offset -lt $epsiPacked.Length; $offset += $epsiLineBytes) {
        $count = [Math]::Min($epsiLineBytes, $epsiPacked.Length - $offset)
        $row = New-Object System.Text.StringBuilder
        [void]$row.Append('% ')
        for ($i = 0; $i -lt $count; $i++) { [void]$row.AppendFormat('{0:X2}', $epsiPacked[$offset + $i]) }
        $epsiText += $row.ToString() + "`n"
    }
    $epsiText += "%%EndPreview`nshowpage`n"
    [System.IO.File]::WriteAllText("$OutDir\sample-epsi.eps", $epsiText, [System.Text.Encoding]::ASCII)
    $epsi.Dispose()
} finally {
    $epsiBitmap.Dispose()
}

# ZIP entry names are UTF-8 in modern CBZ files; keep one tiny fixture for the
# archive parser's non-ASCII-name path (its image payload is the normal base PNG).
New-Zip "$OutDir\sample-unicode.cbz" @{ 'ページ-01.png' = $png }

# PDF + SVG via magick / text
& $magick $base "$OutDir\sample.pdf" 2>$null
[System.IO.File]::WriteAllText("$OutDir\sample.svg", '<svg xmlns="http://www.w3.org/2000/svg" width="240" height="180"><rect width="240" height="180" fill="#eef"/><circle cx="120" cy="90" r="60" fill="teal"/><text x="40" y="95" font-size="20">SVG</text></svg>')

# Metafiles (magick can't WRITE these): EMF via GDI+ Metafile, EMZ = gzip(EMF),
# WMF = EMF converted via GetWinMetaFileBits + an Aldus placeable header. Decode
# is via the bundled magick EMF coder (and, for .emz, decode.rs's gzip-unwrap).
$emfPath = "$OutDir\sample.emf"
$ref = New-Object System.Drawing.Bitmap 1, 1
$gd = [System.Drawing.Graphics]::FromImage($ref); $hdc = $gd.GetHdc()
$mfr = New-Object System.Drawing.Rectangle 0, 0, 512, 384
$mf = New-Object System.Drawing.Imaging.Metafile($emfPath, $hdc, $mfr, ([System.Drawing.Imaging.MetafileFrameUnit]::Pixel))
$gd.ReleaseHdc($hdc); $gd.Dispose()
$mg = [System.Drawing.Graphics]::FromImage($mf)
$mg.Clear([System.Drawing.Color]::FromArgb(245, 245, 250))
$mg.FillRectangle([System.Drawing.Brushes]::Red, 0, 0, 80, 80)
$mg.FillRectangle([System.Drawing.Brushes]::LimeGreen, 432, 0, 80, 80)
$mg.FillRectangle([System.Drawing.Brushes]::Blue, 0, 304, 80, 80)
$mg.FillRectangle([System.Drawing.Brushes]::Magenta, 432, 304, 80, 80)
$mg.FillPolygon([System.Drawing.Brushes]::Black, @(
        (New-Object System.Drawing.Point(256, 110)),
        (New-Object System.Drawing.Point(316, 200)),
        (New-Object System.Drawing.Point(196, 200))))
$mg.DrawString('METAFILE', (New-Object System.Drawing.Font('Arial', 28.0, [System.Drawing.FontStyle]::Bold)), [System.Drawing.Brushes]::DarkSlateBlue, 110.0, 230.0)
$mg.Dispose(); $mf.Dispose()
if (Test-Path $emfPath) {
    $emf = [System.IO.File]::ReadAllBytes($emfPath)
    $fz = [System.IO.File]::Create("$OutDir\sample.emz")
    $gz = New-Object System.IO.Compression.GzipStream($fz, [System.IO.Compression.CompressionMode]::Compress)
    $gz.Write($emf, 0, $emf.Length); $gz.Close(); $fz.Close()
    Add-Type @"
using System; using System.Runtime.InteropServices;
public static class WmfConv {
    [DllImport("gdi32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr GetEnhMetaFile(string p);
    [DllImport("gdi32.dll")] public static extern uint GetWinMetaFileBits(IntPtr h, uint cb, byte[] d, int map, IntPtr hdc);
    [DllImport("gdi32.dll")] public static extern bool DeleteEnhMetaFile(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetDC(IntPtr h);
    [DllImport("user32.dll")] public static extern int ReleaseDC(IntPtr w, IntPtr dc);
}
"@ -ErrorAction SilentlyContinue
    $hemf = [WmfConv]::GetEnhMetaFile($emfPath)
    if ($hemf -ne [IntPtr]::Zero) {
        $dc = [WmfConv]::GetDC([IntPtr]::Zero)
        $sz = [WmfConv]::GetWinMetaFileBits($hemf, 0, $null, 8, $dc)  # 8 = MM_ANISOTROPIC
        if ($sz -gt 0) {
            $buf = New-Object byte[] $sz
            [void][WmfConv]::GetWinMetaFileBits($hemf, $sz, $buf, 8, $dc)
            $ms = New-Object System.IO.MemoryStream; $w = New-Object System.IO.BinaryWriter $ms
            $words = @(0xCDD7, 0x9AC6, 0x0000, 0x0000, 0x0000, 0x0200, 0x0180, 0x0060, 0x0000, 0x0000)
            $w.Write([uint16]0xCDD7); $w.Write([uint16]0x9AC6)        # key 0x9AC6CDD7 (LE)
            $w.Write([uint16]0)                                       # handle
            $w.Write([int16]0); $w.Write([int16]0); $w.Write([int16]512); $w.Write([int16]384)
            $w.Write([uint16]96); $w.Write([uint32]0)                 # inch, reserved
            $chk = 0; foreach ($x in $words) { $chk = $chk -bxor $x }
            $w.Write([uint16]($chk -band 0xFFFF)); $w.Write($buf); $w.Flush()
            [System.IO.File]::WriteAllBytes("$OutDir\sample.wmf", $ms.ToArray())
        }
        [void][WmfConv]::ReleaseDC([IntPtr]::Zero, $dc); [void][WmfConv]::DeleteEnhMetaFile($hemf)
    }
}

# --- 5) Real-world downloads for formats we can't synthesize -------------------
if (-not $SkipDownloads) {
    $dls = @{
        'sample.psd'      = 'https://raw.githubusercontent.com/Agamnentzar/psd-thumbnail-provider/master/Test/test.psd'
        'sample.psb'      = 'https://raw.githubusercontent.com/Agamnentzar/psd-thumbnail-provider/master/Test/test7.psb'
        'sample.afdesign' = 'https://raw.githubusercontent.com/NickBeeuwsaert/AFDesignLoad/master/testDesigns/raster_test.afdesign'
        'sample.blend'    = 'https://raw.githubusercontent.com/mewspring/blend/master/testdata/block.blend'
        'sample.clip'     = 'https://raw.githubusercontent.com/dobrokot/clip_to_psd/master/tests/test_export_all_features.clip'
        # Paint.NET. Two on purpose: the modern one is a normal multi-layer 4.21 save,
        # and the -pdn35 one was written by Paint.NET 3.510, which proves the embedded
        # preview is not a modern-only feature. Both must thumbnail.
        'sample.pdn'      = 'https://raw.githubusercontent.com/addisonElliott/pypdn/master/tests/data/Untitled2.pdn'
        'sample-pdn35.pdn' = 'https://raw.githubusercontent.com/addisonElliott/pypdn/master/tests/data/oldPDN3510.pdn'
        # Camera RAW (decode-only — magick can't write it): real small samples.
        # (The old rawpy iss115.DNG URL was ALWAYS 404 — never noticed because a
        # renamed-PNG fake pre-empted the download until 2026-07-08.)
        'sample.dng'      = 'https://raw.githubusercontent.com/Exiv2/exiv2/main/test/data/IMG_1361.dng'
        'sample.kdc'      = 'https://raw.githubusercontent.com/letmaik/rawpy/main/test/RAW_KODAK_DC50_%C3%A9.KDC'
        # Kindle/Mobipocket ebook with an embedded cover (container/mobi.rs).
        'sample.mobi'     = 'https://raw.githubusercontent.com/bfabiszewski/libmobi/public/tests/samples/sample-cp1252.mobi'
        # Comic-book RAR with images (container/rar.rs, pure-Rust `rars` — renders in
        # the default build now, no feature gate).
        'sample.cbr'      = 'https://raw.githubusercontent.com/ssokolow/rar-test-files/master/build/testfile.rar3.cbr'
        # SketchUp: a real GUI-saved model (carries the embedded 256px thumbnail PNG
        # we carve in container/skp.rs). Minimal/programmatic .skp have no thumbnail.
        'sample.skp'      = 'https://raw.githubusercontent.com/SketchUp/testup-2/main/tests/SketchUp%20Ruby%20API/TC_Sketchup_DefinitionList/import_files/circle.skp'
        # AutoCAD 2000 (real save, DIB preview -> container/dwg.rs wraps it to BMP).
        'sample.dwg'      = 'https://raw.githubusercontent.com/LibreDWG/libredwg/master/test/test-data/example_2000.dwg'
        # Rhino 7 (real save, zlib-deflated DIB preview -> container/rhino.rs).
        'sample.3dm'      = 'https://github.com/ladybug-tools/lbt-grasshopper-samples/raw/master/samples/honeybee-energy/Rhino/shoe_box.3dm'
        # Visio (real save, docProps/thumbnail.emf -> project.rs + magick EMF tier).
        'sample.vsdx'     = 'https://github.com/Structural-Mechanics-CEG/mechanics-figures-source/raw/0acf216e7915cadc2b396bef5037533fef98790a/shear_3/Tekening1.vsdx'
        # InDesign (real save, base64 JPEG in XMP -> container/indd.rs). Git-LFS: fetch via media. host.
        'sample.indd'     = 'https://media.githubusercontent.com/media/caesuric/familiar-quest/39d89aa7a5f98ec3e86d904f9bd483d7f5068931/Art/Unit%20Frame%20Circle.indd'
        # 3ds Max (real save, OLE SummaryInformation thumbnail -> container/max.rs+ole.rs). Git-LFS.
        'sample.max'      = 'https://media.githubusercontent.com/media/wuye9036/SalviaRenderer/9eefbd4d036f2ff7bf7c03ae5b620865af964d6d/res/Logo3D.max'
        # Visio legacy binary (real save, OLE thumbnail = CF_ENHMETAFILE/EMF under the 0xFFFFFFFF sentinel). Git-LFS.
        'sample.vsd'      = 'https://media.githubusercontent.com/media/microchip-ung/mesa/25e97aadd4a1f27190ee08a6c942042ec0673135/mesa/docs/l3/l3.vsd'
        # Publisher (real save, OLE thumbnail = CF_METAFILEPICT/WMF). An empty doc, but a valid preview.
        'sample.pub'      = 'https://archive.org/download/NouveauMicrosoftPublisherDocument/Nouveau%20Microsoft%20Publisher%20Document.pub'
        # HEIC (magick can't write it here): libheif's own example image -> WIC/magick read tiers.
        'sample.heic'     = 'https://raw.githubusercontent.com/strukturag/libheif/master/examples/example.heic'
        # libheif's pinned auxiliary-alpha fixtures. The HEIC guards our ImageMagick-first
        # route around WIC's flattened HEVC alpha item; the compact `mini` AVIF guards our
        # explicit ImageMagick coder hint.
        # SHA-256: DAC399D3BF1019BAAF5F88EEF8B277087D0643E735DB947C42355237BB9D0221
        'sample-heic-alpha.heic' = 'https://raw.githubusercontent.com/strukturag/libheif/1a3583bcce77de6d3f8701c0758e3954863681ba/tests/data/with-alpha-512x512.heic'
        # SHA-256: 6D78AF07FBAD358F4240820331074FFF215AE8559BF4756EC480C6A6BE2A68D9
        'sample-avif-alpha.avif' = 'https://raw.githubusercontent.com/strukturag/libheif/1a3583bcce77de6d3f8701c0758e3954863681ba/tests/data/simple_osm_tile_alpha.avif'
        # DICOM (read-only in magick): pydicom's small CT test file -> magick read tier.
        'sample.dcm'      = 'https://raw.githubusercontent.com/pydicom/pydicom/main/src/pydicom/data/test_files/CT_small.dcm'
        # Flash video. THREE codecs live in .flv and they take three DIFFERENT paths, so one
        # sample cannot represent the extension:
        #   * sample.flv (synthesised below) is SORENSON SPARK  -> spawned st2k child (h263-rs)
        #   * sample-vp6.flv, real VP6 from FFmpeg's FATE suite -> spawned st2k child (nihav)
        #   * sample-h264.flv                                   -> IN-PROCESS mini-MP4 remux
        #                                                          + Media Foundation
        # Without the VP6 file the VP6 half rested entirely on manual checking.
        # SHA-256: F61D4A1696000CBB6D1E6A8BD7E4682656DA3AD017C49FD6D7C47A7F28D8AEFE
        'sample-vp6.flv'  = 'https://fate-suite.ffmpeg.org/flash-vp6/clip1024.flv'
        # H.264-in-FLV is the codec behind the ORIGINAL report — Windows cannot open an FLV at
        # all, so every one of them was blank regardless of what was inside. This note used to
        # say the path needed no file here because a unit test re-wraps sample.mp4's avcC and
        # keyframe into a synthetic FLV. That test proves the MUXER; it cannot prove we read a
        # tag layout a real Flash encoder emitted, which is the half that faces users. 36 KB.
        # SHA-256: 395D606D171A0088BDEDA14929F8A3686ED4CD29477A13EAE1248D4889EC2FEE
        'sample-h264.flv' = 'https://fate-suite.ffmpeg.org/flv/streamloop.flv'
        # VP9 Profile 2 (10-bit 4:2:0) + Profile 3 (12-bit 4:4:4) WebM, from FFmpeg's own
        # FATE conformance vectors. Media Foundation cannot decode these AT ALL (verified
        # with the Store VP9 extension installed and a capable GPU present), so they
        # exercise the out-of-process `st2k vp9-frame` tier (pure-Rust vp9dec) — the last
        # open codec of issue #26. A plain .webm sample already exists (generated below);
        # these two are extra for the same reason .flv carries two: one extension, several
        # codepaths.
        # SHA-256: C4B56B148D5039AA824FDE3D4877DBD2604D0DE7F77AF96F4BA1ADE537396A38
        'sample-vp9p2.webm' = 'https://fate-suite.ffmpeg.org/vp9-test-vectors/vp92-2-20-10bit-yuv420.webm'
        # SHA-256: E758190A9A4A75E5F35C370FC6C362C56B66AAAFE9FBC981747B5CC59C68B903
        'sample-vp9p3.webm' = 'https://fate-suite.ffmpeg.org/vp9-test-vectors/vp93-2-20-12bit-yuv444.webm'
        # Android package (container/apk.rs): a REAL apk, because the whole point of that
        # extractor is resolving the launcher icon the manifest names through the compiled
        # resource table, and a synthesised one only proves the parser reads what we wrote.
        # Pinned to a commit: androguard's test data is stable but `master` is not a promise.
        'sample.apk'      = 'https://raw.githubusercontent.com/androguard/androguard/0c0af30ca6bd55d3d34aa10d7f32593cd091a483/tests/data/APK/TestActivity.apk'
        # GIMP XCF (read-only in magick): GIMP's own test file -> magick read tier.
        'sample.xcf'      = 'https://gitlab.gnome.org/GNOME/gimp/-/raw/master/app/tests/files/gimp-2-6-file.xcf'
    }
    foreach ($n in $dls.Keys) {
        if (Test-Path "$OutDir\$n") { continue }
        # curl UA: GitLab (the GIMP xcf) 406es PowerShell's default User-Agent.
        try { Invoke-WebRequest $dls[$n] -OutFile "$OutDir\$n" -UseBasicParsing -TimeoutSec 60 -UserAgent 'curl/8.4.0' } catch { Write-Host "  download failed: $n" }
    }
}

# Android split-bundle wrappers (.xapk / .apks / .apkm) are zips CONTAINING an apk, so they
# are built from the real one rather than downloaded: the wrapper layer is ours to exercise,
# and hunting three separate hosted bundles would add three more things that can 404.
if (Test-Path "$OutDir\sample.apk") {
    # FIRST: swap the stock Android robot for labelled, colour-coded icons. The upstream
    # sample ships the default robot, which is exactly what a FAILED apk thumbnail also
    # looks like, so pass and fail were indistinguishable by eye (it fooled a human reader
    # once). Must run before the wrappers are built, or they would carry the old icons.
    & (Join-Path $PSScriptRoot 'make-apk-icons-distinctive.ps1') -Apk "$OutDir\sample.apk"

    $apkBytes = [System.IO.File]::ReadAllBytes("$OutDir\sample.apk")
    # base.apk is the entry name real bundles use, and the one apk.rs prefers.
    New-Zip "$OutDir\sample.xapk" @{ 'base.apk' = $apkBytes }
    New-Zip "$OutDir\sample.apks" @{ 'base.apk' = $apkBytes }
    New-Zip "$OutDir\sample.apkm" @{ 'base.apk' = $apkBytes }
}

# Visio .vsdm is structurally identical to .vsdx — reuse the downloaded sample.
if (Test-Path "$OutDir\sample.vsdx") { Copy-Item "$OutDir\sample.vsdx" "$OutDir\sample.vsdm" -Force }

# --- 5b) MODERN DDS (BC4/BC5/BC6H/BC7 + DX10 headers) ------------------------
# `magick` only writes DXT1/3/5, so the plain `magick base.png sample.dds` above
# covers just the 1998 half of the format. The formats that actually ship in games
# today — BC7 for colour, BC6H for HDR, BC4/BC5 for masks and normal maps — need a
# real block compressor, so we fetch Microsoft's own `texconv` from the DirectXTex
# releases (signed by Microsoft; verified before it is run) and generate them.
# Without these the whole `decode/dds.rs` tier is untested by the corpus, which is
# how BC7 shipped decodable only on a Full install and BC6H not at all until
# 2026-08-03. Best-effort: skipped with a note if the download or signature fails.
if (-not $SkipDownloads) {
    $texconv = "$OutDir\_texconv.exe"
    if (-not (Test-Path $texconv)) {
        try {
            Invoke-WebRequest 'https://github.com/microsoft/DirectXTex/releases/download/may2026/texconv.exe' `
                -OutFile $texconv -UseBasicParsing -TimeoutSec 120
        } catch { Write-Host "  download failed: texconv.exe" }
    }
    # Never run an unverified binary: it must be Authenticode-signed by Microsoft.
    $sig = if (Test-Path $texconv) { Get-AuthenticodeSignature $texconv } else { $null }
    if ($sig -and $sig.Status -eq 'Valid' -and $sig.SignerCertificate.Subject -match 'O=Microsoft Corporation') {
        # UNORM and SNORM both, because they are separate decoders: the signed
        # variants are the ones ImageMagick and WIC refuse outright.
        $ddsFmts = [ordered]@{
            'sample-bc4.dds'   = 'BC4_UNORM'
            'sample-bc4s.dds'  = 'BC4_SNORM'
            'sample-bc5.dds'   = 'BC5_UNORM'
            'sample-bc5s.dds'  = 'BC5_SNORM'
            'sample-bc6h.dds'  = 'BC6H_UF16'
            'sample-bc7.dds'   = 'BC7_UNORM'
            'sample-bc7srgb.dds' = 'BC7_UNORM_SRGB'
            'sample-dds-rgba16f.dds' = 'R16G16B16A16_FLOAT'
            'sample-dds-bgra8.dds'   = 'B8G8R8A8_UNORM'
        }
        foreach ($n in $ddsFmts.Keys) {
            if (Test-Path "$OutDir\$n") { continue }
            # texconv names its output after the INPUT file, so generate into a
            # scratch dir with a per-format suffix and rename.
            $sfx = "_$($ddsFmts[$n])"
            & $texconv -f $ddsFmts[$n] -y -m 1 -o $OutDir -sx $sfx $base 2>&1 | Out-Null
            $made = "$OutDir\_base$sfx.dds"
            if (Test-Path $made) { Move-Item $made "$OutDir\$n" -Force }
            else { Write-Host "  dds: texconv could not write $($ddsFmts[$n])" }
        }
        Remove-Item $texconv -Force -EA SilentlyContinue
    } else {
        Write-Host "  (dds: texconv missing or not Microsoft-signed — BC4/BC5/BC6H/BC7 samples skipped)"
    }
    # Two REAL-WORLD textures on top of the texconv matrix, from the reference C
    # `bcdec` repo (MIT). Every file above came out of one encoder, so they all share
    # its habits; these were written by different tools and are what proved our
    # output matches DirectXTex byte-for-byte on real art (and where it differs by
    # the 1-LSB the D3D spec allows). The BC6H one is SIGNED and a real HDR
    # panorama, which is the case nothing else in the tree could decode at all.
    $wild = @{
        'sample-dds-bc7-real.dds'  = 'https://raw.githubusercontent.com/iOrange/bcdec/main/test_images/dice_bc7.dds'
        'sample-dds-bc6hs.dds'     = 'https://raw.githubusercontent.com/iOrange/bcdec/main/test_images/lythwood_room_1k_bc6h_signed.dds'
    }
    foreach ($n in $wild.Keys) {
        if (Test-Path "$OutDir\$n") { continue }
        try { Invoke-WebRequest $wild[$n] -OutFile "$OutDir\$n" -UseBasicParsing -TimeoutSec 60 } catch { Write-Host "  download failed: $n" }
    }
}

# --- 6) Alias extensions that share a backend (and container layout) with an
#        already-built sample. Decode is content-sniffed (extension-agnostic — see
#        container/mod.rs::extract_cover), so a byte-copy under the new name renders
#        identically. Donor -> aliases, grouped by container family:
$aliases = [ordered]@{
    'afdesign' = @('af', 'afphoto', 'afpub')          # Affinity (Serif metadata + embedded preview)
    'mobi'     = @('azw', 'azw3')                      # Kindle/Mobipocket (BOOKMOBI cover)
    'djvu'     = @('djv')                              # DjVu (IFF85 AT&TFORM)
    'gcode'    = @('gco')                              # sliced G-code (base64 PNG thumbnail block)
    'docx'     = @('docm', 'dotx', 'dotm')            # Word OOXML (OPC zip, docProps/thumbnail)
    'xlsx'     = @('xlsm', 'xlsb', 'xltx', 'xltm')    # Excel OOXML/OPC zip
    'pptx'     = @('ppsx', 'ppsm', 'potm', 'pptm', 'potx') # PowerPoint OOXML (OPC zip)
    # Legacy binary Office is OLE compound (\xD0\xCF\x11\xE0); the OLE
    # SummaryInformation thumbnail path (ole.rs) is the same one Publisher uses, so
    # the .pub donor exercises the identical decode for .doc/.ppt/.xls + templates.
    'pub'      = @('doc', 'dot', 'ppt', 'pps', 'pot', 'xls', 'xlt')
}
foreach ($donor in $aliases.Keys) {
    $src = "$OutDir\sample.$donor"
    if (-not (Test-Path $src)) { Write-Host "  (alias: donor sample.$donor missing — skipped $($aliases[$donor] -join ','))"; continue }
    foreach ($a in $aliases[$donor]) { Copy-Item $src "$OutDir\sample.$a" -Force }
}

# Musepack .mpc with an APEv2 cover — exercises container/audio.rs's APEv2 cover
# fallback (lofty doesn't expose APEv2 cover art as a picture). No Musepack-with-art
# exists in the wild test sets, so we craft one from a real .mpc. Best-effort: needs
# python + mutagen; skipped with a note otherwise.
$py = (Get-Command python -EA SilentlyContinue).Source
if ($py -and -not $SkipDownloads) {
    try {
        $mpcSrc = "$OutDir\_mpc_base.mpc"
        Invoke-WebRequest 'https://raw.githubusercontent.com/Serial-ATA/lofty-rs/main/lofty/tests/files/assets/minimal/mpc_sv8.mpc' -OutFile $mpcSrc -UseBasicParsing -TimeoutSec 30
        $mk = @'
import sys
try:
    from mutagen.musepack import Musepack
    from mutagen.apev2 import APEValue, BINARY
except ImportError:
    print("mutagen-missing"); sys.exit(0)
src, out, cover = sys.argv[1], sys.argv[2], sys.argv[3]
data = open(src, "rb").read()
if data[:3] == b"ID3":   # strip a leading ID3v2 so the file starts with MPCK
    n = (data[6] & 0x7f) << 21 | (data[7] & 0x7f) << 14 | (data[8] & 0x7f) << 7 | (data[9] & 0x7f)
    data = data[10 + n:]
open(out, "wb").write(data)
f = Musepack(out)
f["Cover Art (Front)"] = APEValue(b"cover.png\x00" + open(cover, "rb").read(), BINARY)
f.save()
print("ok")
'@
        $mkFile = "$OutDir\_mkmpc.py"; [System.IO.File]::WriteAllText($mkFile, $mk)
        $res = & $py $mkFile $mpcSrc "$OutDir\sample.mpc" $base 2>&1
        Remove-Item $mkFile, $mpcSrc -Force -EA SilentlyContinue
        if ($res -notmatch 'ok') { Write-Host "  (mpc: $res — install mutagen to generate; skipped)" }
    } catch { Write-Host "  (mpc: $($_.Exception.Message); skipped)" }
} else { Write-Host "  (mpc: needs python+mutagen; sample.mpc skipped)" }

# --- 7b) Real audio samples with an embedded cover. Magick can't write audio at
# all — the old loop faked every audio format as a renamed PNG, so regression
# only ever proved PNG sniffing. Real minimal files from mutagen's test data get
# the base PNG embedded as a front cover via mutagen (per-container tag flavor:
# ID3/APIC, FLAC Picture, Vorbis METADATA_BLOCK_PICTURE, MP4 covr, APEv2 binary),
# exercising the real lofty cover path in container/audio.rs. Best-effort like
# mpc/dsf: needs python + mutagen + network; skipped (-> untested) otherwise.
$audioSrc = [ordered]@{
    mp3  = 'silence-44-s.mp3'
    flac = 'silence-44-s.flac'
    ogg  = 'empty.ogg'
    opus = 'example.opus'
    spx  = 'empty.spx'
    m4a  = 'has-tags.m4a'
    ape  = 'mac-399.ape'
    wv   = 'silence-44-s.wv'
    wav  = 'silence-2s-PCM-16000-08-ID3v23.wav'
    aiff = 'with-id3.aif'
    aac  = 'empty.aac'
}
if ($py -and -not $SkipDownloads) {
    $tagPy = @'
import sys, base64
from mutagen import File
from mutagen.id3 import ID3, APIC
from mutagen.flac import FLAC, Picture
from mutagen.mp4 import MP4, MP4Cover
from mutagen.apev2 import APEv2, APEValue, BINARY
from mutagen.oggvorbis import OggVorbis
from mutagen.oggopus import OggOpus
from mutagen.oggspeex import OggSpeex
out, cover = sys.argv[1], sys.argv[2]
png = open(cover, 'rb').read()
f = File(out)
if isinstance(f, FLAC):
    pic = Picture(); pic.type = 3; pic.mime = 'image/png'; pic.data = png
    f.add_picture(pic); f.save()
elif isinstance(f, MP4):
    f['covr'] = [MP4Cover(png, imageformat=MP4Cover.FORMAT_PNG)]; f.save()
elif isinstance(f, (OggVorbis, OggOpus, OggSpeex)):
    pic = Picture(); pic.type = 3; pic.mime = 'image/png'; pic.data = png
    f['metadata_block_picture'] = [base64.b64encode(pic.write()).decode('ascii')]; f.save()
else:
    try:
        if f is not None and f.tags is None:
            f.add_tags()
    except Exception:
        f = None
    if f is not None and isinstance(f.tags, APEv2):
        f.tags['Cover Art (Front)'] = APEValue(b'cover.png\x00' + png, BINARY); f.save()
    elif f is not None and isinstance(f.tags, ID3):
        f.tags.add(APIC(encoding=3, mime='image/png', type=3, desc='cover', data=png)); f.save()
    else:
        # raw stream mutagen can't tag in place (ADTS AAC): prepend a standalone ID3v2
        t = ID3(); t.add(APIC(encoding=3, mime='image/png', type=3, desc='cover', data=png))
        t.save(out, v2_version=3)
print('ok')
'@
    $tagFile = "$OutDir\_tagaudio.py"; [System.IO.File]::WriteAllText($tagFile, $tagPy)
    $audioOk = @(); $audioSkip = @()
    foreach ($a in $audioSrc.Keys) {
        $dst = "$OutDir\sample.$a"
        try {
            Invoke-WebRequest "https://raw.githubusercontent.com/quodlibet/mutagen/main/tests/data/$($audioSrc[$a])" -OutFile $dst -UseBasicParsing -TimeoutSec 30
            $r = (& $py $tagFile $dst $base 2>&1) -join ' '
            if ($r -match 'ok') { $audioOk += $a } else { Remove-Item $dst -Force -EA SilentlyContinue; $audioSkip += "$a($r)" }
        } catch { Remove-Item $dst -Force -EA SilentlyContinue; $audioSkip += $a }
    }
    Remove-Item $tagFile -Force -EA SilentlyContinue
    Write-Host "[corpus] real audio + embedded cover: $($audioOk -join ' ')$(if ($audioSkip.Count) { "  (skipped: $($audioSkip -join ' '))" })"
} else { Write-Host "  (audio samples: need python+mutagen+network; skipped)" }
# Container-identical audio aliases ride the same bytes (content-sniffed):
foreach ($p in @(, @('aiff', 'aif')) + @(, @('ogg', 'oga')) + @(, @('m4a', 'm4b'))) {
    if (Test-Path "$OutDir\sample.$($p[0])") { Copy-Item "$OutDir\sample.$($p[0])" "$OutDir\sample.$($p[1])" -Force }
}

# --- 8) Alias / variant extensions + video samples (complete the full format set) ----
# Our decoders CONTENT-SNIFF, so an alias is the same bytes as a base format under a
# different extension - a valid coverage test that the extension is hooked and decodes.
$aliasMap = [ordered]@{
    blend    = (1..32 | ForEach-Object { "blend$_" })    # Blender auto-save backups
    psd      = @('pdd', 'psdt')                            # Photoshop bitmap / template
    tga      = @('tpic'); iff = @('ilbm'); jxr = @('wmp')
    jpg      = @('jfif', 'mpo')                           # JFIF IS JPEG; MPO = JPEG-compatible multi-picture
    sgi      = @('bw')                                     # B&W flavor of the same SGI container
    jp2      = @('jpf', 'jpx')                             # JPEG-2000 variants
    hdr      = @('hdri', 'rgbe', 'xyze')                  # Radiance HDR variants
    heic     = @('heif', 'heics', 'heifs', 'hif', 'avci') # HEIF variants
    skp      = @('skb'); emf = @('emg'); exr = @('cxr'); cdr = @('cmx')
    afpub    = @('aftemplate'); indd = @('indt'); pspimage = @('psp')
    cbz      = @('phz'); pcd = @('ph')
    orf      = @('ori')
    # NO camera-RAW cross-format aliases here. `iiq = bay, cap`, `dcr = drf, dcs`,
    # `pef = ptx` and `dng = pxn` used to live on this line and were REMOVED 2026-08-21.
    # Every one of them copied one vendor's sensor dump under another vendor's extension,
    # and because our decoders are content-sniffed the copy renders through the ORIGINAL
    # format's path and proves exactly nothing about the extension it is pretending to be.
    # `_no-real-sample.txt`'s own preamble forbids this in as many words. `sample.pxn` was
    # the only one still on disk (a byte-for-byte copy of `sample.dng`, itself a stub that
    # decodes to solid black), so the "we render PXN" row of the gate was asserting a black
    # square; the other four were already gone and these lines would have recreated them on
    # the next rebuild. `orf = ori` STAYS: those two ARE the same Olympus format under two
    # extensions, which is the only kind of alias that is honest.
}
$aliasN = 0
foreach ($b in $aliasMap.Keys) {
    $src = "$OutDir\sample.$b"; if (-not (Test-Path $src)) { continue }
    foreach ($a in $aliasMap[$b]) { Copy-Item $src "$OutDir\sample.$a" -Force; $aliasN++ }
}
# wmz = gzip-compressed WMF (mirrors emz = gzip(emf)).
if (Test-Path "$OutDir\sample.wmf") {
    $wmf = [System.IO.File]::ReadAllBytes("$OutDir\sample.wmf")
    $fs = [System.IO.File]::Create("$OutDir\sample.wmz")
    $gz = New-Object System.IO.Compression.GzipStream($fs, [System.IO.Compression.CompressionMode]::Compress)
    $gz.Write($wmf, 0, $wmf.Length); $gz.Dispose(); $fs.Dispose(); $aliasN++
}
# GeoGebra .ggb: a ZIP whose root geogebra_thumbnail.png is the preview.
$ggbPrev = "$OutDir\_ggb.png"; & $magick $base -resize 200x140 $ggbPrev 2>$null
if (Test-Path $ggbPrev) {
    New-Zip "$OutDir\sample.ggb" @{ 'geogebra_thumbnail.png' = [System.IO.File]::ReadAllBytes($ggbPrev); 'geogebra.xml' = '<?xml version="1.0"?><geogebra/>' }
    Remove-Item $ggbPrev -Force -EA SilentlyContinue; $aliasN++
}
Write-Host "[corpus] $aliasN alias/variant samples (Blender backups, image + RAW aliases, wmz, ggb)"

# Small per-container video clips so the Media Foundation video tier is exercised. The
# mp4-family (mp4/m4v/mov/qt/3gp/3g2/f4v) shares one ISO-BMFF clip; others are per-container.
# Codec-less ones (mpg/mpeg/flv/ts/m2ts/mts/vob/ogv) still exercise the path - they fall to the
# default icon, but must not crash or hang - so they belong in the corpus as coverage.
if (-not $SkipDownloads) {
    $vidBase = 'https://filesamples.com/samples/video'
    foreach ($v in 'mp4', 'mkv', 'webm', 'avi', 'wmv', 'flv', 'mpg', 'mpeg', 'ts', 'm2ts', 'mts', 'vob', 'ogv') {
        try { Invoke-WebRequest "$vidBase/$v/sample_640x360.$v" -OutFile "$OutDir\sample.$v" -UseBasicParsing -TimeoutSec 90 }
        catch { Write-Host "  video download failed: $v" }
    }
    foreach ($p in @(, @('mp4', 'm4v')) + @(, @('mp4', 'mov')) + @(, @('mp4', 'qt')) + @(, @('mp4', '3gp')) + @(, @('mp4', '3g2')) + @(, @('mp4', 'f4v')) + @(, @('avi', 'divx')) + @(, @('wmv', 'asf')) + @(, @('mpg', 'm2v'))) {
        if (Test-Path "$OutDir\sample.$($p[0])") { Copy-Item "$OutDir\sample.$($p[0])" "$OutDir\sample.$($p[1])" -Force }
    }
    Write-Host "[corpus] video samples downloaded + container-aliased"
}
else { Write-Host "  (video samples need network; -SkipDownloads given - skipped)" }

# --- 9) Coverage completion: formats the steps above don't otherwise emit -----
# png: the base IS a PNG, but it's named _base.png (skipped by the _* harness filter),
# so emit a plain sample.png. aifc: AIFF-C reads through the SAME content-sniffed lofty
# path as .aiff, so a byte-copy is a valid hook+decode coverage test (like the aliases).
if (Test-Path $base) { Copy-Item $base "$OutDir\sample.png" -Force }
if (Test-Path "$OutDir\sample.aiff") { Copy-Item "$OutDir\sample.aiff" "$OutDir\sample.aifc" -Force }
# dsf: DSD audio has its OWN magic ("DSD "), so a byte-copy alias would only test the hook,
# not lofty's DSF reader. Fetch a real minimal DSF and embed the base PNG as an ID3v2 cover
# (mirrors the .mpc path above). Best-effort: needs python + mutagen + network.
if ($py -and -not $SkipDownloads -and -not (Test-Path "$OutDir\sample.dsf")) {
    try {
        $dsfSrc = "$OutDir\_dsf_base.dsf"
        Invoke-WebRequest 'https://raw.githubusercontent.com/quodlibet/mutagen/main/tests/data/2822400-1ch-0s-silence.dsf' -OutFile $dsfSrc -UseBasicParsing -TimeoutSec 30
        $mkd = @'
import sys
try:
    from mutagen.dsf import DSF
    from mutagen.id3 import APIC
except ImportError:
    print("mutagen-missing"); sys.exit(0)
src, out, cover = sys.argv[1], sys.argv[2], sys.argv[3]
open(out, "wb").write(open(src, "rb").read())
f = DSF(out)
if f.tags is None:
    f.add_tags()
f.tags.add(APIC(encoding=3, mime="image/png", type=3, desc="cover", data=open(cover, "rb").read()))
f.save()
print("ok")
'@
        $mkdFile = "$OutDir\_mkdsf.py"; [System.IO.File]::WriteAllText($mkdFile, $mkd)
        $resd = & $py $mkdFile $dsfSrc "$OutDir\sample.dsf" $base 2>&1
        Remove-Item $mkdFile, $dsfSrc -Force -EA SilentlyContinue
        if ($resd -notmatch 'ok') { Write-Host "  (dsf: $resd - install mutagen to generate; skipped)" }
    } catch { Write-Host "  (dsf: $($_.Exception.Message); skipped)" }
} elseif (-not (Test-Path "$OutDir\sample.dsf")) { Write-Host "  (dsf: needs python+mutagen+network; sample.dsf skipped)" }
Write-Host "[corpus] coverage completion: png + aifc + dsf"

# --- 9b) A DELIBERATELY HUGE JPEG 2000 -----------------------------------------
# Issue #11's second half: a reporter's 9958x7686 (76 MP) archive.org map scan was
# only ~11 MB on disk but blew the preview pane's 12 s decode budget, so the pane
# showed nothing on a file that decodes fine. File SIZE is not the trigger, pixel
# COUNT is, and no ordinary corpus sample is big enough to catch a regression here.
# JP2 has no pure-Rust or in-box Windows decoder, so this always takes the
# ImageMagick subprocess: exactly the slow path that has to stay inside the budget.
if (-not (Test-Path "$OutDir\huge.jp2")) {
    Write-Host "[corpus] generating huge.jp2 (76 MP - this one takes a moment)"
    try {
        & $magick $base -resize '9958x7686!' -quality 40 "$OutDir\huge.jp2" 2>&1 | Out-Null
        if (Test-Path "$OutDir\huge.jp2") {
            $mb = [math]::Round((Get-Item "$OutDir\huge.jp2").Length / 1MB, 1)
            Write-Host "  huge.jp2: $mb MB on disk, 76 MP decoded"
        }
    } catch { Write-Host "  (huge.jp2: $($_.Exception.Message); skipped)" }
}

# --- 9c) Tiny LOSSLESS JPEG 2000 exactness fixtures ----------------------------
# The native reduced-resolution JP2 decoder (src/decode/jp2) is verified by BIT-EXACT
# comparison against these: reversible 5/3 means a correct decoder must reproduce the
# source PNG perfectly, so one differing byte is a decoder bug, not noise. Plasma content
# matters: smooth gradients are insensitive to the zero-coding H/V swap and would pass a
# broken decoder; the textured files are what distinguish correct from almost-correct.
foreach ($t in @(
        @{ n = 'tiny8-gray';    a = '-size 8x8 gradient:black-white -colorspace Gray' },
        @{ n = 'tiny16-rgb';    a = '-size 16x16 gradient:red-blue' },
        @{ n = 'tiny16-plasma'; a = '-size 16x16 plasma:fractal' },
        @{ n = 'tiny16-gplasma'; a = '-size 16x16 plasma:fractal -colorspace Gray' },
        @{ n = 'tiny32-grad';   a = '-size 32x32 gradient:red-blue' },
        @{ n = 'tiny32-plasma'; a = '-size 32x32 plasma:fractal' })) {
    $png = "$OutDir\$($t.n).png"; $jp2 = "$OutDir\$($t.n).jp2"
    if ((Test-Path $png) -and (Test-Path $jp2)) { continue }
    # NOTE: plasma is RANDOM per invocation, so the pair must be generated together and
    # then left alone - regenerating only one of them breaks the exactness contract.
    Invoke-Expression "& `"$magick`" $($t.a) -depth 8 `"$png`"" 2>$null
    & $magick $png -define jp2:lossless $jp2 2>$null
    $ae = & $magick compare -metric AE $png $jp2 null: 2>&1
    if ("$ae" -notmatch '^0') { Write-Host "  ($($t.n): NOT lossless (AE=$ae) - exactness tests will skip/fail)" -ForegroundColor Yellow }
}
Write-Host "[corpus] tiny lossless jp2 exactness fixtures present"
# tiny-bilevel.jp2 (341 bytes) is a REAL user file from issue #11: 1-bit, PALETTED
# (pclr maps index 0 -> white), 2550x3301. It pins the palette path - a decoder that
# renders raw indices paints this blank white page solid black. It is checked in-tree
# by hand; nothing regenerates it (and nothing should - its exact box layout is the fixture).
if (-not (Test-Path "$OutDir	iny-bilevel.jp2")) { Write-Host "  (tiny-bilevel.jp2 missing - restore it from the repo/issue #11 attachment)" -ForegroundColor Yellow }

# --- 9z) BIG layered GIMP files, with a KNOWN flattened colour -----------------
# The corpus had two .xcf samples, 1.8 KB and 206 KB, and that gap shipped a bug: 2.0.0's
# layer budget dropped the top layers of any file whose layers exceeded it, so 15-layer GIMP
# images thumbnailed the wrong layer and some produced nothing at all. Nothing here was big
# enough to fail. These are written by make-xcf-fixture.py rather than downloaded, because a
# real .xcf of this shape is tens of MB and nobody publishes one.
#
# Every layer is a flat colour from a fixed palette, so each file has a KNOWN correct centre
# pixel, recorded in _expected-colors.txt. That is what makes them testable at all: they all
# render a perfectly valid PNG either way, and only the colour says which layer it is a
# picture of. Check them with:
#   python scripts\compare-renders.py --corpus <corpus> --out <tmp> --new <st2k.exe> `
#       --expect <corpus>\_expected-colors.txt
$expectedColors = @(
    '# <sample><TAB><r,g,b> - the colour a CORRECT render produces. Checked by regression.ps1',
    '# via compare-renders.py. See build-corpus.ps1 section 9z for what each one proves.',
    '',
    '# GIMP: every layer a flat palette colour, so the right answer is the TOP layer. These are',
    '# what catch a decoder that composites the wrong subset (2.0.0 shipped exactly that).'
)
$xcfGen = Join-Path $PSScriptRoot 'make-xcf-fixture.py'
if ($py -and (Test-Path $xcfGen)) {
    & $py $xcfGen $OutDir --matrix
    $expectedColors += @(
        "sample-xcf-layers-over-budget.xcf`t230,220,30",
        "sample-xcf-layers-transparent.xcf`t230,220,30",
        "sample-xcf-big-canvas-2-layers.xcf`t240,140,20",
        "sample-xcf-wide-canvas.xcf`t230,220,30"
    )
    Write-Host "[corpus] big layered .xcf samples written"
} else {
    Write-Host "  (big .xcf samples: need python; skipped)" -ForegroundColor Yellow
}

# --- 9z1) DjVu samples that are not an ordinary scan --------------------------
# The corpus had one real .djvu (a DjVuLibre-encoded layered scan, copied to .djv) and that
# single shape hid two shipping bugs until 2.3.1:
#
#   * a DjVuPhoto page - INFO + BG44, no mask, the right profile for a photograph or a
#     grayscale scan - came back a FLAT GREY RECTANGLE at every size Explorer asks for, on
#     any page past roughly 4267 px on the long edge. That is a letter scan at 400 dpi. It
#     is a perfectly valid PNG of nothing, so gates 1-3 all passed it;
#   * a file carrying a baked TH44 thumbnail was answered WITH that thumbnail whatever was
#     asked for, and encoders cap TH44 at 128 px, so Explorer's 768 px view got a 128 px
#     picture to stretch.
#
# Neither can be reproduced with the real sample, and nobody publishes either shape as a
# test file. They are written by the repo's own decoder crate rather than by a
# make-*-fixture.py like the GIMP ones, because writing DjVu means an IW44 wavelet coder
# and a ZP arithmetic coder and we already link one:
#
#   cargo test --release --lib write_djvu_corpus_fixtures -- --ignored --nocapture
#
# (see src\container\djvu.rs). They need no entry in _expected-colors.txt: what makes them
# testable is gate 4, check-render-sanity.ps1, which flags a tile with no detail in it.
foreach ($djvuFixture in @('sample-djvu-photo.djvu', 'sample-djvu-thumbnail.djvu')) {
    if (-not (Test-Path (Join-Path $OutDir $djvuFixture))) {
        Write-Host "  ($djvuFixture missing - regenerate with: cargo test --release --lib write_djvu_corpus_fixtures -- --ignored)" -ForegroundColor Yellow
    }
}

# --- 9z1b) A PDF with more than one page ---------------------------------------
# The corpus's ordinary PDF, sample.pdf, has exactly ONE page, and every real-world PDF has
# more than one. That single shape hid a shipping bug until 2.3.1: the Quick preview gave all
# six arrow/page keys to page-turning whenever a document had multiple pages, and since paging
# clamps at both ends, landing on such a PDF trapped the keyboard with no way to any other
# file. sample.pdf exercised the opposite branch, so it passed forever.
#
# sample-decoy-multipage.pdf is NOT a substitute. Its two pages are flat blue and flat red and
# its whole job is proving we thumbnail page ONE; a flat page cannot tell you whether page 3
# is really page 3, and gate 4 would flag it as detail-free if it were ever asked to.
#
# So this is a four-page file, each page a different solid colour, written by the repo's own
# test code to the PDF spec (NOT by our own topdf writer - a fixture written by the code under
# test shares its assumptions, and the pair would then agree with each other while both being
# wrong about PDF):
#
#   cargo test --release --lib write_pdf_corpus_fixture -- --ignored --nocapture
#
# (see src\pdf.rs). Page one is the same blue every decoy fixture uses, so the existing
# thumbnail colour gate covers it with no special case; pages two to four are what
# pdf::tests::every_page_of_a_multipage_pdf_renders_as_itself asserts against.
$multiPdf = Join-Path $OutDir 'sample-multipage.pdf'
if (Test-Path $multiPdf) {
    $expectedColors += @(
        '',
        '# Four-page PDF, page one flat rgb(30,60,210). Pins that a MULTI-page document still',
        '# thumbnails from page ONE, which is the half sample.pdf could never test.',
        "sample-multipage.pdf`t30,60,210"
    )
} else {
    Write-Host "  (sample-multipage.pdf missing - regenerate with: cargo test --release --lib write_pdf_corpus_fixture -- --ignored)" -ForegroundColor Yellow
}

# --- 9z2) ZX Spectrum SCREEN$, a format that only a NAMED coder can reach ------
# A SCREEN$ is exactly 6912 bytes with no signature whatsoever: 6144 bytes of bitmap in the
# machine's interleaved layout, then 768 attribute bytes (one per 8x8 cell, FLASH/BRIGHT/
# PAPER/INK). Nothing about those bytes says "I am a picture", which is precisely why it is
# worth having: ImageMagick can only decode it when the FILE NAME names the coder, so this
# sample is the corpus proof for decode::decode_by_extension. Before that existed, `.scr` was
# a registered, advertised format that had never once produced a thumbnail.
#
# Written here rather than downloaded because the format is fully specified and tiny, so this
# IS a real SCREEN$ and not a stand-in. Every pixel set to INK, with BRIGHT red ink on black
# paper, makes the whole 256x192 screen one known colour.
$scrPath = Join-Path $OutDir 'sample.scr'
$scr = New-Object byte[] 6912
for ($i = 0; $i -lt 6144; $i++) { $scr[$i] = 0xFF }                 # bitmap: every pixel INK
for ($i = 6144; $i -lt 6912; $i++) { $scr[$i] = 0x42 }              # BRIGHT | PAPER 0 | INK 2
[System.IO.File]::WriteAllBytes($scrPath, $scr)
$expectedColors += @(
    '',
    '# ZX Spectrum SCREEN$: no signature at all, so it is decodable ONLY when the extension',
    '# names the coder. Renders red, or it never reached ImageMagick.',
    "sample.scr`t255,0,0"
)
Write-Host "[corpus] sample.scr written (6912 bytes, solid BRIGHT red)"

# --- 9z3) HIGH-BIT-DEPTH AVIF, with a KNOWN colour that a wrong TRANSFER destroys ------
# Microsoft's AV1 WIC codec decodes 10/12-bit AVIF through the wrong transfer function: it
# applies the BT.709 EOTF and re-encodes sRGB, which lifts midtones and shadows (a true 48
# comes back as 62, a true 128 as 138). It does NOT do this at 8 bits with byte-identical
# tags, which is what makes it a codec bug rather than a mis-tagged file. decode/color.rs
# inverts that curve in-process instead of paying an ImageMagick subprocess to avoid it
# (1261 ms -> 200 ms, and worst channel error 11 -> 1).
#
# None of that was testable before: the corpus's only .avif is 8-bit, so it exercises a
# COMPLETELY different branch and every gate was blind to the high-bit-depth path.
#
# The colour is a dark grey ON PURPOSE. Uncorrected it renders ~62 against a true 48 - an
# error of 14, comfortably outside compare-renders.py's +/-8 tolerance - whereas mid-grey
# would only miss by 10 and a bright patch by 1, i.e. would pass while broken.
$avifHi = Join-Path $OutDir 'sample-avif-10bit.avif'
$ffmpeg = (Get-Command ffmpeg -ErrorAction SilentlyContinue).Source
if ($ffmpeg) {
    $flatSrc = Join-Path $env:TEMP ("st2k-avif10-{0}.png" -f $PID)
    & magick -size 320x240 'xc:rgb(48,48,48)' $flatSrc 2>$null
    if (Test-Path $flatSrc) {
        & $ffmpeg -hide_banner -loglevel error -y -i $flatSrc -c:v libaom-av1 -still-picture 1 `
            -cpu-used 6 -crf 0 -pix_fmt yuv444p10le -colorspace bt709 -color_primaries bt709 `
            -color_trc bt709 $avifHi 2>$null
        Remove-Item $flatSrc -ErrorAction SilentlyContinue
    }
    # SELF-CHECK, the same discipline as fuzzseed's every_seed_reaches_its_parser: a fixture
    # that does not actually carry the properties under test is worse than no fixture, because
    # it goes green forever while proving nothing. It must be high-bit-depth (av1C bit 6) AND
    # carry an nclx colr box, or decode/color.rs routes it somewhere else entirely.
    if (Test-Path $avifHi) {
        $b = [System.IO.File]::ReadAllBytes($avifHi)
        $hasNclx = $false; $hiDepth = $false
        for ($i = 0; $i -lt $b.Length - 8; $i++) {
            if ($b[$i] -eq 0x63 -and $b[$i+1] -eq 0x6F -and $b[$i+2] -eq 0x6C -and $b[$i+3] -eq 0x72 -and
                $b[$i+4] -eq 0x6E -and $b[$i+5] -eq 0x63 -and $b[$i+6] -eq 0x6C -and $b[$i+7] -eq 0x78) { $hasNclx = $true }
            if ($b[$i] -eq 0x61 -and $b[$i+1] -eq 0x76 -and $b[$i+2] -eq 0x31 -and $b[$i+3] -eq 0x43) {
                if ((($b[$i+6] -shr 6) -band 1) -eq 1) { $hiDepth = $true }     # av1C byte 2, high_bitdepth
            }
        }
        if ($hasNclx -and $hiDepth) {
            $expectedColors += @(
                '',
                '# 10-bit AVIF, flat rgb(48,48,48). Windows AV1 WIC returns ~62 for this through the',
                '# wrong transfer curve; decode/color.rs inverts it. Renders 48 or the correction broke.',
                "sample-avif-10bit.avif`t48,48,48"
            )
            Write-Host "[corpus] sample-avif-10bit.avif written (10-bit, nclx, flat rgb(48,48,48))"
        } else {
            Remove-Item $avifHi -ErrorAction SilentlyContinue
            Write-Host "[corpus] sample-avif-10bit.avif REJECTED (hiDepth=$hiDepth nclx=$hasNclx) - a fixture that cannot fail is worse than none" -ForegroundColor Yellow
        }
    }
} else {
    Write-Host "[corpus] ffmpeg not found - skipping the 10-bit AVIF fixture" -ForegroundColor Yellow
}

# --- 9z4) 8-bit BT.601 AVIF, with a KNOWN colour a wrong MATRIX destroys -------------
# The other half of the AVIF colour story (9z3 is the 10-bit transfer curve). avifenc and
# ffmpeg default to BT.601 colour for 8-bit AVIF; Microsoft's converters assume BT.709 and
# clip, shifting saturated colour by up to 39/255. decode/avifmf.rs decodes these through
# the OS AV1 decoder's raw YUV and applies the correct matrix itself. This fixture is flat
# rgb(0,255,0) because saturated green shifts hardest: the wrong matrix renders ~(0,216,0),
# comfortably outside compare-renders.py's +/-8 tolerance, while both correct paths (Media
# Foundation and the ImageMagick fallback) land within 1.
$avif601 = Join-Path $OutDir 'sample-avif-601.avif'
$ffmpeg601 = (Get-Command ffmpeg -ErrorAction SilentlyContinue).Source
if ($ffmpeg601) {
    $flat601 = Join-Path $env:TEMP ("st2k-avif601-{0}.png" -f $PID)
    & magick -size 320x240 'xc:rgb(0,255,0)' $flat601 2>$null
    if (Test-Path $flat601) {
        & $ffmpeg601 -hide_banner -loglevel error -y -i $flat601 -c:v libaom-av1 -still-picture 1 `
            -cpu-used 6 -crf 4 -pix_fmt yuv420p -colorspace smpte170m -color_primaries bt709 `
            -color_trc bt709 $avif601 2>$null
        Remove-Item -LiteralPath $flat601 -Force -ErrorAction SilentlyContinue
    }
    # Self-check (the fuzzseed discipline): it must really be 8-bit Main-profile with a
    # matrix-6 nclx, or it exercises a different branch and goes green proving nothing.
    if (Test-Path $avif601) {
        $b = [System.IO.File]::ReadAllBytes($avif601)
        $ok = $false
        for ($i = 0; $i -lt $b.Length - 18; $i++) {
            if ($b[$i] -eq 0x6E -and $b[$i+1] -eq 0x63 -and $b[$i+2] -eq 0x6C -and $b[$i+3] -eq 0x78) {
                # nclx: primaries u16, transfer u16, matrix u16
                if ($b[$i+8] -eq 0 -and $b[$i+9] -eq 6) { $ok = $true }
            }
        }
        if ($ok) {
            $expectedColors += @(
                '',
                '# 8-bit BT.601 AVIF (avifenc/ffmpeg-default colour), flat rgb(0,255,0). The wrong',
                '# matrix renders ~(0,216,0); the correct paths land within 1. See decode/avifmf.rs.',
                "sample-avif-601.avif`t0,255,0"
            )
            Write-Host "[corpus] sample-avif-601.avif written (8-bit, 4:2:0, nclx matrix 6)"
        } else {
            Remove-Item -LiteralPath $avif601 -Force -ErrorAction SilentlyContinue
            Write-Host "[corpus] sample-avif-601.avif REJECTED (no matrix-6 nclx) - a fixture that cannot fail is worse than none" -ForegroundColor Yellow
        }
    }
} else {
    Write-Host "[corpus] ffmpeg not found - skipping the BT.601 AVIF fixture" -ForegroundColor Yellow
}


# --- 9z5) WIDE-GAMUT JPEG, with a KNOWN colour that a DROPPED ICC PROFILE destroys ----
# JPEG is the most common format there is and the corpus had not one colour-managed sample,
# which is how a real bug lived here undetected: WIC's JPEG decoder answers GetColorContexts
# with an Exif-flag context rather than a profile one, so the scaled JPEG fast path handed back
# RAW AdobeRGB numbers. rgb(60,150,200) is chosen because its round trip through AdobeRGB is
# EXACT while the unmanaged numbers are (97,149,197) - a 37-level red error, far outside
# compare-renders.py's +/-8. Colour management now reads the APP2 chain itself (decode/color.rs
# jpeg_icc), and this fixture is what stops it being dropped again.
$jpegIcc = Join-Path $OutDir 'sample-jpeg-adobergb.jpg'
$colorDir = Join-Path $env:SystemRoot 'System32\spool\drivers\color'
$srgbIcc = Join-Path $colorDir 'sRGB Color Space Profile.icm'
$adobeIcc = Join-Path $colorDir 'AdobeRGB1998.icc'
if ((Test-Path $srgbIcc) -and (Test-Path $adobeIcc)) {
    & magick -size 320x240 'xc:rgb(60,150,200)' -profile $srgbIcc -profile $adobeIcc `
        -sampling-factor 1x1 -quality 100 $jpegIcc 2>$null
    # Self-check (the fuzzseed discipline), BOTH halves: the file must really carry a profile,
    # and its unmanaged numbers must really be wrong - a fixture that passes either way proves
    # nothing.
    if (Test-Path $jpegIcc) {
        $jb = [System.IO.File]::ReadAllBytes($jpegIcc)
        $sig = [System.Text.Encoding]::ASCII.GetBytes('ICC_PROFILE')
        $hasIcc = $false
        for ($i = 0; $i -lt $jb.Length - $sig.Length; $i++) {
            $m = $true
            for ($j = 0; $j -lt $sig.Length; $j++) { if ($jb[$i + $j] -ne $sig[$j]) { $m = $false; break } }
            if ($m) { $hasIcc = $true; break }
        }
        $rawPx = & magick $jpegIcc -strip -format '%[fx:int(255*p{10,10}.r+0.5)]' info: 2>$null
        $wrongEnough = ([int]$rawPx - 60) -gt 8
        if ($hasIcc -and $wrongEnough) {
            $expectedColors += @(
                '',
                '# AdobeRGB-tagged JPEG, flat rgb(60,150,200) in sRGB. Ignoring the profile renders',
                '# (97,149,197). WIC does not surface a JPEG ICC, so decode/color.rs reads APP2.',
                "sample-jpeg-adobergb.jpg`t60,150,200"
            )
            Write-Host "[corpus] sample-jpeg-adobergb.jpg written (AdobeRGB, unmanaged red $rawPx)"
        } else {
            Remove-Item -LiteralPath $jpegIcc -Force -ErrorAction SilentlyContinue
            Write-Host "[corpus] sample-jpeg-adobergb.jpg REJECTED (icc=$hasIcc unmanagedRed=$rawPx) - a fixture that cannot fail is worse than none" -ForegroundColor Yellow
        }
    }
} else {
    Write-Host "[corpus] Windows colour profiles not found - skipping the wide-gamut JPEG fixture" -ForegroundColor Yellow
}

# --- 9z6) PROGRESSIVE JPEG ------------------------------------------------------------
# The JPEG variant two decoders are most likely to disagree on, and the corpus had none - so
# routing JPEG to the OS codec was verified only against baseline files. Flat colour, so it
# joins the known-colour gate rather than only the renders-at-all one.
$jpegProg = Join-Path $OutDir 'sample-jpeg-progressive.jpg'
& magick -size 320x240 'xc:rgb(200,120,40)' -interlace Plane -sampling-factor 1x1 `
    -quality 100 $jpegProg 2>$null
if (Test-Path $jpegProg) {
    # Self-check: SOF2 (0xFFC2) is what makes it progressive. Without it this is just another
    # baseline JPEG and the fixture proves nothing.
    $pb = [System.IO.File]::ReadAllBytes($jpegProg)
    $isProg = $false
    for ($i = 0; $i -lt $pb.Length - 1; $i++) {
        if ($pb[$i] -eq 0xFF -and $pb[$i + 1] -eq 0xC2) { $isProg = $true; break }
    }
    if ($isProg) {
        $expectedColors += @(
            '',
            '# Progressive JPEG (SOF2), flat rgb(200,120,40). Pins that the OS-codec fast path',
            '# reads the progressive scan the same way the pure-Rust tier does.',
            "sample-jpeg-progressive.jpg`t200,120,40"
        )
        Write-Host "[corpus] sample-jpeg-progressive.jpg written (SOF2)"
    } else {
        Remove-Item -LiteralPath $jpegProg -Force -ErrorAction SilentlyContinue
        Write-Host "[corpus] sample-jpeg-progressive.jpg REJECTED (no SOF2) - a fixture that cannot fail is worse than none" -ForegroundColor Yellow
    }
}

# Every OTHER multi-part format has the same exposure XCF had: choosing a page, a frame or an
# icon size is a CHOICE, and a wrong choice still produces a perfectly good picture. These
# fixtures make the right answer blue and every wrong one red, so the choice becomes testable.
# The generator self-checks that each decoy fixture really contains a decoy and deletes any
# that does not, because a fixture that cannot fail is worse than no fixture.
$decoyGen = Join-Path $PSScriptRoot 'make-decoy-fixtures.ps1'
if (Test-Path $decoyGen) {
    & pwsh -NoProfile -File $decoyGen -OutDir $OutDir
    $decoyBlue = @(
        'sample-decoy-multipage.pdf', 'sample-decoy-multipage.tif',
        'sample-decoy-frames.gif', 'sample-decoy-frames.webp',
        'sample-decoy-sizes.ico',
        'sample-big-canvas.psd', 'sample-big-canvas.png',
        'sample-big-canvas.jpg', 'sample-big-canvas.bmp'
    ) | Where-Object { Test-Path (Join-Path $OutDir $_) }
    if ($decoyBlue) {
        $expectedColors += @('', '# First page / first frame / largest icon is blue; every decoy behind it is red.')
        $expectedColors += ($decoyBlue | ForEach-Object { "$_`t30,60,210" })
    }
}

Set-Content -Path "$OutDir\_expected-colors.txt" -Value $expectedColors -Encoding ascii
Write-Host ("[corpus] _expected-colors.txt: {0} samples with a known correct colour" -f (($expectedColors | Where-Object { $_ -match "`t" }).Count))

# --- 10) Honesty ledger: registered formats with NO real sample ----------------
# Mostly Camera RAW (real sensor dumps are MBs and vendor-licensed — only dng has
# a small real download, aliased to pxn) plus the obscure magick-read-only long
# tail. regression.ps1 reads this file and reports them UNTESTED, so a PASS total
# is never mistaken for full-format coverage (they used to falsely pass as fakes).
$have = Get-ChildItem $OutDir -File | Where-Object { $_.Name -notlike '_*' } |
    ForEach-Object { $_.Extension.TrimStart('.').ToLower() } | Sort-Object -Unique
$noSample = @($exts | Sort-Object -Unique | Where-Object { $have -notcontains $_ })
# The commentary is regenerated with the list, or the next rebuild silently strips it and the
# reader after that assumes the gap is an oversight and "fixes" it with a renamed stand-in.
$noSampleOut = @(
    '# Registered extensions the corpus has NO sample of any kind for, so no gate in this repo',
    '# says anything about them. regression.ps1 prints this list on every run precisely so the',
    '# PASS number is never mistaken for full-format coverage.',
    '#',
    '# Lines starting with # are comments; everything else is one extension per line.',
    '#',
    '# Camera RAW is filled separately by `python scripts\fetch-raw-samples.py` (raw.pixls.us,',
    '# smallest CC0 sample per extension, hundreds of MB) - run it if the RAW extensions show up',
    '# here. Doing that in 2026-08 found three formats that had NEVER produced a thumbnail.',
    '#',
    '# NOTHING here is a candidate for a renamed stand-in: a sample that is not really the format',
    '# makes the gate lie, which is strictly worse than the honest gap this file records. The',
    '# Paint Shop Pro variants are the tempting case - they share psp.rs and its magic-based',
    '# dispatch, so a renamed .PspBrush would pass while proving nothing about a real frame,',
    '# mask, shape or selection file.'
) + $noSample
Set-Content -Path "$OutDir\_no-real-sample.txt" -Value ($noSampleOut -join "`n") -Encoding ascii
Write-Host "[corpus] $($noSample.Count) formats have NO real sample (recorded in _no-real-sample.txt): $($noSample -join ' ')" -ForegroundColor Yellow

$count = (Get-ChildItem $OutDir -File | Where-Object { $_.Name -notlike '_*' }).Count
Write-Host "[corpus] $count sample files in $OutDir" -ForegroundColor Green
