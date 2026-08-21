<#
.SYNOPSIS
    Proves a pre-build actually WARMS the views Explorer paints, instead of merely
    reporting that it did.

.DESCRIPTION
    This is the check that v2.1.2's bug slipped past, and the reason it exists.

    For its whole life, `prebuild` extracted at the SMALLEST requested size and let
    Windows derive the bigger buckets. Windows reported those as cached, `WTS_INCACHEONLY`
    succeeded for them, and the run printed 100% success. But Explorer threw the derived
    entries away and re-extracted for real on first browse, which is the exact slow
    tile-by-tile build the feature exists to prevent.

    Nothing in the test suite could see that, because nothing FAILED. Every call returned
    OK. The only way to catch it is to ask the shell the way EXPLORER does, with a live
    request (no SIIGBF_INCACHEONLY), and watch whether our provider gets called again. If
    it does, the pre-build did not pre-build that view.

    Unit tests pin the build ORDER (`prebuild::tests::the_largest_bucket_is_always_extracted_first`),
    which is the mechanism. This script checks the OUTCOME, end to end, against the real
    Windows shell.

.NOTES
    NOT a CI gate and deliberately not wired into release.ps1: it needs the shell extension
    REGISTERED on this machine, and it measures the INSTALLED provider, not the build tree.
    Run it after an install when you touch prebuild.rs, the thumbnail provider, or the size
    buckets. It writes only to a scratch folder under %TEMP% and restores the Debug flag.

.EXAMPLE
    pwsh scripts\check-prebuild-coverage.ps1
    pwsh scripts\check-prebuild-coverage.ps1 -Exe D:\.DevScratch\build-cache\st2k-target\release\st2k.exe
#>
[CmdletBinding()]
param(
    # Which st2k to drive. Defaults to the installed one, since that is what a user runs.
    [string]$Exe = "$env:ProgramFiles\SageThumbs2K\st2k.exe",
    # Samples to prove it on. Defaults to a spread of decode tiers: PDF (the slow
    # rasterizer that surfaced the bug), a plain raster, and an animated one.
    [string[]]$Samples = @("sample.pdf", "sample.png", "sample.gif"),
    # The views to prove. 96 and 256 are what Explorer asks for at 100% scaling; 768 is
    # its largest icon view at 300% scaling.
    [int[]]$Views = @(96, 256, 768),
    # SELF-TEST. Reproduces the pre-2.1.2 behaviour by building ONLY the smallest bucket
    # and letting Windows derive the rest, then asserts this script REPORTS THAT AS A
    # FAILURE. A guard that cannot fail is worse than no guard, and this one is checking a
    # bug whose whole signature was "everything reports success", so it has to be proven
    # able to say no. Expect a non-zero exit; that IS the pass.
    [switch]$ProveItFails
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
$corpus = Join-Path (Split-Path $repo -Parent) 'test-corpus'

if (-not (Test-Path $Exe)) {
    Write-Host "st2k not found at $Exe" -ForegroundColor Red
    Write-Host "  install first (pwsh scripts\install.ps1) or pass -Exe" -ForegroundColor Yellow
    exit 1
}

# The provider's own verbose log is the instrument: it is the only way to see whether the
# shell reached our code or answered from its cache. Restored on the way out.
$key = 'HKCU:\Software\SageThumbs2K'
$prevDebug = (Get-ItemProperty -Path $key -Name Debug -ErrorAction SilentlyContinue).Debug
$log = Join-Path $env:LOCALAPPDATA 'SageThumbs2K.log'

Add-Type -AssemblyName System.Drawing -ErrorAction SilentlyContinue
Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices;
public static class St2kShellAsk {
    [ComImport, Guid("bcc18b79-ba16-442f-80c4-8a59c30c463b"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IShellItemImageFactory { void GetImage([In, MarshalAs(UnmanagedType.Struct)] SIZE s, [In] int f, [Out] out IntPtr h); }
    [StructLayout(LayoutKind.Sequential)] struct SIZE { public int cx; public int cy; }
    [DllImport("shell32.dll", CharSet = CharSet.Unicode, PreserveSig = false)]
    static extern void SHCreateItemFromParsingName([MarshalAs(UnmanagedType.LPWStr)] string p, IntPtr b,
        [MarshalAs(UnmanagedType.LPStruct)] Guid r, [MarshalAs(UnmanagedType.Interface)] out IShellItemImageFactory i);
    [DllImport("gdi32.dll")] static extern bool DeleteObject(IntPtr o);
    // SIIGBF_THUMBNAILONLY only. NO SIIGBF_INCACHEONLY: asking the way Explorer does is
    // the entire point, because an in-cache-only probe is what lied in the first place.
    public static bool Ask(string path, int size) {
        IShellItemImageFactory f; IntPtr hbm = IntPtr.Zero;
        SHCreateItemFromParsingName(path, IntPtr.Zero, new Guid("bcc18b79-ba16-442f-80c4-8a59c30c463b"), out f);
        try { f.GetImage(new SIZE { cx = size, cy = size }, 0x08, out hbm); } catch { return false; }
        if (hbm == IntPtr.Zero) return false;
        DeleteObject(hbm); return true;
    }
}
'@ -ErrorAction Stop

# CALLERS MUST WRAP THIS IN @(). PowerShell unrolls an array on return, so a single match
# comes back as a bare string and `$calls[0]` then indexes a CHARACTER ('7' of "768")
# rather than the element, quietly failing a comparison on a run that is actually correct.
# That is precisely what happened the first time this script was run. Wrapping at the call
# site is the fix; returning `,$array` from here instead double-wraps and is worse.
function Get-ProviderCalls {
    Start-Sleep -Milliseconds 400   # the surrogate flushes after it returns
    if (-not (Test-Path $log)) { return }
    Get-Content $log | ForEach-Object { if ($_ -match 'GetThumbnail: cx=(\d+)') { $matches[1] } }
}

$failures = 0
$scratch = Join-Path $env:TEMP ("st2k-cov-" + [guid]::NewGuid().ToString('N').Substring(0, 8))

try {
    Set-ItemProperty -Path $key -Name Debug -Value 1 -Type DWord
    New-Item -ItemType Directory -Path $scratch -Force | Out-Null
    Write-Host "[coverage] pre-build must WARM the views, not just report that it did" -ForegroundColor Cyan
    Write-Host "  st2k   : $Exe"
    Write-Host "  views  : $($Views -join ', ') px"
    Write-Host ""

    foreach ($sample in $Samples) {
        $src = Join-Path $corpus $sample
        if (-not (Test-Path $src)) {
            Write-Host "  $sample  SKIP (not in $corpus)" -ForegroundColor DarkYellow
            continue
        }

        # A GUID name means Windows has never cached a thumbnail for this path, so every
        # request below has to be answered for real rather than from an earlier run.
        $file = Join-Path $scratch ([guid]::NewGuid().ToString('N') + [IO.Path]::GetExtension($src))
        Copy-Item $src $file

        # The old code extracted at the SMALLEST size and its probes for the larger buckets
        # then hit, so it never extracted anything else. Asking for only the smallest size
        # reproduces that exactly.
        $askFor = if ($ProveItFails) { ($Views | Measure-Object -Minimum).Minimum } else { ($Views -join ',') }

        if (Test-Path $log) { Remove-Item $log -Force }
        & $Exe prebuild $scratch --size $askFor --jobs 1 | Out-Null
        $built = @(Get-ProviderCalls)
        $expected = ($Views | Measure-Object -Maximum).Maximum
        if ($ProveItFails) { $expected = ($Views | Measure-Object -Minimum).Minimum }

        Write-Host ("  {0}" -f $sample) -ForegroundColor White
        if ($built.Count -eq 1 -and [int]$built[0] -eq $expected) {
            Write-Host ("    [ok]   rendered once, at {0} px (the largest requested)" -f $expected) -ForegroundColor Green
        }
        else {
            Write-Host ("    [FAIL] expected ONE render at {0} px, got [{1}]" -f $expected, ($built -join ',')) -ForegroundColor Red
            Write-Host "           largest-first is what makes the smaller views free; see prebuild::build_order" -ForegroundColor DarkGray
            $failures++
        }

        foreach ($px in $Views) {
            if (Test-Path $log) { Remove-Item $log -Force }
            $got = [St2kShellAsk]::Ask($file, $px)
            $again = @(Get-ProviderCalls)
            if (-not $got) {
                Write-Host ("    [FAIL] the {0} px view produced no thumbnail at all" -f $px) -ForegroundColor Red
                $failures++
            }
            elseif ($again.Count -gt 0) {
                Write-Host ("    [FAIL] the {0} px view RE-EXTRACTED (cx={1}) - it was never pre-built" -f $px, ($again -join ',')) -ForegroundColor Red
                Write-Host "           the run would still have reported success; that is the v2.1.2 bug" -ForegroundColor DarkGray
                $failures++
            }
            else {
                Write-Host ("    [ok]   the {0} px view was served from cache, no work on browse" -f $px) -ForegroundColor Green
            }
        }
        Remove-Item $file -Force -ErrorAction SilentlyContinue
        Write-Host ""
    }
}
finally {
    Remove-Item $scratch -Recurse -Force -ErrorAction SilentlyContinue
    if ($null -eq $prevDebug) { Remove-ItemProperty -Path $key -Name Debug -ErrorAction SilentlyContinue }
    else { Set-ItemProperty -Path $key -Name Debug -Value $prevDebug -Type DWord }
}

if ($ProveItFails) {
    # Inverted on purpose: under the old smallest-first behaviour the bigger views MUST be
    # reported as re-extracting. If they are not, this script has lost its teeth and would
    # sit green through a regression of the exact bug it was written for.
    if ($failures -gt 0) {
        Write-Host "[coverage] SELF-TEST PASS - $failures failure(s) reported for the old build order, as required" -ForegroundColor Green
        exit 0
    }
    Write-Host "[coverage] SELF-TEST FAILED - the old build order was reported as CLEAN" -ForegroundColor Red
    Write-Host "  this script can no longer detect the bug it exists for; fix it before trusting a green run" -ForegroundColor Yellow
    exit 1
}

if ($failures -gt 0) {
    Write-Host "[coverage] $failures failure(s): a pre-build is claiming work it did not do" -ForegroundColor Red
    exit 1
}
Write-Host "[coverage] PASS - every view is genuinely warm after a pre-build" -ForegroundColor Green
exit 0
