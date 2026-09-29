<#
.SYNOPSIS
  Every corpus sample built from _base.png, thumbnailed the way EXPLORER asks for it, checked
  by its corner colours - and one contact sheet that shows every failure at a glance.

.DESCRIPTION
  `sample.<ext>` is `test-corpus\_base.png` converted to that format, and the base has a red
  square top-left, green top-right, blue bottom-left and magenta bottom-right. A correct
  thumbnail keeps them where they are; a flip, a rotation, a red/blue swap, a greyscale, a
  blank or a missing thumbnail all move or lose them. The bottom-right one is not read,
  because the SageThumbs format badge is stamped there; the other three already tell all
  eight flips and rotations apart.

  Each sample is rendered twice:
    * by `st2k.exe thumbnail` (our decoder, what `regression.ps1` sees), and
    * through `IShellItemImageFactory::GetImage` with THUMBNAILONLY on a GUID-named copy - the
      call Explorer, the file dialogs and the wallpaper picker make, served by whichever
      handler Windows really picks, in its own host process. No window opens.
  The decoder can be right while the shell is wrong (another program's handler wins, the
  handler is not registered, the host cannot load it): issue #47 was exactly that, and a
  decoder-only check cannot see it.

  A sample whose OWN decoder render already lacks the corners is not built from the base in a
  way that survives (a document page, a one-bit format, an audio file's generated art); it is
  listed as "not a corner sample" rather than failed, so a genuine decoder regression still
  shows up there as a format that used to pass.

  Runs the INSTALLED build: that is what users have. Needs Windows PowerShell 5.1 (-STA);
  see _shell-surface-probe.ps1 for why not PowerShell 7.

.EXAMPLE
  powershell -NoProfile -STA -File scripts\check-corpus-corners.ps1
  powershell -NoProfile -STA -File scripts\check-corpus-corners.ps1 -Filter 'sample.ps*'
#>
[CmdletBinding()]
param(
    # Default: the test-corpus folder beside the repo (Windows PowerShell 5.1 leaves
    # $PSScriptRoot empty in a parameter default, so it is resolved below).
    [string]$Corpus = '',
    [string]$Exe = 'C:\Program Files\SageThumbs2K\st2k.exe',
    # An independent decoder for the second opinion; the newest ImageMagick in Program Files.
    [string]$Magick = '',
    [string]$Filter = 'sample.*',
    [string]$Out = (Join-Path $env:TEMP 'st2k-corners'),
    [int]$Size = 256
)
$ErrorActionPreference = 'Stop'
if ([Threading.Thread]::CurrentThread.ApartmentState -ne 'STA') { throw 'run under powershell.exe -STA' }

Add-Type -AssemblyName System.Drawing
Add-Type -ReferencedAssemblies System.Drawing @'
using System;
using System.Drawing;
using System.Runtime.InteropServices;

public static class St2kCorners {
    [StructLayout(LayoutKind.Sequential)] public struct SIZE { public int cx, cy; public SIZE(int x, int y) { cx=x; cy=y; } }
    [ComImport, Guid("bcc18b79-ba16-442f-80c4-8a59c30c463b"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IShellItemImageFactory { void GetImage(SIZE size, uint flags, out IntPtr bitmap); }
    [DllImport("shell32.dll", CharSet=CharSet.Unicode, PreserveSig=false)]
    static extern void SHCreateItemFromParsingName(string path, IntPtr bindCtx, ref Guid iid, [MarshalAs(UnmanagedType.Interface)] out IShellItemImageFactory item);
    [DllImport("gdi32.dll")] static extern bool DeleteObject(IntPtr obj);

    // SIIGBF_BIGGERSIZEOK | SIIGBF_THUMBNAILONLY: a real thumbnail or an error, never the
    // file-type icon the shell substitutes otherwise.
    public static Bitmap Shell(string path, int size, out string error) {
        error = null;
        IShellItemImageFactory item = null;
        try {
            Guid iid = typeof(IShellItemImageFactory).GUID;
            SHCreateItemFromParsingName(path, IntPtr.Zero, ref iid, out item);
            IntPtr h; item.GetImage(new SIZE(size, size), 0x1 | 0x8, out h);
            if (h == IntPtr.Zero) { error = "no bitmap"; return null; }
            try { return Image.FromHbitmap(h); } finally { DeleteObject(h); }
        } catch (COMException e) {
            error = "0x" + e.ErrorCode.ToString("x8"); return null;
        } finally { if (item != null) Marshal.ReleaseComObject(item); }
    }

    // Whether a clear patch of (r,g,b) sits in the given quarter of the image (qx, qy = 0 or 1).
    // Looking for the colour anywhere in its quarter, not at one point, keeps a correct render
    // that the decoder padded or letterboxed from reading as wrong, while a flip, a rotation or
    // a channel swap still moves the colour to another quarter. The quarter is inset by an
    // eighth from the centre lines so a colour spilling across the middle does not count.
    public static double Share(Bitmap b, int qx, int qy, int r, int g, int bl) {
        int w = b.Width / 2, h = b.Height / 2, x0 = qx * w, y0 = qy * h, hit = 0, all = 0;
        int ix = qx == 0 ? 0 : w / 8, iy = qy == 0 ? 0 : h / 8;
        int ex = qx == 0 ? w - w / 8 : w, ey = qy == 0 ? h - h / 8 : h;
        for (int y = y0 + iy; y < y0 + ey; y += 2) for (int x = x0 + ix; x < x0 + ex; x += 2) {
            Color c = b.GetPixel(x, y); all++;
            double d = Math.Sqrt(Math.Pow(c.R - r, 2) + Math.Pow(c.G - g, 2) + Math.Pow(c.B - bl, 2));
            if (d <= 80) hit++;
        }
        return all == 0 ? 0 : (double)hit / all;
    }
}
'@

# The three unbadged corner squares of the base and the quarter each belongs in. In the base
# each fills about 13% of its quarter; 1% is enough to say it is there after any scaling.
$probes = @(
    @{ Name = 'top-left red'; QX = 0; QY = 0; R = 255; G = 0; B = 0 },
    @{ Name = 'top-right green'; QX = 1; QY = 0; R = 50; G = 205; B = 50 },
    @{ Name = 'bottom-left blue'; QX = 0; QY = 1; R = 0; G = 0; B = 255 }
)
function Test-Corners([System.Drawing.Bitmap]$bmp) {
    $bad = foreach ($p in $probes) {
        $share = [St2kCorners]::Share($bmp, $p.QX, $p.QY, $p.R, $p.G, $p.B)
        if ($share -lt 0.01) { "no $($p.Name)" }
    }
    [pscustomobject]@{ Ok = (-not $bad); Detail = ($bad -join ', ') }
}

if (-not $Corpus) { $Corpus = Join-Path (Split-Path -Parent $MyInvocation.MyCommand.Path) '..\..\test-corpus' }
if (-not $Magick) {
    # Only the build for this processor: an ARM64 copy beside an x64 one cannot run here.
    $arm = $env:PROCESSOR_ARCHITECTURE -eq 'ARM64'
    $Magick = Get-ChildItem 'C:\Program Files\ImageMagick-*\magick.exe' -ErrorAction SilentlyContinue |
        Where-Object { ($_.DirectoryName -like '*arm64*') -eq $arm } |
        Sort-Object FullName -Descending | Select-Object -First 1 -ExpandProperty FullName
}
$Corpus = (Resolve-Path $Corpus).Path
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$scratch = Join-Path $Out 'copies'
New-Item -ItemType Directory -Force -Path $scratch | Out-Null
$rows = New-Object System.Collections.ArrayList
$samples = Get-ChildItem -Path $Corpus -Filter $Filter -File | Sort-Object Name
foreach ($f in $samples) {
    $ext = $f.Extension
    $row = [ordered]@{ ext = $ext.TrimStart('.'); cli = ''; shell = ''; magick = ''; verdict = ''; tile = $null }

    $cliPng = Join-Path $Out "cli-$($row.ext).png"
    & $Exe thumbnail $f.FullName $cliPng --size $Size *> $null
    $cliBmp = if (Test-Path $cliPng) { New-Object System.Drawing.Bitmap $cliPng } else { $null }
    $cliOk = $false
    if ($cliBmp) { $t = Test-Corners $cliBmp; $cliOk = $t.Ok; $row.cli = if ($t.Ok) { 'ok' } else { $t.Detail } } else { $row.cli = 'no render' }

    $copy = Join-Path $scratch ([guid]::NewGuid().ToString('N') + $ext)
    Copy-Item $f.FullName $copy
    $err = $null
    $shellBmp = [St2kCorners]::Shell($copy, $Size, [ref]$err)
    if ($shellBmp) {
        $shellBmp.Save((Join-Path $Out "shell-$($row.ext).png"), [System.Drawing.Imaging.ImageFormat]::Png)
        $t = Test-Corners $shellBmp; $row.shell = if ($t.Ok) { 'ok' } else { $t.Detail }
    } else { $row.shell = "no thumbnail ($err)" }

    # Our render lacks the corners: ask an independent decoder whether the FILE has them. If it
    # does, the fault is ours, not the sample's.
    $row.magick = ''
    if (-not $cliOk -and $Magick) {
        $mPng = Join-Path $Out "magick-$($row.ext).png"
        $launch = $null
        try { & $Magick "$($f.FullName)[0]" -resize "$($Size)x$($Size)" "png:$mPng" *> $null }
        catch { $launch = $_.Exception.Message }
        if ($launch) {
            # ImageMagick itself did not run (wrong build for this CPU, missing file): that is
            # no opinion at all, and the row says so rather than reading as "cannot read".
            $row.magick = "magick did not run: $launch"
        } elseif (Test-Path $mPng) {
            $mBmp = New-Object System.Drawing.Bitmap $mPng
            $t = Test-Corners $mBmp; $row.magick = if ($t.Ok) { 'ok' } else { $t.Detail }
            $mBmp.Dispose()
        } else { $row.magick = 'cannot read' }
    }

    $row.verdict = if ($cliOk -and $row.shell -eq 'ok') { 'pass' }
    elseif ($cliOk) { 'FAIL' }
    elseif ($row.magick -eq 'ok') { 'DECODER' }
    else { 'not a corner sample' }
    $row.tile = if ($shellBmp) { $shellBmp } else { $cliBmp }
    [void]$rows.Add([pscustomobject]$row)
}
Remove-Item $scratch -Recurse -Force -ErrorAction SilentlyContinue

# One contact sheet: every sample's shell thumbnail (its decoder render when the shell gave
# none), framed green for pass, red for FAIL (the shell), orange for DECODER (ImageMagick sees
# the corners and we do not), grey for not-a-corner-sample.
$tile = 112; $cols = 16; $label = 16
$sheetRows = [Math]::Ceiling($rows.Count / $cols)
$sheet = New-Object System.Drawing.Bitmap ($cols * $tile), ($sheetRows * ($tile + $label))
$g = [System.Drawing.Graphics]::FromImage($sheet)
$g.Clear([System.Drawing.Color]::FromArgb(32, 32, 32))
$font = New-Object System.Drawing.Font 'Segoe UI', 8
for ($i = 0; $i -lt $rows.Count; $i++) {
    $r = $rows[$i]; $x = ($i % $cols) * $tile; $y = [Math]::Floor($i / $cols) * ($tile + $label)
    $frame = switch ($r.verdict) { 'pass' { 'LimeGreen' } 'FAIL' { 'Red' } 'DECODER' { 'Orange' } default { 'Gray' } }
    $g.FillRectangle((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromName($frame))), $x, $y, $tile, $tile + $label)
    if ($r.tile) {
        $s = [Math]::Min(($tile - 8) / $r.tile.Width, ($tile - 8) / $r.tile.Height)
        $w = [int]($r.tile.Width * $s); $h = [int]($r.tile.Height * $s)
        $g.DrawImage($r.tile, $x + [int](($tile - $w) / 2), $y + [int](($tile - $h) / 2), $w, $h)
    }
    $g.DrawString($r.ext, $font, [System.Drawing.Brushes]::Black, $x + 3, $y + $tile)
}
$sheetPath = Join-Path $Out 'corners-contact.png'
$sheet.Save($sheetPath, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $sheet.Dispose()

$rows | Select-Object ext, verdict, cli, shell, magick | Export-Csv (Join-Path $Out 'corners.csv') -NoTypeInformation
$fail = @($rows | Where-Object { $_.verdict -eq 'FAIL' -or $_.verdict -eq 'DECODER' })
$skip = @($rows | Where-Object verdict -eq 'not a corner sample')
"corners: $($rows.Count) samples, $(@($rows | Where-Object verdict -eq 'pass').Count) pass, $($fail.Count) failing, $($skip.Count) not corner samples"
foreach ($r in $fail) { "  {0,-7} {1,-10} ours: {2}  shell: {3}" -f $r.verdict, $r.ext, $r.cli, $r.shell }
"sheet: $sheetPath"
if ($fail.Count) { exit 1 }
