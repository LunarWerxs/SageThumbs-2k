<#
  decode-speed-lib.ps1 — the shared decode-timing primitives.

  Dot-sourced by BOTH scripts\check-decode-speed.ps1 (the pass/fail GATE) and
  scripts\speed-report.ps1 (the stored REPORT). They ask different questions of the same
  measurements, and a second copy of "how do we time a decode" would drift from this one
  within a release — the numbers in the report would then stop meaning what the gate
  enforces, which is the one property that makes either of them worth having.

  Requires the caller to have set `$st2k` (path to the release st2k.exe) and to have run
  `Add-Type -AssemblyName PresentationCore` for the WIC side.

  WHY THESE SHAPES (each one is a measurement that was wrong before it was fixed):

  * OUR side is timed by `st2k bench-decode`, which decodes many files inside ONE process.
    Timing `st2k thumbnail` once per file instead measures PROCESS STARTUP as much as
    decoding — a floor that swings 28 ms on an idle box to 187 ms on a busy one, and which
    is not symmetric with the WIC side (in-process), so load inflates only us. The shell
    extension does not spawn per thumbnail either, so per-file spawn was never the thing
    worth measuring.
  * WIC's side bypasses the image cache. A cached BitmapFrame returns in ~3 ms and makes
    Windows look impossibly fast; that made an early version of this report nonsense.
  * BOTH sides take the MINIMUM of N runs. The fastest observation is the one least
    polluted by other work, which is the only statistic that behaves on a box running a
    dozen agent sessions.
#>

# Time a batch of files through our own decoder. Returns @{ filename = milliseconds }.
# A file that fails to decode is simply ABSENT from the map — never recorded as fast.
function Measure-Decode([string[]]$paths, [int]$runs, [int]$size) {
    if (-not $paths) { return @{} }
    $out = @{}
    # Chunked so the command line cannot overflow on a big corpus.
    for ($i = 0; $i -lt $paths.Count; $i += 60) {
        $chunk = $paths[$i..([Math]::Min($i + 59, $paths.Count - 1))]
        $argv = @('bench-decode') + $chunk + @('--size', $size, '--runs', $runs)
        foreach ($line in (& $script:st2k @argv 2>$null)) {
            $p = $line -split "`t"
            if ($p.Count -ge 2 -and $p[1] -ne 'FAIL') { $out[$p[0]] = [double]$p[1] }
        }
    }
    return $out
}

$script:WicOpt = [System.Windows.Media.Imaging.BitmapCreateOptions]::IgnoreImageCache

# Time one file through Windows' own codec, in-process. `$null` = WIC cannot open it at all,
# which is a "no native peer" (most of our formats), NOT a failure.
function Measure-Wic([string]$path, [int]$runs) {
    $best = [double]::MaxValue
    try { $bytes = [IO.File]::ReadAllBytes($path) } catch { return $null }
    for ($i = 0; $i -lt $runs; $i++) {
        $ms = New-Object IO.MemoryStream (, $bytes)
        try {
            $sw = [Diagnostics.Stopwatch]::StartNew()
            $fr = [System.Windows.Media.Imaging.BitmapFrame]::Create($ms, $script:WicOpt, 'OnLoad')
            $cv = New-Object System.Windows.Media.Imaging.FormatConvertedBitmap `
                $fr, ([System.Windows.Media.PixelFormats]::Bgra32), $null, 0
            $stride = [int]($cv.PixelWidth * 4)
            $buf = New-Object byte[] ($stride * $cv.PixelHeight)
            $cv.CopyPixels($buf, $stride, 0)      # force a REAL decode, not a lazy handle
            $sw.Stop()
            $e = $sw.Elapsed.TotalMilliseconds
            if ($e -lt $best) { $best = $e }
        } catch {
            return $null
        } finally { $ms.Dispose() }
    }
    return [Math]::Round($best, 3)
}

# Time the drift references. Pass one file per EXECUTION WORLD — an in-process decode and a
# subprocess (ImageMagick-tier) one. Timing only an in-process reference is blind to the load
# that matters here: background work inflates a subprocess decode ~3x while barely moving an
# in-process one, so a run once read "drift 1%" while five magick-tier formats sat at 2.5-2.9x
# their baselines. Pure load, which then "confirmed" under the same load.
function Measure-Drift([string[]]$refs, [int]$runs, [int]$size) {
    $vals = @()
    foreach ($r in $refs) {
        if ($r) { $vals += (Measure-Decode @($r) $runs $size).Values | Select-Object -First 1 }
    }
    return $vals
}

# The WORST relative move across the reference pair. Past the caller's tolerance the box
# changed under the run and its numbers are not comparable.
function Get-WorstDrift($before, $after) {
    $w = 0.0
    for ($i = 0; $i -lt [Math]::Min($before.Count, $after.Count); $i++) {
        if ($before[$i] -and $after[$i]) {
            $d = [Math]::Abs($after[$i] - $before[$i]) / [Math]::Max($before[$i], $after[$i])
            if ($d -gt $w) { $w = $d }
        }
    }
    return $w
}

# The corpus files worth timing: samples only, never the manifests or the contact sheet.
function Get-SpeedSamples([string]$corpus, [string[]]$only) {
    $skipNames = @('contact.png', 'README.md')
    $files = Get-ChildItem $corpus -File | Where-Object {
        $_.Extension -and $_.Name -notin $skipNames -and -not $_.Name.StartsWith('_') -and
        $_.Extension.ToLowerInvariant() -notin @('.md', '.txt')
    }
    if ($only) {
        $want = $only | ForEach-Object { $_.ToLowerInvariant().TrimStart('.') }
        $files = $files | Where-Object { $want -contains $_.Extension.ToLowerInvariant().TrimStart('.') }
    }
    return @($files)
}
