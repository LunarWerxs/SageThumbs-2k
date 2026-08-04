# Repro for issue #9 (the AVIF half): AVIF files written by libaom (avifenc, ffmpeg)
# come out of our cascade with shifted colours, while every independent decoder
# reads them correctly.
#
# Root cause found 2026-08-04: for .avif the cascade reaches WIC before ImageMagick,
# and Microsoft's AV1 Video Extension mishandles the colour signalling that libaom
# writes by default (full range + BT.601 matrix, i.e. avifenc's default CICP 1/13/6).
# ImageMagick/libheif and libavif both read the same files correctly, which is why
# the reporter's other viewers and Icaros were fine.
#
# Needs on PATH: magick, ffmpeg, python (with Pillow), and an installed st2k.exe.

$ErrorActionPreference = 'Stop'
$work = Join-Path $env:TEMP "st2k-avif-repro"
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $work | Out-Null
Push-Location $work

$st2k = "$env:ProgramFiles\SageThumbs2K\st2k.exe"
if (-not (Test-Path $st2k)) { throw "st2k.exe not found at $st2k" }

# Six saturated patches, 60x60 each. Greys stay correct under a matrix error and
# move under a range error, so the two failure modes are told apart by patch 5.
magick -size 60x60 xc:"rgb(206,79,79)" xc:"rgb(79,206,79)" xc:"rgb(79,79,206)" `
       xc:"rgb(230,180,40)" xc:"rgb(20,160,190)" xc:"rgb(128,128,128)" `
       +append -colorspace sRGB patches.png

# The shapes the reporter actually produces. libaom defaults to full range with a
# BT.601 matrix, which is what avifenc writes unless told otherwise.
$shapes = @('yuv444p10le', 'yuv444p', 'yuv420p10le')
foreach ($pf in $shapes) {
    ffmpeg -y -v error -i patches.png -c:v libaom-av1 -pix_fmt $pf `
        -color_range pc -colorspace smpte170m -color_primaries bt709 `
        -color_trc iec61966-2-1 -crf 18 -still-picture 1 -frames:v 1 "ff_$pf.avif"
    & $st2k thumbnail "ff_$pf.avif" "st2k_$pf.png" --size 360 | Out-Null
}

@'
import sys
from PIL import Image
def patches(path):
    im = Image.open(path).convert("RGB")
    return [im.getpixel((i * 60 + 30, 30)) for i in range(6)]
src = patches("patches.png")
def worst(a, b):
    return max(max(abs(x - y) for x, y in zip(p, q)) for p, q in zip(a, b))
print(f'{"source":34s}', " ".join(map(str, src)))
for pf in sys.argv[1:]:
    lib = patches(f"ff_{pf}.avif")          # Pillow decodes AVIF via libavif
    ours = patches(f"st2k_{pf}.png")
    print(f'{"libavif " + pf:34s}', " ".join(map(str, lib)), f" worst={worst(src, lib)}")
    print(f'{"st2k    " + pf:34s}', " ".join(map(str, ours)), f" worst={worst(src, ours)}")
'@ | Set-Content -Encoding UTF8 compare.py
python compare.py @shapes

Write-Host ""
Write-Host "Expected today: libavif worst<=3 (codec noise), st2k worst 13-19."
Write-Host "Greys shift on the 10-bit shapes (range), saturated colours shift on 8-bit (matrix)."
Write-Host "ImageMagick reads all of them correctly, so preferring it over WIC for .avif fixes it,"
Write-Host "at the cost of a subprocess and no coverage on the Compact install."
Pop-Location
