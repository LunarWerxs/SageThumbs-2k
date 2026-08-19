<#
  check-decode-speed.ps1 — is any format's thumbnail SLOW? Two gates, because one alone
  leaves most of the product uncovered.

      pwsh scripts\check-decode-speed.ps1                  # run both gates
      pwsh scripts\check-decode-speed.ps1 -UpdateBaseline  # re-record gate B's baseline
      pwsh scripts\check-decode-speed.ps1 -Only avif,heic  # scope to some extensions
      pwsh scripts\check-decode-speed.ps1 -Verbose2        # print every measured row

  GATE A — VS NATIVE. For every format Windows can also decode, are we slower than Windows?
  GATE B — VS OURSELVES. For EVERY format, did this build get slower than the last one?

  WHY THIS EXISTS (2026-08-18). A user reported "AVIF thumbnailing is way slower than native
  W11 with the AV1 codec installed, or Icaros." It was true and it had shipped: decode.rs
  routed most AVIF around Microsoft's AV1 WIC codec (issue #9 — that codec misreads colour)
  and out to an ImageMagick SUBPROCESS. Correct colour, ~5-15x the cost.

  NOTHING CAUGHT IT, and that is the point of this file:
    * regression.ps1 has three gates and all three are about CORRECTNESS — did a PNG appear,
      did THIS sample render, is the picture right. AVIF passed all three. It rendered, and it
      rendered the RIGHT picture. It was just slow.
    * perf.ps1 flags anything over a FLAT 3000 ms. AVIF at ~860-1300 ms sails under it. A flat
      threshold cannot see a 13x slowdown that stays inside the budget, and it has no idea what
      a given format SHOULD cost.

  Gate B exists because Gate A alone was not "every one", and that gap is not small: of the
  224 real-corpus samples, only 75 have a native WIC peer. The other 152 — DjVu, PSD, the
  ebook/comic containers, the project-file previews, the whole ImageMagick long tail, i.e.
  most of what makes this product worth installing — have no Windows equivalent to be measured
  against, so Gate A is structurally blind to them. Gate B pins each sample against its own
  recorded time, so "nothing to compare against" stops meaning "nothing is checked".

  ── MEASUREMENT, and why it is done this way ────────────────────────────────────────────────
  Timing is `st2k bench-decode`, which decodes every file inside ONE process and reports
  per-file milliseconds. The earlier version of this script spawned `st2k thumbnail` once per
  file and subtracted a measured process-start "floor" instead. That was wrong twice over:

    * The floor is enormous and unstable — 28 ms on an idle box, 187 ms on a busy one — and it
      is NOT symmetric with what it gets compared against, since WIC is timed in-process. Load
      therefore inflated OUR side only, and a run at 80% CPU invented regressions: one sweep
      flagged 15 samples that every single one of which cleared on retry, and reported a 1 ms
      TIFF as 265 ms.
    * The shell extension does not spawn a process per thumbnail either, so per-file spawn cost
      was never part of what this is trying to measure.

  Both sides are now in-process, and the numbers are stable enough that this is a gate rather
  than a coin flip.

  FALSE REDS ARE THE WORST OUTCOME — a gate that cries wolf gets ignored, and then it misses
  the real thing — so, on top of the above:
    1. MIN-of-N, never mean: the fastest observation is the least polluted by other work.
    2. An ABSOLUTE floor (-MinMs). Below it a ratio is noise dividing noise.
    3. An ABSOLUTE delta (-MinDeltaMs). A format 3x native but 5 ms slower is not the hunt.
    4. Gate A compares only where WIC ACTUALLY DECODES. The 149 formats where we are the only
       decoder have no native baseline to be slower than, and are reported as "no native peer",
       never as a pass or a failure.
    5. DRIFT, checked TWICE. A fixed reference file is re-timed after the sweep and again after
       the confirmation pass; if either moved past -MaxDrift the box changed under us and the
       run is INCONCLUSIVE (warn, exit 0) rather than a verdict. Both checks are load-bearing:
       a run that drifted 29% "confirmed" a 377 ms AVIF against its true ~150 ms, because the
       retry simply re-measured the same background load.
    6. CONFIRMATION. Anything that trips is re-measured with 3x the runs, and only a repeat
       breach fails — the same false-alarm guard regression.ps1 uses.

  THE GATE A BASELINE IS THE OTHER HALF OF THE MECHANISM. Being slower than WIC is sometimes
  the RIGHT call — the AVIF 8-bit colour route is a deliberate trade. This check does not forbid
  that. It forces each such case to exist as an explicit, reviewed line in
  decode-speed-vs-native.txt with a REASON written next to it. A trade someone argued for stays
  green; a trade that appears by accident turns the build red. That difference is the entire
  reason AVIF went unnoticed for so long.

  EXIT CODES:
    0  both gates pass (or the box moved too much to judge — guard 5).
    1  a format is materially slower than Windows without a baselined reason (A), or slower
       than its own recorded time (B).
#>
[CmdletBinding()]
param(
    [string]$Corpus = "$PSScriptRoot\..\..\test-corpus-real",
    [string]$Baseline = "$PSScriptRoot\decode-speed-vs-native.txt",
    [string]$SpeedBaseline = "$PSScriptRoot\decode-speed-baseline.txt",
    [switch]$UpdateBaseline,
    [string[]]$Only,
    # Default gate A tolerance for a format with no explicit line. 3x is deliberately loose:
    # our pipeline does real work WIC does not (EXIF orientation, ICC to sRGB, fit-to-box), and
    # this gate hunts order-of-magnitude routing mistakes, not tuning.
    [double]$DefaultMaxRatio = 3.0,
    # Gate B tolerance: how much slower than its own recorded time a sample may get.
    [double]$MaxSelfRatio = 2.5,
    [int]$MinMs = 25,
    [int]$MinDeltaMs = 40,
    # Tightened from 0.35 after a run at 29% drift produced a FALSE RED (AVIF read 377 ms
    # against its usual ~150 ms purely from background load). Inconclusive is a safe outcome
    # and a false red is not, so this errs toward declining to judge.
    [double]$MaxDrift = 0.20,
    # 5, not 3. Measured on this box at 77% CPU: at 7 runs the worst sample sits 1.41x its
    # baseline with every delta under 6 ms, whereas 3 runs let transient spikes through as
    # gate-B suspects that then had to be cleared on retry. Min-of-N is the cheapest noise
    # reduction available here and the whole sweep is ~100 s, so buying stability is worth it.
    [int]$Runs = 5,
    [int]$Size = 256,
    [switch]$Verbose2
)
$ErrorActionPreference = 'Stop'

$st2k = Join-Path (& "$PSScriptRoot\_targetdir.ps1") 'release\st2k.exe'
if (-not (Test-Path $st2k))   { throw "st2k.exe not built (cargo build --release --bin st2k)" }
if (-not (Test-Path $Corpus)) { throw "corpus not found: $Corpus" }

Add-Type -AssemblyName PresentationCore

# ---------------------------------------------------------------- baselines
$allow = @{}
if (Test-Path $Baseline) {
    foreach ($line in Get-Content $Baseline) {
        $t = $line.Trim()
        if (-not $t -or $t.StartsWith('#')) { continue }
        $body = ($t -split '#', 2)[0]
        $parts = @($body.Trim() -split '\s+')
        if ($parts.Count -lt 2) { continue }
        $allow[$parts[0].ToLowerInvariant().TrimStart('.')] = [double]$parts[1]
    }
}
$speed = @{}
if (Test-Path $SpeedBaseline) {
    foreach ($line in Get-Content $SpeedBaseline) {
        $t = $line.Trim()
        if (-not $t -or $t.StartsWith('#')) { continue }
        $parts = @($t -split '\s+')
        if ($parts.Count -ge 2 -and $parts[1] -ne 'FAIL') { $speed[$parts[0]] = [double]$parts[1] }
    }
}

# ---------------------------------------------------------------- measurement
# One process, every file. Returns @{ name = ms }.
function Measure-Decode([string[]]$paths, [int]$runs) {
    if (-not $paths) { return @{} }
    $out = @{}
    # Chunked so the command line cannot overflow on a big corpus.
    for ($i = 0; $i -lt $paths.Count; $i += 60) {
        $chunk = $paths[$i..([Math]::Min($i + 59, $paths.Count - 1))]
        $argv = @('bench-decode') + $chunk + @('--size', $Size, '--runs', $runs)
        foreach ($line in (& $st2k @argv 2>$null)) {
            $p = $line -split "`t"
            if ($p.Count -ge 2 -and $p[1] -ne 'FAIL') { $out[$p[0]] = [double]$p[1] }
        }
    }
    return $out
}

# WIC, in-process, image cache BYPASSED (a cached BitmapFrame returns in ~3 ms and would make
# Windows look impossibly fast — this measurement was wrong once before it was fixed).
$wicOpt = [System.Windows.Media.Imaging.BitmapCreateOptions]::IgnoreImageCache
function Measure-Wic([string]$path, [int]$runs) {
    $best = [double]::MaxValue
    try { $bytes = [IO.File]::ReadAllBytes($path) } catch { return $null }
    for ($i = 0; $i -lt $runs; $i++) {
        $ms = New-Object IO.MemoryStream (,$bytes)
        try {
            $sw = [Diagnostics.Stopwatch]::StartNew()
            $fr = [System.Windows.Media.Imaging.BitmapFrame]::Create($ms, $wicOpt, 'OnLoad')
            $cv = New-Object System.Windows.Media.Imaging.FormatConvertedBitmap `
                    $fr, ([System.Windows.Media.PixelFormats]::Bgra32), $null, 0
            $stride = [int]($cv.PixelWidth * 4)
            $buf = New-Object byte[] ($stride * $cv.PixelHeight)
            $cv.CopyPixels($buf, $stride, 0)      # force a REAL decode, not a lazy handle
            $sw.Stop()
            $e = $sw.Elapsed.TotalMilliseconds
            if ($e -lt $best) { $best = $e }
        } catch {
            return $null                          # WIC cannot open it: no native peer
        } finally { $ms.Dispose() }
    }
    return [Math]::Round($best, 3)
}

$exitCode = 0

$skipNames = @('contact.png', 'README.md')
$files = Get-ChildItem $Corpus -File | Where-Object {
    $_.Extension -and $_.Name -notin $skipNames -and -not $_.Name.StartsWith('_') -and
    $_.Extension.ToLowerInvariant() -notin @('.md', '.txt')
}
if ($Only) {
    $want = $Only | ForEach-Object { $_.ToLowerInvariant().TrimStart('.') }
    $files = $files | Where-Object { $want -contains $_.Extension.ToLowerInvariant().TrimStart('.') }
}
if (-not $files) { throw "no samples matched" }

# Guard 5: a fixed reference file, timed before and after, detects the box moving under us.
$refFile = ($files | Sort-Object Name | Select-Object -First 1).FullName
$driftBefore = (Measure-Decode @($refFile) ($Runs * 2)).Values | Select-Object -First 1

$mine = Measure-Decode ($files.FullName) $Runs
$rows = @()
foreach ($f in $files) {
    if (-not $mine.ContainsKey($f.Name)) { continue }   # undecodable is regression.ps1's job
    $w = Measure-Wic $f.FullName $Runs
    $rows += [pscustomobject]@{
        name = $f.Name; ext = $f.Extension.ToLowerInvariant().TrimStart('.')
        mine = $mine[$f.Name]; wic = $w
        ratio = if ($null -ne $w -and $w -gt 0) { [Math]::Round($mine[$f.Name] / $w, 2) } else { $null }
    }
}

$driftAfter = (Measure-Decode @($refFile) ($Runs * 2)).Values | Select-Object -First 1
$drift = if ($driftBefore -and $driftAfter) {
    [Math]::Abs($driftAfter - $driftBefore) / [Math]::Max($driftBefore, $driftAfter)
} else { 0 }

$compared = @($rows | Where-Object { $null -ne $_.ratio })
$noPeer   = @($rows | Where-Object { $null -eq $_.ratio })
Write-Host ("[speed] {0} samples · {1} have a native WIC peer · {2} are ours alone · drift {3:P0}" -f
    $rows.Count, $compared.Count, $noPeer.Count, $drift) -ForegroundColor Cyan

if ($Verbose2) {
    $compared | Sort-Object ratio -Descending | ForEach-Object {
        "  {0,-8} {1,8:N1} ms ours · {2,8:N1} ms WIC · {3,6}x  {4}" -f $_.ext, $_.mine, $_.wic, $_.ratio, $_.name
    }
}

if ($UpdateBaseline) {
    if ($drift -gt $MaxDrift) {
        Write-Host ("[baseline] REFUSED - the box moved {0:P0} mid-run. A baseline recorded now would be" -f $drift) -ForegroundColor Red
        Write-Host  "           unreliable in both directions. Re-run when the machine is idle." -ForegroundColor Yellow
        exit 1
    }
    # Gate A's baseline is NEVER regenerated: its whole value is the hand-written REASON on each
    # line, and overwriting would discard that silently — the same quiet loss this check exists
    # to stop.
    if (Test-Path $Baseline) {
        Write-Host ("[gate A] baseline left ALONE ({0}) - it carries hand-written reasons." -f (Split-Path $Baseline -Leaf)) -ForegroundColor DarkGray
    }
    $bLines = @(
        '# decode-speed baseline — our OWN decode time (ms) per corpus sample, measured by',
        '# `st2k bench-decode` (one process, min of N runs). Enforced by check-decode-speed.ps1.',
        '#',
        '# This is the half that covers the formats Windows CANNOT decode, which is most of them:',
        '# 149 of these samples have no native peer, so the vs-native gate is blind to them. A',
        '# format that falls off a fast path shows up here as its own number getting worse, with',
        '# nothing needed to compare against.',
        '#',
        '# GENERATE ON AN IDLE MACHINE, and regenerate DELIBERATELY: accepting a slower number',
        '# here is accepting a slower product.',
        ''
    ) + ($rows | Sort-Object name | ForEach-Object { "{0,-28} {1,9:N3}" -f $_.name, $_.mine })
    Set-Content -LiteralPath $SpeedBaseline -Value $bLines -Encoding UTF8
    Write-Host ("[gate B] baseline written: {0} ({1} samples)" -f $SpeedBaseline, $rows.Count) -ForegroundColor Yellow
    exit 0
}

# ============================================================================ GATE A
# Worst sample per extension: a hard sample must not hide behind an easy one of the same
# format — the lesson regression.ps1's per-FILE gate encodes.
$aSuspect = @()
foreach ($g in ($compared | Group-Object ext)) {
    $w = $g.Group | Sort-Object ratio -Descending | Select-Object -First 1
    if ($w.mine -lt $MinMs) { continue }
    if (($w.mine - $w.wic) -lt $MinDeltaMs) { continue }
    $max = if ($allow.ContainsKey($g.Name)) { $allow[$g.Name] } else { $DefaultMaxRatio }
    if ($w.ratio -gt $max) { $aSuspect += [pscustomobject]@{ row = $w; max = $max } }
}

# ============================================================================ GATE B
$bSuspect = @(); $unseen = @()
foreach ($r in $rows) {
    if (-not $speed.ContainsKey($r.name)) { $unseen += $r.name; continue }
    $was = [Math]::Max(0.001, $speed[$r.name])
    if ($r.mine -lt $MinMs) { continue }
    if (($r.mine - $was) -lt $MinDeltaMs) { continue }
    $ratio = [Math]::Round($r.mine / $was, 2)
    if ($ratio -gt $MaxSelfRatio) {
        $bSuspect += [pscustomobject]@{ name = $r.name; now = $r.mine; was = $was; ratio = $ratio }
    }
}

if (($aSuspect -or $bSuspect) -and $drift -gt $MaxDrift) {
    Write-Host ("[speed] INCONCLUSIVE - the box moved {0:P0} mid-run (limit {1:P0}); NOT a verdict:" -f $drift, $MaxDrift) -ForegroundColor Yellow
    $aSuspect | ForEach-Object { "    A  {0,-8} {1,5}x  {2}" -f $_.row.ext, $_.row.ratio, $_.row.name }
    $bSuspect | ForEach-Object { "    B  {0,-24} {1,5}x" -f $_.name, $_.ratio }
    Write-Host  "         Re-run on a quieter box." -ForegroundColor Yellow
    exit 0
}

# Guard 6: confirm everything that tripped, with 3x the runs, in one more process.
$confirmNames = @($aSuspect.row.name) + @($bSuspect.name) | Sort-Object -Unique
$again = @{}
if ($confirmNames) {
    Write-Host ("[speed] confirming {0} suspect(s) at {1} runs..." -f $confirmNames.Count, ($Runs * 3)) -ForegroundColor DarkGray
    $again = Measure-Decode (@($confirmNames | ForEach-Object { Join-Path $Corpus $_ })) ($Runs * 3)
}

# The confirmation pass is only worth anything if the box held still FOR it. Re-time the
# reference once more: run 1 of the 2026-08-18 stability check drifted 29% and its "confirmed"
# AVIF number was 377 ms against a true ~150 ms, i.e. the retry re-measured the same load
# rather than clearing it.
if ($confirmNames) {
    $driftConfirm = (Measure-Decode @($refFile) ($Runs * 2)).Values | Select-Object -First 1
    $cDrift = if ($driftBefore -and $driftConfirm) {
        [Math]::Abs($driftConfirm - $driftBefore) / [Math]::Max($driftBefore, $driftConfirm)
    } else { 0 }
    if ($cDrift -gt $MaxDrift) {
        Write-Host ("[speed] INCONCLUSIVE - the box moved {0:P0} during confirmation; NOT a verdict." -f $cDrift) -ForegroundColor Yellow
        Write-Host  "         Re-run on a quieter box." -ForegroundColor Yellow
        exit 0
    }
}

$aBad = @()
foreach ($sp in $aSuspect) {
    $now = if ($again.ContainsKey($sp.row.name)) { $again[$sp.row.name] } else { $sp.row.mine }
    $w = Measure-Wic (Join-Path $Corpus $sp.row.name) ($Runs * 3)
    if ($null -eq $w -or $w -le 0) { continue }
    $ratio = [Math]::Round($now / $w, 2)
    if ($ratio -gt $sp.max -and ($now - $w) -ge $MinDeltaMs) {
        $aBad += [pscustomobject]@{ ext = $sp.row.ext; name = $sp.row.name; mine = $now; wic = $w; ratio = $ratio; max = $sp.max }
    } else {
        Write-Host ("           A {0,-8} cleared on retry ({1}x)" -f $sp.row.ext, $ratio) -ForegroundColor DarkGray
    }
}
$bBad = @()
foreach ($sp in $bSuspect) {
    $now = if ($again.ContainsKey($sp.name)) { $again[$sp.name] } else { $sp.now }
    $ratio = [Math]::Round($now / $sp.was, 2)
    if ($ratio -gt $MaxSelfRatio -and ($now - $sp.was) -ge $MinDeltaMs) {
        $bBad += [pscustomobject]@{ name = $sp.name; now = $now; was = $sp.was; ratio = $ratio }
    } else {
        Write-Host ("           B {0,-24} cleared on retry ({1}x)" -f $sp.name, $ratio) -ForegroundColor DarkGray
    }
}

if ($aBad) {
    Write-Host ""
    Write-Host ("[gate A] {0} format(s) MATERIALLY SLOWER than Windows' own codec:" -f $aBad.Count) -ForegroundColor Red
    $aBad | Sort-Object ratio -Descending | ForEach-Object {
        "  {0,-8} {1,5}x  (ours {2:N1} ms vs WIC {3:N1} ms)  allowed {4}x   {5}" -f $_.ext, $_.ratio, $_.mine, $_.wic, $_.max, $_.name
    }
    Write-Host "  Either make it faster, or add a baselined line WITH A REASON:" -ForegroundColor Yellow
    Write-Host ("    {0,-8} {1,5} # why being slower than WIC is correct here" -f $aBad[0].ext, ([Math]::Ceiling($aBad[0].ratio * 10) / 10)) -ForegroundColor Yellow
    $exitCode = 1
} else {
    Write-Host "[gate A] OK - no format is unaccountably slower than Windows." -ForegroundColor Green
}

if ($speed.Count -eq 0) {
    Write-Host ("[gate B] no baseline yet ({0}) - run -UpdateBaseline on an idle box." -f (Split-Path $SpeedBaseline -Leaf)) -ForegroundColor Yellow
} elseif ($bBad) {
    Write-Host ""
    Write-Host ("[gate B] {0} sample(s) SLOWER THAN THEIR OWN BASELINE:" -f $bBad.Count) -ForegroundColor Red
    $bBad | Sort-Object ratio -Descending | ForEach-Object {
        "  {0,-26} {1,5}x   {2:N1} ms now, {3:N1} ms baselined" -f $_.name, $_.ratio, $_.now, $_.was
    }
    Write-Host "  Something fell off a fast path. Fix it, or re-baseline DELIBERATELY." -ForegroundColor Yellow
    $exitCode = 1
} else {
    Write-Host ("[gate B] OK - {0} samples within {1}x of their own baseline." -f $rows.Count, $MaxSelfRatio) -ForegroundColor Green
}
if ($unseen) {
    Write-Host ("[gate B] {0} sample(s) not in the baseline (new formats?): {1}" -f
        $unseen.Count, (($unseen | Select-Object -First 6) -join ', ')) -ForegroundColor DarkGray
}
exit $exitCode
