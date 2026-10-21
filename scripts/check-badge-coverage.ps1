<#
.SYNOPSIS
    Proves every registered format shows the format badge in Explorer, end to end.

.DESCRIPTION
    Issue #66 was one format (PNG) drawn without the badge on one PC. Looking for the rest
    by reading code found some; this measures all of them. For each extension `st2k formats`
    lists, one corpus sample (its `real.<ext>` pin when there is one) is copied fresh and
    thumbnailed the way Explorer's view asks, through the thumbnail cache with a forced,
    uncached extraction, once with the badge on and once with it off. The badge is what
    differs, and it has to sit in the bottom-right corner. Before the badge-on pass the app's
    `--sync-user-shell` runs, so the ProgID slots it takes (issue #66) are in place, as they
    are on a user's PC.

    Why the thumbnail cache and not IShellItemImageFactory: measured 2026-10-21, the factory's
    first request for some camera RAW files (a NEF, an RW2, a DNG) fails at once with
    0x80070490 without ever creating our provider, and a later request is served by whichever
    extractor filled the cache meanwhile. The cache with WTS_FORCEEXTRACTION calls the
    registered provider every time, as Explorer's view does.

.NOTES
    Measures the INSTALLED provider: run it after an install when you touch the badge, the
    thumbnail provider, the ProgID slots, the corner-mark setting or the format list. It writes
    CornerMark in HKCU (and HKLM with -Via setup) and runs `--sync-user-shell`, and puts them
    back as it found them; scratch files go under %TEMP%. Not a CI gate: it needs the shell
    extension registered on this machine.

    Exit 0 when every format with a sample is badged. A format with no corpus sample is listed,
    not failed (`_no-real-sample.txt` is that list's owner).

.EXAMPLE
    pwsh scripts\check-badge-coverage.ps1
    pwsh scripts\check-badge-coverage.ps1 -Only png,psd,aseprite
    pwsh scripts\check-badge-coverage.ps1 -Via setup -Only png,jpg,psd
#>
[CmdletBinding()]
param(
    # The app whose `--sync-user-shell` takes the slots. Defaults to the installed one.
    [string]$App = "$env:ProgramFiles\SageThumbs2K\SageThumbs2K.exe",
    # The CLI that lists the formats.
    [string]$St2k = "$env:ProgramFiles\SageThumbs2K\st2k.exe",
    [string]$Corpus = (Join-Path $PSScriptRoot '..\..\test-corpus'),
    # A comma-separated subset of extensions; all of them when empty.
    [string]$Only = '',
    # Where the badge-on choice is written: 'settings' is the user's own value (HKCU), 'setup'
    # the machine-wide one setup's Thumbnail corner page writes (HKLM, so this elevates) with
    # the user's own value removed. Thumbnails ignored the setup one until 2026-10-21.
    [ValidateSet('settings', 'setup')][string]$Via = 'settings',
    [string]$Out = (Join-Path $env:TEMP "st2k-badge-coverage-$PID")
)
$ErrorActionPreference = 'Stop'

# The C# below needs .NET Framework's System.Drawing; pwsh 7 forwards those types elsewhere.
if ($PSVersionTable.PSEdition -eq 'Core') {
    $argv = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $PSCommandPath,
        '-App', $App, '-St2k', $St2k, '-Corpus', $Corpus, '-Out', $Out, '-Via', $Via)
    if ($Only) { $argv += @('-Only', $Only) }
    & powershell.exe @argv
    exit $LASTEXITCODE
}

Add-Type -AssemblyName System.Drawing
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;
using System.Threading;
public static class BadgeCoverage {
  [StructLayout(LayoutKind.Sequential)] struct BITMAP { public int bmType, bmWidth, bmHeight, bmWidthBytes; public ushort bmPlanes, bmBitsPixel; public IntPtr bmBits; }
  [StructLayout(LayoutKind.Sequential)] struct BITMAPINFOHEADER { public int biSize, biWidth, biHeight; public ushort biPlanes, biBitCount; public int biCompression, biSizeImage, biXPelsPerMeter, biYPelsPerMeter, biClrUsed, biClrImportant; }
  [StructLayout(LayoutKind.Sequential)] public struct SIZE { public int cx; public int cy; }
  [ComImport, Guid("091162a4-bc96-411f-aae8-c5122cd03363"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  interface ISharedBitmap { [PreserveSig] int GetSharedBitmap(out IntPtr h); [PreserveSig] int GetSize(out SIZE s); [PreserveSig] int GetFormat(out int a); [PreserveSig] int InitializeBitmap(IntPtr h, int a); [PreserveSig] int Detach(out IntPtr h); }
  [ComImport, Guid("F676C15D-596A-4ce2-8234-33996F445DB1"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  interface IThumbnailCache { [PreserveSig] int GetThumbnail(IntPtr item, uint cx, int flags, out ISharedBitmap bmp, out int outFlags, IntPtr id); }
  [DllImport("shell32.dll", CharSet = CharSet.Unicode, PreserveSig = false)]
  static extern void SHCreateItemFromParsingName(string path, IntPtr pbc, [MarshalAs(UnmanagedType.LPStruct)] Guid riid, out IntPtr ppv);
  [DllImport("gdi32.dll")] static extern int GetObject(IntPtr h, int c, out BITMAP bm);
  [DllImport("gdi32.dll")] static extern int GetDIBits(IntPtr hdc, IntPtr hbm, uint start, uint lines, byte[] bits, ref BITMAPINFOHEADER bi, uint usage);
  [DllImport("user32.dll")] static extern IntPtr GetDC(IntPtr w);
  [DllImport("user32.dll")] static extern int ReleaseDC(IntPtr w, IntPtr dc);
  [DllImport("shell32.dll")] public static extern void SHChangeNotify(int e, int f, IntPtr a, IntPtr b);

  // `file` thumbnailed at `edge` through the thumbnail cache (WTS_FORCEEXTRACTION |
  // WTS_EXTRACTDONOTCACHE), saved as a straight-alpha PNG. "ok WxH" or what failed.
  public static string Render(string file, string png, int edge, int timeoutMs) {
    string result = "timeout";
    var t = new Thread(() => {
      IntPtr item = IntPtr.Zero;
      try {
        SHCreateItemFromParsingName(file, IntPtr.Zero, new Guid("43826d1e-e718-42ee-bc55-a1e261c37bfe"), out item);
        var cache = (IThumbnailCache)Activator.CreateInstance(Type.GetTypeFromCLSID(new Guid("50EF4544-AC9F-4A8E-B21B-8A26180DB13F")));
        ISharedBitmap sb; int of;
        int hr = cache.GetThumbnail(item, (uint)edge, 0x4 | 0x20, out sb, out of, IntPtr.Zero);
        if (hr != 0) { result = "hr=0x" + hr.ToString("x8"); return; }
        IntPtr h; sb.GetSharedBitmap(out h);
        result = Save(h, png);
        Marshal.ReleaseComObject(sb);
      } catch (Exception e) {
        result = "error " + e.GetType().Name + ": " + e.Message.Split('\n')[0].Trim();
      } finally { if (item != IntPtr.Zero) Marshal.Release(item); }
    });
    t.SetApartmentState(ApartmentState.STA); t.IsBackground = true; t.Start();
    t.Join(timeoutMs);
    return result;
  }

  static string Save(IntPtr h, string png) {
    BITMAP bm; GetObject(h, Marshal.SizeOf(typeof(BITMAP)), out bm);
    int w = bm.bmWidth, hh = Math.Abs(bm.bmHeight);
    var bi = new BITMAPINFOHEADER { biSize = Marshal.SizeOf(typeof(BITMAPINFOHEADER)), biWidth = w, biHeight = -hh, biPlanes = 1, biBitCount = 32 };
    var px = new byte[w * hh * 4];
    IntPtr dc = GetDC(IntPtr.Zero);
    GetDIBits(dc, h, 0, (uint)hh, px, ref bi, 0);
    ReleaseDC(IntPtr.Zero, dc);
    // Premultiplied BGRA from the shell; un-premultiply so the PNG views right. A bitmap whose
    // alpha bytes are all zero is an opaque one.
    bool anyAlpha = false;
    for (int i = 3; i < px.Length; i += 4) if (px[i] != 0) { anyAlpha = true; break; }
    for (int i = 0; i < px.Length; i += 4) {
      if (!anyAlpha) { px[i + 3] = 255; continue; }
      int a = px[i + 3]; if (a == 0 || a == 255) continue;
      for (int c = 0; c < 3; c++) px[i + c] = (byte)Math.Min(255, px[i + c] * 255 / a);
    }
    using (var b = new Bitmap(w, hh, PixelFormat.Format32bppArgb)) {
      var d = b.LockBits(new Rectangle(0, 0, w, hh), ImageLockMode.WriteOnly, PixelFormat.Format32bppArgb);
      for (int y = 0; y < hh; y++) Marshal.Copy(px, y * w * 4, d.Scan0 + y * d.Stride, w * 4);
      b.UnlockBits(d);
      b.Save(png, ImageFormat.Png);
    }
    return "ok " + w + "x" + hh;
  }

  static byte[] Load(string png, out int w, out int h) {
    using (var b = new Bitmap(png)) {
      w = b.Width; h = b.Height;
      var d = b.LockBits(new Rectangle(0, 0, w, h), ImageLockMode.ReadOnly, PixelFormat.Format32bppArgb);
      var px = new byte[w * h * 4];
      for (int y = 0; y < h; y++) Marshal.Copy(d.Scan0 + y * d.Stride, px, y * w * 4, w * 4);
      b.UnlockBits(d);
      return px;
    }
  }

  // Where the badge-on tile `on` differs from the badge-off tile `off`: "WxH n=N box=x0,y0,x1,y1"
  // (n=0 and no box when they match). A smaller `off` is compared centred in `on`, which is how
  // a picture too small for the badge comes back (centred on a tile of the requested size).
  public static string Diff(string on, string off) {
    int w, h, wb, hb;
    var pa = Load(on, out w, out h); var pb = Load(off, out wb, out hb);
    if (wb > w || hb > h) return "size " + w + "x" + h + " vs " + wb + "x" + hb;
    int ox = (w - wb) / 2, oy = (h - hb) / 2;
    int n = 0, x0 = w, y0 = h, x1 = -1, y1 = -1;
    for (int y = 0; y < h; y++) for (int x = 0; x < w; x++) {
      int i = (y * w + x) * 4, s = 0;
      int bx = x - ox, by = y - oy;
      bool inside = bx >= 0 && by >= 0 && bx < wb && by < hb;
      for (int c = 0; c < 4; c++) s += Math.Abs(pa[i + c] - (inside ? pb[(by * wb + bx) * 4 + c] : 0));
      if (s <= 48) continue;
      n++; x0 = Math.Min(x0, x); y0 = Math.Min(y0, y); x1 = Math.Max(x1, x); y1 = Math.Max(y1, y);
    }
    return w + "x" + h + " n=" + n + (n > 0 ? " box=" + x0 + "," + y0 + "," + x1 + "," + y1 : "");
  }
}
'@

# A badge is at least 30 changed pixels hugging the bottom-right corner, covering well under
# the whole tile.
function Verdict([string]$diff) {
    if ($diff -notmatch '^(\d+)x(\d+) n=(\d+)(?: box=(\d+),(\d+),(\d+),(\d+))?') { return 'unclear' }
    $w = [int]$Matches[1]; $h = [int]$Matches[2]; $n = [int]$Matches[3]
    if ($n -eq 0) { return 'NO-BADGE' }
    $bw = [int]$Matches[6] - [int]$Matches[4] + 1; $bh = [int]$Matches[7] - [int]$Matches[5] + 1
    $hugs = [int]$Matches[6] -ge 0.8 * $w -and [int]$Matches[7] -ge 0.8 * $h
    if ($n -ge 30 -and $hugs -and $bw * $bh -le 0.6 * $w * $h) { 'badged' } else { 'unclear' }
}

function Sync { $p = Start-Process $App -ArgumentList '--sync-user-shell' -WindowStyle Hidden -Wait -PassThru; "  --sync-user-shell exit=$($p.ExitCode)" }

$formats = & $St2k formats --json | Out-String | ConvertFrom-Json
if ($Only) { $formats = $formats | Where-Object { ($Only -split ',') -contains $_.ext } }
$expectedFail = @{}
Get-Content (Join-Path $Corpus '_expected-fail.txt') | Where-Object { $_ -and $_ -notmatch '^\s*#' } |
    ForEach-Object { $expectedFail[($_ -replace '^size:', '').Trim()] = 1 }
$files = Get-ChildItem $Corpus -Recurse -File | Where-Object {
    $_.FullName -notmatch '\\(_render|scratch|_rawcheck|New folder)\\' -and -not $expectedFail[$_.Name] -and $_.Length -lt 60MB
}
$byExt = $files | Group-Object { $_.Extension.TrimStart('.').ToLowerInvariant() } -AsHashTable -AsString
$negative = 'bad|corrupt|trunc|broken|fail|encrypt|empty|noimg|zero|invalid|malformed|evil|bomb|huge|tiny|garbage|neg'
$plan = foreach ($f in $formats) {
    $c = $byExt[$f.ext]; $pick = $null
    if ($c) {
        $pick = $c | Where-Object { $_.Name -ieq "real.$($f.ext)" } | Select-Object -First 1
        if (-not $pick) { $pick = $c | Where-Object { $_.BaseName -notmatch $negative } | Sort-Object Length -Descending | Select-Object -First 1 }
        if (-not $pick) { $pick = $c | Sort-Object Length -Descending | Select-Object -First 1 }
    }
    [pscustomobject]@{ ext = $f.ext; sample = $pick; on = ''; off = ''; diff = ''; verdict = '' }
}
foreach ($d in 'on', 'off') { New-Item -ItemType Directory -Force (Join-Path $Out $d) | Out-Null }
$stamp = [DateTime]::Now.ToString('HHmmss')

$key = 'HKCU:\Software\SageThumbs2K'
$old = (Get-ItemProperty $key -ErrorAction SilentlyContinue).CornerMark
$oldMachine = (Get-ItemProperty 'HKLM:\Software\SageThumbs2K' -ErrorAction SilentlyContinue).CornerMark
# Write (or with $null remove) the machine-wide CornerMark, elevated.
function Set-MachineMark($v) {
    $argv = if ($null -eq $v) { 'delete HKLM\Software\SageThumbs2K /v CornerMark /f' }
    else { "add HKLM\Software\SageThumbs2K /v CornerMark /t REG_DWORD /d $v /f" }
    Start-Process reg.exe -ArgumentList $argv -Verb RunAs -WindowStyle Hidden -Wait
}
try {
    "formats: $($plan.Count), with a sample: $(@($plan | Where-Object sample).Count); badge on via $Via; scratch: $Out"
    if ($Via -eq 'setup') {
        Remove-ItemProperty $key -Name CornerMark -ErrorAction SilentlyContinue
        Set-MachineMark 1
    } else { Set-ItemProperty $key -Name CornerMark -Value 1 -Type DWord }
    Sync
    [BadgeCoverage]::SHChangeNotify(0x08000000, 0, [IntPtr]::Zero, [IntPtr]::Zero)
    foreach ($pass in 'on', 'off') {
        if ($pass -eq 'off') { Set-ItemProperty $key -Name CornerMark -Value 0 -Type DWord }
        $i = 0
        foreach ($p in $plan) {
            $i++
            if (-not $p.sample) { continue }
            $copy = Join-Path $Out "$pass\$i-$stamp.$($p.ext)"
            Copy-Item -LiteralPath $p.sample.FullName $copy
            $p.$pass = [BadgeCoverage]::Render($copy, "$copy.png", 256, 90000)
        }
    }
    $i = 0
    foreach ($p in $plan) {
        $i++
        if (-not $p.sample) { $p.verdict = 'no-sample'; continue }
        if ($p.on -notlike 'ok*') { $p.verdict = 'NO-THUMB'; continue }
        if ($p.off -notlike 'ok*') { $p.verdict = 'unclear'; continue }
        $p.diff = [BadgeCoverage]::Diff((Join-Path $Out "on\$i-$stamp.$($p.ext).png"), (Join-Path $Out "off\$i-$stamp.$($p.ext).png"))
        $p.verdict = Verdict $p.diff
    }
} finally {
    if ($null -eq $old) { Remove-ItemProperty $key -Name CornerMark -ErrorAction SilentlyContinue }
    else { Set-ItemProperty $key -Name CornerMark -Value $old -Type DWord }
    if ($Via -eq 'setup') { Set-MachineMark $oldMachine }
    Sync
    [BadgeCoverage]::SHChangeNotify(0x08000000, 0, [IntPtr]::Zero, [IntPtr]::Zero)
}

$plan | Select-Object ext, @{ n = 'sample'; e = { $_.sample.Name } }, on, off, diff, verdict |
    ConvertTo-Json -Depth 2 | Set-Content (Join-Path $Out 'results.json') -Encoding UTF8
$plan | Group-Object verdict | Sort-Object Name | ForEach-Object { "{0,-10} {1}" -f $_.Name, $_.Count }
$bad = @($plan | Where-Object { $_.verdict -notin 'badged', 'no-sample' })
foreach ($b in $bad) { "  {0,-12} {1,-9} {2} | {3} | {4}" -f $b.ext, $b.verdict, $b.sample.Name, $b.on, $b.diff }
$missing = @($plan | Where-Object verdict -eq 'no-sample' | ForEach-Object ext)
if ($missing) { "no sample: $($missing -join ' ')" }
if ($bad) { exit 1 }
"every format with a sample is badged"
