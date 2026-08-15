<#
.SYNOPSIS
  Prove the INSTALLED SageThumbs 2K thumbnails the formats 2.0 added, inside Explorer's own
  out-of-process host, with the decoder helper process spawning correctly.

.DESCRIPTION
  Everything else in the 2.0 gate proves the DECODE: `st2k.exe` and the corpus run the same
  Rust, so a green regression says the bytes are read correctly. None of it exercises the
  HOST. In Explorer the same code runs inside `dllhost.exe` — a different process, a different
  working directory, a different token, a different session from the console this repo is
  developed in — and the 2.0 video tiers SPAWN A CHILD PROCESS from in there. That spawn is
  the part no headless test had ever performed, so it is what this script exists for.

  Four things are asserted that only a live run can answer:

    1. The installed DLL really is the built one (hash), so everything below is about the
       code just written rather than whatever was installed last week.
    2. Explorer's thumbnail host loads it. A `dllhost.exe` with `sagethumbs2k.dll` mapped is
       the proof the shell chose our handler AND that it is process-isolated, which is the
       whole reason a panicky decoder is tolerable at all.
    3. `st2k.exe` spawns AS A CHILD OF THAT HOST for the out-of-process codecs. The path is
       resolved from the DLL's own module handle, not the working directory — this is where
       that would break if it were ever "simplified" to a relative path, and a console-only
       test could never notice because there the two happen to coincide.
    4. No `st2k.exe` ever owns a visible window. It is a CONSOLE-subsystem binary, so without
       CREATE_NO_WINDOW every FLV or VP9 tile would flash a console window on the user's
       desktop. That is invisible to every automated check that is not looking for it, and
       intensely visible to a user scrolling a folder of videos.

  Thumbnail-cache defeat is structural, not a cache flush: every sample is copied to a
  GUID-named file, so Windows has no cache entry to reuse and MUST call the handler. That is
  more reliable than deleting `thumbcache_*.db`, which the running Explorer holds open.

  It closes only the Explorer window it opened, and leaves no registry or setting changed.

.EXAMPLE
  pwsh -NoProfile -File scripts\verify-installed-thumbnails-explorer.ps1
  pwsh -NoProfile -File scripts\verify-installed-thumbnails-explorer.ps1 -Keep
#>
[CmdletBinding()]
param(
    # Corpus directory holding the samples. Defaults to the repo's sibling test-corpus.
    [string]$CorpusPath,
    # Directory holding the freshly built DLL, for the installed==built hash assertion.
    [string]$BuiltDir = 'D:\st2k-target\release',
    # Keep the scratch directory (and its captures) for inspection.
    [switch]$Keep
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path $PSScriptRoot -Parent
if (-not $CorpusPath) { $CorpusPath = Join-Path (Split-Path $root -Parent) 'test-corpus' }
$CorpusPath = (Resolve-Path -LiteralPath $CorpusPath).Path
$installedDll = Join-Path $env:ProgramFiles 'SageThumbs2K\sagethumbs2k.dll'
$installedExe = Join-Path $env:ProgramFiles 'SageThumbs2K\st2k.exe'

# The samples that matter, why each is here, and how many helper processes ONE thumbnail of
# it must cost.
#
# `Helpers` is an assertion in both directions, and the zeros are the load-bearing half. A
# tier that quietly stopped working in-process would still produce a correct picture — by
# spawning a process per file instead. Correct output, silently worse behaviour, invisible to
# any check that only looks at the tile. Explorer draws folders of these at a time, so "VP9
# Profile 0 costs no process" is a real user-facing property, not trivia.
#
# `sample.flv` is SORENSON, not H.264 — `flv.rs::corpus_sorenson_flv_declines_the_mp4_remux_path`
# pins that (codec id 2). It therefore costs a helper like the other Flash codecs. The real
# H.264 FLV is `sample-h264.flv`, and its expected cost of ZERO is the sharpest assertion in
# this table: it is the commonest FLV and the codec behind the original report, so a change
# that pushed it onto the helper path would be a per-file process for the most likely file.
$cases = @(
    @{ Name = 'sample.apk';        Helpers = 0; Why = 'Android binary XML + resources.arsc, parsed IN-PROCESS in the host' }
    @{ Name = 'sample.xapk';       Helpers = 0; Why = 'split-bundle wrapper: zip inside a zip, in-process' }
    @{ Name = 'sample-vp6.flv';    Helpers = 1; Why = 'VP6 via nihav in a spawned st2k child' }
    @{ Name = 'sample.flv';        Helpers = 1; Why = 'Sorenson Spark via h263-rs in a spawned st2k child' }
    @{ Name = 'sample-h264.flv';   Helpers = 0; Why = 'H.264 FLV: remuxed in-process, decoded by Media Foundation' }
    @{ Name = 'sample-vp9p2.webm'; Helpers = 1; Why = 'VP9 Profile 2 via vp9dec in a spawned st2k child' }
    @{ Name = 'sample-vp9p3.webm'; Helpers = 1; Why = 'VP9 Profile 3 via vp9dec in a spawned st2k child' }
    @{ Name = 'sample.webm';       Helpers = 0; Why = 'VP9 Profile 0: must stay on the in-process Media Foundation path' }
    @{ Name = 'sample.mp4';        Helpers = 0; Why = 'H.264: the commonest video of all, must never leave the process' }
    # A STILL that must never be mistaken for a video. libheif writes `mif3` as this file's
    # major ftyp brand; that brand was missing from `video::is_video_magic`'s still list, so
    # the shell cascade classified an image as video, decoded no frame, and STOPPED instead of
    # falling through to the image tiers. Explorer showed the stock icon while `st2k thumbnail`
    # rendered it perfectly, which is exactly the class of fault only a through-the-shell check
    # can catch. Helpers = 0: nothing about a still image should ever spawn a child.
    @{ Name = 'sample-avif-alpha.avif'; Helpers = 0; Why = 'AVIF whose ftyp brand (mif3) once sniffed as video' }
)

# ---------------------------------------------------------------------------------------------
# Native helpers. Process enumeration is Toolhelp32 rather than Get-CimInstance because the
# decoder children live for a few hundred milliseconds: a CIM query costs more than that, so
# it would miss the very thing being looked for and report a clean "no child ever spawned".
# ---------------------------------------------------------------------------------------------
if (-not ('St2kExplorerProbe' -as [type])) {
    # No System.Drawing: PowerShell 7 does not carry it, and this needs to run under whatever
    # pwsh the machine has. GDI reads the bitmap directly, which is what System.Drawing would
    # have done anyway, and the proof file is written as a BMP (a 54-byte header plus the
    # pixels GetDIBits already handed us) rather than pulling in an imaging stack to encode PNG.
    Add-Type @'
using System;
using System.Collections.Generic;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;

public static class St2kExplorerProbe {
    [StructLayout(LayoutKind.Sequential)] public struct SIZE { public int cx, cy; public SIZE(int x,int y){cx=x;cy=y;} }

    [ComImport, Guid("bcc18b79-ba16-442f-80c4-8a59c30c463b"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IShellItemImageFactory { void GetImage(SIZE size, uint flags, out IntPtr bitmap); }
    [DllImport("shell32.dll", CharSet=CharSet.Unicode, PreserveSig=false)]
    static extern void SHCreateItemFromParsingName(string path, IntPtr bc, ref Guid iid,
        [MarshalAs(UnmanagedType.Interface)] out IShellItemImageFactory item);
    [DllImport("gdi32.dll")] static extern bool DeleteObject(IntPtr o);
    [DllImport("gdi32.dll")] static extern int GetObjectW(IntPtr h, int n, IntPtr p);
    [DllImport("gdi32.dll")] static extern int GetDIBits(IntPtr dc, IntPtr bmp, uint start, uint lines, byte[] bits, byte[] info, uint usage);
    [DllImport("user32.dll")] static extern IntPtr GetDC(IntPtr hwnd);
    [DllImport("user32.dll")] static extern int ReleaseDC(IntPtr hwnd, IntPtr dc);

    [StructLayout(LayoutKind.Sequential)]
    struct BITMAP { public int bmType, bmWidth, bmHeight, bmWidthBytes; public ushort bmPlanes, bmBitsPixel; public IntPtr bmBits; }

    // SIIGBF_THUMBNAILONLY (0x08) is load-bearing: without it the shell substitutes a generic
    // file-type ICON and every assertion below would pass while the user sees nothing.
    // SIIGBF_INCACHEONLY is deliberately NOT set — we want the handler invoked for real.
    const uint SIIGBF_THUMBNAILONLY = 0x08;

    /// Ask the shell for a thumbnail and report {width, height, distinct colours}. The colour
    /// count is the real assertion: a blank tile still returns a perfectly valid HBITMAP, so
    /// "did we get a bitmap" is not the same question as "did we get a picture".
    public static int[] Thumbnail(string input, string output, int edge) {
        Guid iid = typeof(IShellItemImageFactory).GUID;
        IShellItemImageFactory item;
        SHCreateItemFromParsingName(input, IntPtr.Zero, ref iid, out item);
        IntPtr h;
        try { item.GetImage(new SIZE(edge, edge), SIIGBF_THUMBNAILONLY, out h); }
        finally { Marshal.ReleaseComObject(item); }
        if (h == IntPtr.Zero) throw new COMException("shell returned no thumbnail");
        try {
            int cb = Marshal.SizeOf(typeof(BITMAP));
            IntPtr buf = Marshal.AllocHGlobal(cb);
            BITMAP bm;
            try {
                if (GetObjectW(h, cb, buf) == 0) throw new Exception("GetObject failed on the returned HBITMAP");
                bm = (BITMAP)Marshal.PtrToStructure(buf, typeof(BITMAP));
            } finally { Marshal.FreeHGlobal(buf); }

            int w = bm.bmWidth, hgt = Math.Abs(bm.bmHeight);
            // BITMAPINFOHEADER asking for 32bpp BI_RGB, negative height = top-down rows.
            byte[] info = new byte[40];
            BitConverter.GetBytes(40).CopyTo(info, 0);
            BitConverter.GetBytes(w).CopyTo(info, 4);
            BitConverter.GetBytes(-hgt).CopyTo(info, 8);
            BitConverter.GetBytes((short)1).CopyTo(info, 12);
            BitConverter.GetBytes((short)32).CopyTo(info, 14);
            BitConverter.GetBytes(0).CopyTo(info, 16);   // BI_RGB
            byte[] bits = new byte[w * hgt * 4];
            IntPtr dc = GetDC(IntPtr.Zero);
            try {
                if (GetDIBits(dc, h, 0, (uint)hgt, bits, info, 0 /*DIB_RGB_COLORS*/) == 0)
                    throw new Exception("GetDIBits failed");
            } finally { ReleaseDC(IntPtr.Zero, dc); }

            HashSet<int> colours = new HashSet<int>();
            int stepY = Math.Max(1, hgt / 24), stepX = Math.Max(1, w / 24);
            for (int y = 0; y < hgt; y += stepY)
                for (int x = 0; x < w; x += stepX) {
                    int o = (y * w + x) * 4;
                    colours.Add((bits[o+2] << 16) | (bits[o+1] << 8) | bits[o]);
                }

            if (output != null) WriteBmp(output, w, hgt, bits);
            return new int[] { w, hgt, colours.Count };
        } finally { DeleteObject(h); }
    }

    /// A 32bpp top-down BMP: the pixels are already in the exact layout the format wants, so
    /// this is a header and a write rather than an encode.
    static void WriteBmp(string path, int w, int h, byte[] bits) {
        using (BinaryWriter bw = new BinaryWriter(File.Create(path))) {
            bw.Write((ushort)0x4D42); bw.Write(54 + bits.Length); bw.Write(0); bw.Write(54);
            bw.Write(40); bw.Write(w); bw.Write(-h); bw.Write((ushort)1); bw.Write((ushort)32);
            bw.Write(0); bw.Write(bits.Length); bw.Write(2835); bw.Write(2835); bw.Write(0); bw.Write(0);
            bw.Write(bits);
        }
    }

    // ---- process snapshot (Toolhelp32) ----
    [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)]
    struct PROCESSENTRY32W {
        public uint dwSize; public uint cntUsage; public uint th32ProcessID;
        public IntPtr th32DefaultHeapID; public uint th32ModuleID; public uint cntThreads;
        public uint th32ParentProcessID; public int pcPriClassBase; public uint dwFlags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst=260)] public string szExeFile;
    }
    [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr CreateToolhelp32Snapshot(uint flags, uint pid);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode)] static extern bool Process32FirstW(IntPtr snap, ref PROCESSENTRY32W e);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode)] static extern bool Process32NextW(IntPtr snap, ref PROCESSENTRY32W e);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool CloseHandle(IntPtr h);

    public class Proc { public int Pid; public int Parent; public string Name; }

    public static Proc[] Snapshot() {
        List<Proc> all = new List<Proc>();
        IntPtr snap = CreateToolhelp32Snapshot(0x00000002 /*SNAPPROCESS*/, 0);
        if (snap == (IntPtr)(-1)) return all.ToArray();
        try {
            PROCESSENTRY32W e = new PROCESSENTRY32W();
            e.dwSize = (uint)Marshal.SizeOf(typeof(PROCESSENTRY32W));
            if (Process32FirstW(snap, ref e)) {
                do {
                    all.Add(new Proc { Pid=(int)e.th32ProcessID, Parent=(int)e.th32ParentProcessID, Name=e.szExeFile });
                } while (Process32NextW(snap, ref e));
            }
        } finally { CloseHandle(snap); }
        return all.ToArray();
    }

    // ---- visible top-level windows, with their owning pid ----
    delegate bool EnumWindowsProc(IntPtr hwnd, IntPtr lParam);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumWindowsProc cb, IntPtr p);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);

    public class Win { public int Pid; public string Class; }

    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint m, IntPtr w, IntPtr l);

    public static Win[] VisibleWindows() {
        List<Win> found = new List<Win>();
        EnumWindows(delegate(IntPtr h, IntPtr unused) {
            if (!IsWindowVisible(h)) return true;
            uint pid; GetWindowThreadProcessId(h, out pid);
            StringBuilder sb = new StringBuilder(256);
            GetClassNameW(h, sb, sb.Capacity);
            found.Add(new Win { Pid=(int)pid, Class=sb.ToString() });
            return true;
        }, IntPtr.Zero);
        return found.ToArray();
    }
}
'@
}

$script:failures = [Collections.Generic.List[string]]::new()
function Check([string]$name, [scriptblock]$body) {
    try { & $body; Write-Host "  PASS  $name" -ForegroundColor Green }
    catch {
        Write-Host "  FAIL  $name" -ForegroundColor Red
        Write-Host "        $($_.Exception.Message)" -ForegroundColor Red
        $script:failures.Add($name)
    }
}

Write-Host "[explorer] installed: $installedDll" -ForegroundColor Cyan

# ---- 1. installed == built ------------------------------------------------------------------
Check 'the installed DLL is the one just built' {
    if (-not (Test-Path -LiteralPath $installedDll)) { throw "not installed: $installedDll" }
    $built = Join-Path $BuiltDir 'sagethumbs2k.dll'
    if (-not (Test-Path -LiteralPath $built)) { throw "no built DLL at $built" }
    $a = (Get-FileHash -Algorithm SHA256 -LiteralPath $installedDll).Hash
    $b = (Get-FileHash -Algorithm SHA256 -LiteralPath $built).Hash
    if ($a -ne $b) { throw "installed $a != built $b — reinstall before trusting anything below" }
    Write-Host "        sha256 $a" -ForegroundColor DarkGray
}
Check 'the decoder helper is installed beside the DLL' {
    # `sibling_of_dll` resolves st2k.exe from the DLL's own directory. If the installer ever
    # stops copying it, every out-of-process tier silently degrades to "no thumbnail" — the
    # pre-2.0 behaviour, which looks like nothing is wrong.
    if (-not (Test-Path -LiteralPath $installedExe)) { throw "missing: $installedExe" }
}

$tempDir = Join-Path ([IO.Path]::GetTempPath()) ('st2k-explorer-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tempDir | Out-Null
$explorerHwnd = [IntPtr]::Zero
$shell = $null

try {
    # GUID-named copies: Windows cannot have a thumbnail-cache entry for a path that did not
    # exist a moment ago, so the handler MUST be invoked. (Deleting thumbcache_*.db would not
    # do it — the running Explorer holds those open.)
    #
    # TWO directories, and that is not tidiness. Thumbnailing a file through the shell API
    # POPULATES the cache, so if Explorer were then pointed at the same files it would serve
    # every tile from that cache without ever calling the handler — no host load, no decoder
    # child, and a confident FAIL for a build that works perfectly. Each phase gets its own
    # untouched copies.
    $apiDir = Join-Path $tempDir 'shell-api'
    $explorerDir = Join-Path $tempDir 'explorer'
    New-Item -ItemType Directory -Path $apiDir, $explorerDir | Out-Null

    function Stage-Samples([string]$into) {
        $out = @()
        foreach ($c in $cases) {
            $src = Join-Path $CorpusPath $c.Name
            if (-not (Test-Path -LiteralPath $src)) { continue }
            $ext = [IO.Path]::GetExtension($c.Name)
            $dst = Join-Path $into ("{0}-{1}{2}" -f `
                [IO.Path]::GetFileNameWithoutExtension($c.Name), `
                [Guid]::NewGuid().ToString('N').Substring(0, 8), $ext)
            Copy-Item -LiteralPath $src -Destination $dst
            $out += [pscustomobject]@{ Case = $c; Path = $dst }
        }
        return $out
    }
    $staged = @(Stage-Samples $apiDir)
    $null = Stage-Samples $explorerDir
    foreach ($c in $cases) {
        if (-not (Test-Path -LiteralPath (Join-Path $CorpusPath $c.Name))) {
            Write-Host "  SKIP  $($c.Name) — not in the corpus" -ForegroundColor Yellow
        }
    }
    if ($staged.Count -eq 0) { throw 'no samples staged — corpus missing?' }

    # Which live dllhost processes have our DLL mapped right now. Called after each phase
    # rather than inside the poll loop: a module walk is far too slow for a 15 ms cadence,
    # and a host stays alive for seconds after it finishes a batch.
    function Get-ThumbnailHosts {
        $found = @()
        foreach ($p in Get-Process -Name dllhost -ErrorAction SilentlyContinue) {
            try {
                if ($p.Modules | Where-Object { $_.FileName -ieq $installedDll }) { $found += $p.Id }
            } catch { }   # a host at another integrity level is not inspectable; not a failure
        }
        return $found
    }

    # ---- 2/3/4. drive the shell, watching processes and windows throughout --------------------
    # A decoder child lives a few hundred milliseconds, so the observation has to be a tight
    # poll running CONCURRENTLY with the shell calls. Results go into a shared synchronized
    # store rather than the pipeline's return value: stopping a pipeline to read its output
    # throws PipelineStopped and loses exactly the evidence this is here to collect.
    # Both stores are keyed dictionaries, so a 15 ms poll over ~40 s records each distinct
    # process and window once instead of accumulating a quarter-million duplicates.
    # TWO process stores, and the second one is not redundant. `Procs` is cumulative and keyed
    # by pid, which is what the parentage assertions need. But Windows recycles pids
    # aggressively for short-lived processes, so counting "new keys in a cumulative store" to
    # measure ONE file's helpers under-reports the moment a helper lands on a pid a previous
    # helper just freed — measured: it reported 0 for a VP9 Profile 3 file that a per-file
    # count showed spawning 1. `Window` is cleared before each file, so a reused pid is a new
    # entry there and the count is right.
    $sync = [hashtable]::Synchronized(@{
        Procs  = [Collections.Hashtable]::Synchronized(@{})
        Window = [Collections.Hashtable]::Synchronized(@{})
        Wins   = [Collections.Hashtable]::Synchronized(@{})
        Stop   = $false
    })
    $rs = [runspacefactory]::CreateRunspace()
    $rs.Open()
    $rs.SessionStateProxy.SetVariable('sync', $sync)
    $watcher = [powershell]::Create()
    $watcher.Runspace = $rs
    [void]$watcher.AddScript({
        # `Add-Type` publishes into the AppDomain, so the probe type is visible here too.
        while (-not $sync.Stop) {
            foreach ($p in [St2kExplorerProbe]::Snapshot()) {
                if ($p.Name -imatch '^(st2k|dllhost)\.exe$') {
                    if (-not $sync.Procs.ContainsKey($p.Pid)) {
                        $sync.Procs[$p.Pid] = [pscustomobject]@{ Pid = $p.Pid; Parent = $p.Parent; Name = $p.Name }
                    }
                    if ($p.Name -ieq 'st2k.exe') { $sync.Window[$p.Pid] = $true }
                }
            }
            foreach ($w in [St2kExplorerProbe]::VisibleWindows()) {
                $key = "$($w.Pid)|$($w.Class)"
                if (-not $sync.Wins.ContainsKey($key)) {
                    $sync.Wins[$key] = [pscustomobject]@{ Pid = $w.Pid; Class = $w.Class }
                }
            }
            Start-Sleep -Milliseconds 15
        }
    })
    $watchHandle = $watcher.BeginInvoke()

    # The shell API path first: this is what every picker, the taskbar and the file dialog use,
    # so a pass here is broader than "Explorer draws it".
    foreach ($s in $staged) {
        Check "shell thumbnail: $($s.Case.Name)  [$($s.Case.Why)]" {
            $sync.Window.Clear()
            $bmp = Join-Path $tempDir ([IO.Path]::GetFileNameWithoutExtension($s.Path) + '-thumb.bmp')
            $r = [St2kExplorerProbe]::Thumbnail($s.Path, $bmp, 256)
            # A helper can outlive the call that spawned it; count after it has settled.
            Start-Sleep -Milliseconds 400
            $spawned = $sync.Window.Count
            Write-Host "        $($r[0])x$($r[1]), $($r[2]) distinct colours, $spawned helper process(es)" -ForegroundColor DarkGray
            # A blank tile still returns a perfectly valid HBITMAP, so "did we get a bitmap"
            # is a different question from "did we get a picture". Demand real variety.
            if ($r[2] -lt 4) { throw "blank/uniform tile — only $($r[2]) distinct colours" }
            if ($r[0] -lt 16 -or $r[1] -lt 16) { throw "implausible tile size $($r[0])x$($r[1])" }
            if ($spawned -ne $s.Case.Helpers) {
                throw "expected $($s.Case.Helpers) helper process(es), saw $spawned — the decode tier ordering changed, so this format is being decoded somewhere other than where it was designed to be"
            }
        }
    }

    $hosts = @(Get-ThumbnailHosts)
    Write-Host "        hosts after the shell-API phase: $(if ($hosts.Count) { $hosts -join ', ' } else { 'none' })" -ForegroundColor DarkGray

    # Now Explorer itself, on its OWN untouched copies. This is the surface the user sees, and
    # the one that definitely uses the isolated surrogate.
    $shell = New-Object -ComObject Shell.Application
    $shell.Explore($explorerDir)
    $deadline = [datetime]::UtcNow.AddSeconds(20)
    $explorerWin = $null
    do {
        foreach ($w in @($shell.Windows())) {
            try {
                if ($w.FullName -and ([IO.Path]::GetFileName($w.FullName) -ieq 'explorer.exe') -and
                    ([string]::Equals(([Uri]$w.LocationURL).LocalPath.TrimEnd('\'), $explorerDir, [StringComparison]::OrdinalIgnoreCase))) {
                    $explorerWin = $w
                    $explorerHwnd = [IntPtr][int64]$w.HWND
                    break
                }
            } catch { }
        }
        if ($explorerHwnd -ne [IntPtr]::Zero) { break }
        Start-Sleep -Milliseconds 150
    } while ([datetime]::UtcNow -lt $deadline)
    if ($explorerHwnd -eq [IntPtr]::Zero) { throw "Explorer did not open $explorerDir" }

    # Force a thumbnail view. Explorer picks a folder's view from its content type, and a
    # Details or List view generates NO thumbnails at all — the whole phase would then observe
    # nothing and blame the code. FVM_THUMBNAIL = 5.
    try {
        $explorerWin.Document.CurrentViewMode = 5
        Start-Sleep -Milliseconds 500
    } catch {
        Write-Host "        (could not force thumbnail view: $($_.Exception.Message))" -ForegroundColor Yellow
    }

    # Give Explorer time to walk the folder and generate every tile.
    Start-Sleep -Seconds 12
    $hosts = @($hosts + (Get-ThumbnailHosts) | Sort-Object -Unique)

    Check 'Explorer loaded the handler into an isolated dllhost' {
        if ($hosts.Count -eq 0) {
            throw 'no dllhost.exe had sagethumbs2k.dll mapped — either the shell did not choose our handler, or it ran it in-process, which would mean a decoder panic takes the whole shell down'
        }
        Write-Host "        dllhost pids: $($hosts -join ', ')" -ForegroundColor DarkGray
    }

    # Ask the poller to finish on its own terms, so EndInvoke returns rather than throwing.
    $sync.Stop = $true
    try { $null = $watcher.EndInvoke($watchHandle) } catch { }
    $watcher.Dispose()
    $rs.Close()

    $procs = @($sync.Procs.Values)
    $wins  = @($sync.Wins.Values)

    $st2kSeen = @($procs | Where-Object { $_.Name -ieq 'st2k.exe' })
    $hostPids = @($procs | Where-Object { $_.Name -ieq 'dllhost.exe' } | Select-Object -ExpandProperty Pid)

    Check 'the decoder helper spawned from the shell host, not from this script' {
        if ($st2kSeen.Count -eq 0) {
            throw 'no st2k.exe child was ever observed — the out-of-process VP6/VP9 tiers never ran, so those tiles came from somewhere else, or not at all'
        }
        # Parented to a host we PROVED has our DLL mapped is the conclusive form. Parented to
        # some other dllhost is still the surrogate path (a host can exit before its modules
        # are read), so it is reported and accepted; parented only to this script is not,
        # because that is the in-process case the whole isolation design exists to avoid.
        $parents = @($st2kSeen | Select-Object -ExpandProperty Parent -Unique)
        $fromKnownHost = @($st2kSeen | Where-Object { $hosts -contains $_.Parent })
        $fromAnyHost   = @($st2kSeen | Where-Object { $hostPids -contains $_.Parent })
        Write-Host "        st2k children: $($st2kSeen.Count); parent pids: $($parents -join ', '); this script is $PID" -ForegroundColor DarkGray
        if ($fromKnownHost.Count -gt 0) {
            Write-Host "        $($fromKnownHost.Count) spawned by a dllhost with sagethumbs2k.dll mapped" -ForegroundColor DarkGray
            return
        }
        if ($fromAnyHost.Count -gt 0) {
            Write-Host "        $($fromAnyHost.Count) spawned by a dllhost (module list unread — host already gone)" -ForegroundColor DarkGray
            return
        }
        throw "no st2k.exe child was parented to a thumbnail host; parents were $($parents -join ', ') and this script is $PID, so the dllhost spawn path is unproven"
    }

    Check 'no decoder child ever showed a console window' {
        $bad = @($wins | Where-Object { $st2kSeen.Pid -contains $_.Pid })
        if ($bad.Count -gt 0) {
            throw "st2k.exe owned $($bad.Count) visible window(s) (classes: $(($bad | Select-Object -ExpandProperty Class -Unique) -join ', ')) — CREATE_NO_WINDOW is missing, so every video tile flashes a console on the user's desktop"
        }
    }
} finally {
    # Close ONLY the window this script opened (WM_CLOSE), never the shell at large.
    if ($explorerHwnd -ne [IntPtr]::Zero) {
        [void][St2kExplorerProbe]::PostMessageW($explorerHwnd, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)
    }
    if ($shell) { [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell) }
    if (-not $Keep -and (Test-Path -LiteralPath $tempDir)) {
        Start-Sleep -Milliseconds 800   # let Explorer release the directory
        Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
    } elseif ($Keep) {
        Write-Host "[explorer] kept: $tempDir" -ForegroundColor DarkGray
    }
}

if ($script:failures.Count -gt 0) {
    Write-Host "[explorer] FAIL — $($script:failures.Count): $($script:failures -join '; ')" -ForegroundColor Red
    exit 1
}
Write-Host '[explorer] PASS' -ForegroundColor Green
