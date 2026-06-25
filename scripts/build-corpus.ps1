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
$special = 'epub','mobi','azw','azw3','fb2','fbz','cbz','cb7','cbr','cbt','kra','ora','3mf','fcstd','gcode','gco','clip','afphoto','afdesign','afpub','af','blend','psd','psb','djvu','djv','pdf','eps','emf','emz','wmf','sketch','procreate','key','pages','numbers','cdr','skp','dwg','3dm','xd','cdt','indd','vsdx','vsdm','max','vsd','pub'

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
$jpg = if (Test-Path $jpgPrev) { [System.IO.File]::ReadAllBytes($jpgPrev) } else { $png }
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
        # Camera RAW (decode-only — magick can't write it): real small samples.
        'sample.dng'      = 'https://raw.githubusercontent.com/rawpy/rawpy/v0.18.1/tests/iss115.DNG'
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
    }
    foreach ($n in $dls.Keys) {
        if (Test-Path "$OutDir\$n") { continue }
        try { Invoke-WebRequest $dls[$n] -OutFile "$OutDir\$n" -UseBasicParsing -TimeoutSec 60 } catch { Write-Host "  download failed: $n" }
    }
}

# Visio .vsdm is structurally identical to .vsdx — reuse the downloaded sample.
if (Test-Path "$OutDir\sample.vsdx") { Copy-Item "$OutDir\sample.vsdx" "$OutDir\sample.vsdm" -Force }

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
    'pptx'     = @('ppsx', 'ppsm', 'potm')            # PowerPoint OOXML (OPC zip)
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

# --- 8) Alias / variant extensions + video samples (complete the full format set) ----
# Our decoders CONTENT-SNIFF, so an alias is the same bytes as a base format under a
# different extension - a valid coverage test that the extension is hooked and decodes.
$aliasMap = [ordered]@{
    blend    = (1..32 | ForEach-Object { "blend$_" })    # Blender auto-save backups
    psd      = @('pdd', 'psdt')                            # Photoshop bitmap / template
    tga      = @('tpic'); iff = @('ilbm'); jxr = @('wmp')
    jp2      = @('jpf', 'jpx')                             # JPEG-2000 variants
    hdr      = @('hdri', 'rgbe', 'xyze')                  # Radiance HDR variants
    heic     = @('heics', 'heifs', 'hif')                # HEIF variants
    skp      = @('skb'); emf = @('emg'); exr = @('cxr'); cdr = @('cmx')
    afpub    = @('aftemplate'); indd = @('indt'); pspimage = @('psp')
    cbz      = @('phz'); pcd = @('ph')
    orf      = @('ori'); iiq = @('bay', 'cap'); dcr = @('drf', 'dcs'); pef = @('ptx'); dng = @('pxn')
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

$count = (Get-ChildItem $OutDir -File | Where-Object { $_.Name -notlike '_*' }).Count
Write-Host "[corpus] $count sample files in $OutDir" -ForegroundColor Green
