<#
  Twin JPEG XRs of ONE scene for the HDR test in src/decode/tests/colour.rs: the linear scRGB
  float picture Windows itself writes for HDR screenshots (128bppRGBAFloat, 80 nits = 1.0), and
  its 8-bit sRGB control. Same scene as the JPEG XL / AVIF twins (a grey ramp over six colour
  patches, diffuse white at 203 nits), written through WIC's own JPEG XR encoder via WPF, which
  is the only JPEG XR writer on a stock Windows box.

    pwsh scripts\make-jxr-hdr-fixtures.ps1 tests\fixtures\jxr
#>
param([Parameter(Mandatory)][string]$OutDir)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName PresentationCore
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$W = 320; $H = 200
$scrgbPerWhite = 203.0 / 80.0   # 1.0 in the scene (203 nits) is 2.5375 in scRGB
function Scene([int]$x, [int]$y) {
    if ($y -lt $H / 2) { $v = $x / ($W - 1); return @($v, $v, $v) }
    # red, green, blue, yellow, cyan, magenta at 60%, as scalars (a nested array here made
    # PowerShell hand `op_Multiply` an Object[]).
    $p = [math]::Min([int][math]::Floor($x * 6 / $W), 5)
    [double]$r = if ($p -in 0, 3, 5) { 0.6 } else { 0.0 }
    [double]$g = if ($p -in 1, 3, 4) { 0.6 } else { 0.0 }
    [double]$b = if ($p -in 2, 4, 5) { 0.6 } else { 0.0 }
    return @($r, $g, $b)
}
function SrgbOetf([double]$l) {
    $l = [math]::Min([math]::Max($l, 0.0), 1.0)
    if ($l -le 0.0031308) { return 12.92 * $l }
    return 1.055 * [math]::Pow($l, 1 / 2.4) - 0.055
}

# HDR twin: linear scRGB floats, exactly what WIC hands back for an HDR file.
$hdr = New-Object byte[] ($W * $H * 16)
# SDR twin: 8-bit sRGB (BGRA in memory), the control.
$sdr = New-Object byte[] ($W * $H * 4)
for ($y = 0; $y -lt $H; $y++) {
    for ($x = 0; $x -lt $W; $x++) {
        $rgb = Scene $x $y
        $o = ($y * $W + $x) * 16
        [BitConverter]::GetBytes([single]($rgb[0] * $scrgbPerWhite)).CopyTo($hdr, $o)
        [BitConverter]::GetBytes([single]($rgb[1] * $scrgbPerWhite)).CopyTo($hdr, $o + 4)
        [BitConverter]::GetBytes([single]($rgb[2] * $scrgbPerWhite)).CopyTo($hdr, $o + 8)
        [BitConverter]::GetBytes([single]1.0).CopyTo($hdr, $o + 12)
        $q = ($y * $W + $x) * 4
        $sdr[$q]     = [byte][math]::Round((SrgbOetf $rgb[2]) * 255)
        $sdr[$q + 1] = [byte][math]::Round((SrgbOetf $rgb[1]) * 255)
        $sdr[$q + 2] = [byte][math]::Round((SrgbOetf $rgb[0]) * 255)
        $sdr[$q + 3] = 255
    }
}

function Write-Jxr([string]$path, $source) {
    $enc = New-Object System.Windows.Media.Imaging.WmpBitmapEncoder
    $enc.Lossless = $true
    $enc.Frames.Add([System.Windows.Media.Imaging.BitmapFrame]::Create($source))
    $fs = [IO.File]::Create($path)
    try { $enc.Save($fs) } finally { $fs.Dispose() }
    "wrote $path $((Get-Item $path).Length) bytes"
}
$hdrBmp = [System.Windows.Media.Imaging.BitmapSource]::Create($W, $H, 96, 96, [System.Windows.Media.PixelFormats]::Rgba128Float, $null, $hdr, $W * 16)
$sdrBmp = [System.Windows.Media.Imaging.BitmapSource]::Create($W, $H, 96, 96, [System.Windows.Media.PixelFormats]::Bgra32, $null, $sdr, $W * 4)
Write-Jxr (Join-Path $OutDir 'scene-scrgb.jxr') $hdrBmp
Write-Jxr (Join-Path $OutDir 'scene-sdr709.jxr') $sdrBmp
