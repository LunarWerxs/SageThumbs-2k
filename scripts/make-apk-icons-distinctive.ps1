<#
.SYNOPSIS
Replace the stock Android robot icons inside a test-corpus APK with unmistakable ones.

.DESCRIPTION
The upstream sample APK (androguard's TestActivity.apk) ships the default green Android
robot as its launcher icon. That is the SAME picture Windows and every other tool shows
when an APK thumbnail FAILS, so a successful render and a total failure look identical.
It has already fooled a human reader once. A test asset whose pass state cannot be told
from its fail state is worse than no asset.

This rewrites the three density variants in place with flat, labelled tiles, keeping their
entry names byte-identical so `AndroidManifest.xml` and `resources.arsc` still resolve. It
also makes the sample test DENSITY SELECTION for free: the three are different colours and
carry their own density name, so the thumbnail says which one the decoder picked. hdpi is
the expected winner (highest density present).

Idempotent: re-running it just rewrites the same three entries.
#>
param(
    [string]$Apk = (Join-Path $PSScriptRoot '..\..\test-corpus\sample.apk')
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

if (-not (Test-Path -LiteralPath $Apk)) {
    Write-Host "  (no $Apk yet, skipping icon swap)"
    return
}

# density entry name -> (pixel size, background, label). Sizes are deliberately larger than
# the usual 36/48/72 so the 256 px tile is crisp rather than a 4x upscale of a postage stamp.
$variants = @(
    @{ Entry = 'res/drawable-ldpi/icon.png'; Size = 96;  Back = [System.Drawing.Color]::FromArgb(200, 60, 60);  Label = 'LDPI' },
    @{ Entry = 'res/drawable-mdpi/icon.png'; Size = 144; Back = [System.Drawing.Color]::FromArgb(60, 90, 200);  Label = 'MDPI' },
    @{ Entry = 'res/drawable-hdpi/icon.png'; Size = 192; Back = [System.Drawing.Color]::FromArgb(40, 160, 90);  Label = 'HDPI' }
)

function New-IconPng([int]$size, $back, [string]$label) {
    $bmp = New-Object System.Drawing.Bitmap $size, $size
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.Clear($back)
    # A thick ring plus the density name: nothing about this reads as "generic file icon".
    $pen = New-Object System.Drawing.Pen ([System.Drawing.Color]::White), ($size / 12)
    $inset = [int]($size / 6)
    $g.DrawEllipse($pen, $inset, $inset, $size - 2 * $inset, $size - 2 * $inset)
    $fontSize = [float]($size / 7)
    $font = New-Object System.Drawing.Font 'Arial', $fontSize, ([System.Drawing.FontStyle]::Bold)
    $fmt = New-Object System.Drawing.StringFormat
    $fmt.Alignment = 'Center'; $fmt.LineAlignment = 'Center'
    $rect = New-Object System.Drawing.RectangleF 0, 0, $size, $size
    $g.DrawString($label, $font, [System.Drawing.Brushes]::White, $rect, $fmt)
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $pen.Dispose(); $font.Dispose(); $g.Dispose(); $bmp.Dispose()
    return $ms.ToArray()
}

$zip = [System.IO.Compression.ZipFile]::Open($Apk, [System.IO.Compression.ZipArchiveMode]::Update)
try {
    foreach ($v in $variants) {
        $existing = $zip.GetEntry($v.Entry)
        if ($null -eq $existing) {
            Write-Host "  (no $($v.Entry) in this APK, skipping)"
            continue
        }
        # Deflate, not Store: the APK's other entries are deflated and our own zip reader is
        # exercised more usefully by a compressed entry than by a stored one.
        $existing.Delete()
        $entry = $zip.CreateEntry($v.Entry, [System.IO.Compression.CompressionLevel]::Optimal)
        $bytes = New-IconPng $v.Size $v.Back $v.Label
        $out = $entry.Open()
        $out.Write($bytes, 0, $bytes.Length)
        $out.Dispose()
    }
}
finally {
    $zip.Dispose()
}

Write-Host "  sample.apk icons replaced (ldpi/mdpi/hdpi are now labelled and colour-coded)"
