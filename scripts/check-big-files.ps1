<#
.SYNOPSIS
  Every format in the corpus, past the 256 MiB input ceiling: does the installed SageThumbs
  still give it a thumbnail?

.DESCRIPTION
  The tests prove each format on one small sample, so nothing proved what a file too big to
  hold gets, and that is where layered art, scanned books and album rips live. This takes one
  sample of every extension in the corpus, makes a sparse copy padded to 300 MB (no disk is
  used), and asks the Windows shell for both thumbnails exactly as Explorer does
  (IShellItemImageFactory, thumbnail only). A format whose small copy gets one and whose big
  copy does not fails the check, unless it is listed below with its reason.

  What it proves: no format refuses a file for its size alone, and every preview in a file's
  head survives any size. What it cannot: padding goes after the file's own bytes, so a format
  that keeps a structure at its END (an index, a tag, a preview) loses it to the padding. Those
  are listed, each with the test or the real file that proves the real layout; the two such
  layouts fixed on 2026-10-10 (an Affinity preview at the end, a DjVu book whose header
  declares the whole book) are pinned by unit tests in the codecs crate, not by this.

  Some RAW files never reach our handler through that route: with Microsoft's Raw Image
  Extension installed, the thumbnail cache's photo fast path takes their embedded preview
  itself and answers the first request 0x80070490 (docs/DEVELOPMENT_GOTCHAS.md, "Some RAW files
  are thumbnailed by WINDOWS"). Such a file is asked again through IThumbnailCache with
  WTS_EXTRACTDONOTCACHE, which skips the fast path, so our handler is still checked on it.

  Runs against the INSTALLED build: install the build being checked first. Needs the test
  corpus (`..\test-corpus`); skips without it.

.PARAMETER Corpus
  The test corpus folder. Default: ..\test-corpus beside the repo.
#>
param(
    [string]$Corpus = (Join-Path $PSScriptRoot '..\..\test-corpus'),
    [long]$BigBytes = 300MB
)
$ErrorActionPreference = 'Stop'

# Extension -> why its big copy may fail. Keep each reason true: a listed format that passes is
# reported so the line can go.
$expected = [ordered]@{
    cal  = 'CALS runs to the end of the file: a big one is all raster, which no partial read draws'
    cals = 'CALS runs to the end of the file: a big one is all raster, which no partial read draws'
    pix  = 'Alias PIX runs end exactly at the end of the file: a big one is all raster'
    svg  = 'one XML document: a cut one does not parse, and a 256 MB SVG is not drawable in time'
    mdc  = 'Minolta RAW from 2000, about 1 MB; never this size, and its reader needs the whole file'
    pdf  = 'cross-reference at the end, hidden by the padding; real big PDFs: previews_found_by_offset_or_index_survive_the_ceiling'
    ai   = 'a PDF inside, cross-reference at the end, hidden by the padding (as pdf)'
    zip  = 'central directory at the end, hidden by the padding; real big archives: the streamed archive tests'
    ape  = 'cover art in the APE tag at the end, hidden by the padding; a gap before the tag passes'
    wv   = 'cover art in the APE tag at the end, hidden by the padding; a gap before the tag passes'
    mpc  = 'cover art in the APE tag at the end, hidden by the padding; a gap before the tag passes'
    ogv  = 'the frame is sought by time from the last page, hidden by the padding; a real 290 MB Theora video passes'
}

if (-not (Test-Path -LiteralPath $Corpus -PathType Container)) {
    Write-Host "check-big-files: no corpus at $Corpus - skipped" -ForegroundColor Yellow
    exit 0
}
$Corpus = (Resolve-Path -LiteralPath $Corpus).Path

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class St2kBigFileThumb {
    [ComImport, Guid("bcc18b79-ba16-442f-80c4-8a59c30c463b"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IShellItemImageFactory { [PreserveSig] int GetImage(SIZE size, int flags, out IntPtr phbm); }
    [StructLayout(LayoutKind.Sequential)] struct SIZE { public int cx; public int cy; }
    [DllImport("shell32.dll", CharSet = CharSet.Unicode, PreserveSig = false)]
    static extern void SHCreateItemFromParsingName(string path, IntPtr pbc, [MarshalAs(UnmanagedType.LPStruct)] Guid riid, [MarshalAs(UnmanagedType.Interface)] out IShellItemImageFactory ppv);
    [DllImport("gdi32.dll")] static extern bool DeleteObject(IntPtr h);
    // 0 when the shell drew a thumbnail (SIIGBF_THUMBNAILONLY: never the icon), else the HRESULT.
    public static int Get(string path) {
        IShellItemImageFactory f;
        SHCreateItemFromParsingName(path, IntPtr.Zero, typeof(IShellItemImageFactory).GUID, out f);
        IntPtr hbm;
        int hr = f.GetImage(new SIZE { cx = 256, cy = 256 }, 0x08, out hbm);
        if (hr == 0) DeleteObject(hbm);
        return hr;
    }

    [ComImport, Guid("43826d1e-e718-42ee-bc55-a1e261c37bfe"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IShellItem { }
    [ComImport, Guid("091162a4-bc96-411f-aae8-c5122cd03363"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface ISharedBitmap { }
    [ComImport, Guid("F676C15D-596A-4ce2-8234-33996F445DB1"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IThumbnailCache {
        [PreserveSig] int GetThumbnail(IShellItem psi, uint cx, int flags, out ISharedBitmap ppvThumb, out int pOutFlags, IntPtr pThumbnailID);
    }
    [DllImport("shell32.dll", CharSet = CharSet.Unicode, PreserveSig = false, EntryPoint = "SHCreateItemFromParsingName")]
    static extern void CreateItem(string path, IntPtr pbc, [MarshalAs(UnmanagedType.LPStruct)] Guid riid, [MarshalAs(UnmanagedType.Interface)] out IShellItem ppv);
    // The same request past the thumbnail cache's photo fast path (WTS_EXTRACTDONOTCACHE), so
    // the registered handler answers it: 0 when it drew a thumbnail, else the HRESULT.
    public static int GetPastFastPath(string path) {
        IShellItem si;
        CreateItem(path, IntPtr.Zero, typeof(IShellItem).GUID, out si);
        var cache = (IThumbnailCache)Activator.CreateInstance(Type.GetTypeFromCLSID(new Guid("50EF4544-AC9F-4A8E-B21B-8A26180DB13F")));
        ISharedBitmap bmp; int outFlags;
        return cache.GetThumbnail(si, 256, 0x20, out bmp, out outFlags, IntPtr.Zero);
    }
}
"@

$elementNotFound = [int]0x80070490   # the photo fast path took the file and our handler was never asked
$windowsDrawn = [System.Collections.Generic.SortedSet[string]]::new()
function Get-Thumb([string]$Path) {
    $hr = try { [St2kBigFileThumb]::Get($Path) } catch { $_.Exception.HResult }
    if ($hr -ne $elementNotFound) { return $hr }
    [void]$windowsDrawn.Add([IO.Path]::GetExtension($Path).TrimStart('.').ToLowerInvariant())
    try { [St2kBigFileThumb]::GetPastFastPath($Path) } catch { $_.Exception.HResult }
}

# One sample per extension: real.* first, then sample.*, then the rest by name.
$byExt = [ordered]@{}
Get-ChildItem -LiteralPath $Corpus -File |
    Where-Object { $_.Extension -and $_.Name -notlike '_*' -and $_.Extension -notin '.md', '.txt' } |
    Sort-Object { switch ($_.BaseName) { 'real' { 0 } 'sample' { 1 } default { 2 } } }, Name |
    ForEach-Object {
        $e = $_.Extension.TrimStart('.').ToLowerInvariant()
        if (-not $byExt.Contains($e)) { $byExt[$e] = $_ }
    }

$work = Join-Path ([IO.Path]::GetTempPath()) "st2k-big-files-$PID"
New-Item -ItemType Directory -Force -Path $work | Out-Null
$failed = @(); $passedListed = @(); $smallFailed = @(); $ok = 0
try {
    foreach ($e in $byExt.Keys) {
        $src = $byExt[$e]
        $small = Join-Path $work "small-$($src.Name)"
        $big = Join-Path $work "big-$($src.Name)"
        Copy-Item -LiteralPath $src.FullName -Destination $small
        Copy-Item -LiteralPath $src.FullName -Destination $big
        fsutil sparse setflag $big | Out-Null
        $fs = [IO.File]::Open($big, 'Open', 'ReadWrite')
        try { $fs.SetLength($BigBytes) } finally { $fs.Close() }

        if ((Get-Thumb $small) -ne 0) { $smallFailed += $e; continue }
        $hr = Get-Thumb $big
        if ($hr -eq 0) {
            $ok++
            if ($expected.Contains($e)) { $passedListed += $e }
        } elseif (-not $expected.Contains($e)) {
            $failed += ('{0} ({1}): 0x{2:X8}' -f $e, $src.Name, $hr)
        }
    }
} finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ("check-big-files: {0} formats; {1} keep their thumbnail at {2} MB; {3} listed; {4} with no small thumbnail ({5})" -f `
        $byExt.Count, $ok, [int]($BigBytes / 1MB), $expected.Count, $smallFailed.Count, ($smallFailed -join ', '))
if ($windowsDrawn.Count) {
    Write-Host ("  drawn by Windows' own RAW codec in Explorer, checked past it: {0}" -f ($windowsDrawn -join ', '))
}
foreach ($e in $passedListed) {
    Write-Host "  $e passes now: drop it from the list in check-big-files.ps1" -ForegroundColor Yellow
}
if ($failed.Count) {
    Write-Host "check-big-files: $($failed.Count) format(s) lose their thumbnail past the input ceiling:" -ForegroundColor Red
    $failed | ForEach-Object { Write-Host "  $_" -ForegroundColor Red }
    exit 1
}
Write-Host 'check-big-files: clean' -ForegroundColor Green
