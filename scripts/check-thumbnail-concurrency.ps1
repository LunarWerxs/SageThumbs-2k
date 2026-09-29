<#
.SYNOPSIS
    Ask the INSTALLED thumbnail handler for many thumbnails at once, from several processes, and
    fail if any request fails or any client is still waiting at the deadline.

.DESCRIPTION
    In-process tests decode one file at a time in the test process. This drives the real path:
    Windows' IThumbnailCache, which activates our handler in the shell's thumbnail surrogate
    (dllhost). Each client works on GUID-named copies of the files and passes
    WTS_EXTRACTDONOTCACHE, so every request reaches the handler rather than the cache.

    It exists because 3.4.0 froze the surrogate (and every thumbnail and taskbar icon with it)
    when eight AVIFs were asked for together, while every in-process test passed
    (docs/DEVELOPMENT_GOTCHAS.md, "A hang only the real host can show"). Run it after installing
    any change to a decode path the shell reaches, with files of the formats the change touched.

    Exit 0: every client finished with no failed request. Exit 1: a failure, or a client still
    running at the deadline (it is then killed, and so is any surrogate holding our DLL, so the
    machine is usable again). Exit 2: nothing to measure (no files, or the handler is not
    installed) - never read that as a pass.

.EXAMPLE
    pwsh scripts\check-thumbnail-concurrency.ps1 -Files ..\test-corpus\*.avif,tests\fixtures\avif\*.avif
#>
param(
    [string[]]$Files,
    [int]$Clients = 8,
    [int]$Rounds = 3,
    [int]$TimeoutSec = 180,
    [int]$Size = 256,
    # Internal: run as one client over the list in $ListFile, writing its tally to stdout.
    [switch]$Client,
    [string]$ListFile
)
$ErrorActionPreference = 'Stop'

if ($Client) {
    Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class ThumbProbe {
    [ComImport, Guid("43826d1e-e718-42ee-bc55-a1e261c37bfe"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    public interface IShellItem { }
    [ComImport, Guid("F676C15D-596A-4ce2-8234-33996F445DB1"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IThumbnailCache {
        [PreserveSig] int GetThumbnail(IShellItem item, uint cx, uint flags, out IntPtr bmp, out uint outFlags, IntPtr id);
    }
    [DllImport("shell32.dll", CharSet=CharSet.Unicode, PreserveSig=false)]
    static extern void SHCreateItemFromParsingName(string path, IntPtr bc, ref Guid iid, [MarshalAs(UnmanagedType.Interface)] out IShellItem item);
    static IThumbnailCache cache;
    public static int Get(string path, uint cx) {
        if (cache == null) cache = (IThumbnailCache)Activator.CreateInstance(Type.GetTypeFromCLSID(new Guid("50EF4544-AC9F-4A8E-B21B-8A26180DB13F")));
        Guid iid = typeof(IShellItem).GUID; IShellItem item;
        SHCreateItemFromParsingName(path, IntPtr.Zero, ref iid, out item);
        IntPtr bmp; uint of;
        // 0x20 = WTS_EXTRACTDONOTCACHE: always ask the handler, never answer from the cache.
        int hr = cache.GetThumbnail(item, cx, 0x20, out bmp, out of, IntPtr.Zero);
        if (bmp != IntPtr.Zero) Marshal.Release(bmp);
        Marshal.ReleaseComObject(item);
        return hr;
    }
}
'@
    $scratch = Join-Path ([IO.Path]::GetTempPath()) ('st2k-concurrency-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory $scratch | Out-Null
    try {
        $copies = foreach ($f in (Get-Content $ListFile | Where-Object { $_ })) {
            $c = Join-Path $scratch ([guid]::NewGuid().ToString('N') + [IO.Path]::GetExtension($f))
            Copy-Item -LiteralPath $f $c
            $c
        }
        $fails = @{}; $n = 0
        for ($r = 1; $r -le $Rounds; $r++) {
            foreach ($c in $copies) {
                $hr = [ThumbProbe]::Get($c, [uint32]$Size); $n++
                if ($hr -ne 0) { $k = '0x{0:x8} {1}' -f $hr, [IO.Path]::GetExtension($c); $fails[$k] = 1 + [int]$fails[$k] }
            }
        }
        $failed = ($fails.Values | Measure-Object -Sum).Sum
        "DONE requests=$n failed=$([int]$failed) " + (($fails.GetEnumerator() | ForEach-Object { "[$($_.Key) x$($_.Value)]" }) -join ' ')
    } finally {
        Remove-Item $scratch -Recurse -Force -ErrorAction SilentlyContinue
    }
    exit 0
}

function Get-OurSurrogates {
    Get-Process dllhost -ErrorAction SilentlyContinue |
        Where-Object { try { $_.Modules.ModuleName -contains 'sagethumbs2k.dll' } catch { $false } }
}

# `pwsh -File` hands an array parameter over as ONE comma-joined string; take it apart here.
$patterns = @($Files | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
$list = @(foreach ($pattern in $patterns) { Get-ChildItem -Path $pattern -File -ErrorAction SilentlyContinue | ForEach-Object FullName }) | Sort-Object -Unique
if (-not $list) {
    Write-Host "check-thumbnail-concurrency: INCONCLUSIVE - no files matched $($Files -join ', ')"
    exit 2
}
$installed = Test-Path 'Registry::HKEY_CLASSES_ROOT\CLSID\{7B2E6A14-9C3D-4F8A-B1E7-2A5D9F0C6E31}\InprocServer32'
if (-not $installed) {
    Write-Host 'check-thumbnail-concurrency: INCONCLUSIVE - the SageThumbs thumbnail handler is not registered'
    exit 2
}

$work = Join-Path ([IO.Path]::GetTempPath()) ('st2k-concurrency-run-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory $work | Out-Null
$listFile = Join-Path $work 'files.txt'
$list | Set-Content $listFile
$self = $MyInvocation.MyCommand.Path
$shell = (Get-Process -Id $PID).Path
$procs = foreach ($i in 1..$Clients) {
    Start-Process $shell -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput (Join-Path $work "client-$i.txt") -RedirectStandardError (Join-Path $work "client-$i.err") `
        -ArgumentList "-NoProfile -STA -ExecutionPolicy Bypass -File `"$self`" -Client -ListFile `"$listFile`" -Rounds $Rounds -Size $Size"
}
$sw = [Diagnostics.Stopwatch]::StartNew()
while (@($procs | Where-Object { -not $_.HasExited }).Count -and $sw.Elapsed.TotalSeconds -lt $TimeoutSec) {
    Start-Sleep -Milliseconds 250
}
$stuck = @($procs | Where-Object { -not $_.HasExited })
$bad = 0
foreach ($i in 1..$Clients) {
    $line = Get-Content (Join-Path $work "client-$i.txt") -ErrorAction SilentlyContinue | Where-Object { $_ -like 'DONE*' } | Select-Object -First 1
    if (-not $line) { $bad++; Write-Host "  client $i : no result"; continue }
    if ($line -notmatch 'failed=0 ') { $bad++ }
    Write-Host "  client $i : $line"
}
Write-Host ("check-thumbnail-concurrency: {0} files x {1} rounds x {2} clients, {3} stuck after {4:N0} s" -f $list.Count, $Rounds, $Clients, $stuck.Count, $sw.Elapsed.TotalSeconds)
if ($stuck.Count) {
    $stuck | ForEach-Object { Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue }
    Get-OurSurrogates | ForEach-Object {
        Write-Host "  killed the wedged thumbnail surrogate $($_.Id)"
        Stop-Process -Id $_.Id -Force
    }
}
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
if ($stuck.Count -or $bad) {
    Write-Host 'check-thumbnail-concurrency: FAILED'
    exit 1
}
Write-Host 'check-thumbnail-concurrency: PASS'
exit 0
