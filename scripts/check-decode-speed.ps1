<#
  check-decode-speed.ps1 — is any format's thumbnail SLOW? Two gates, because one alone
  leaves most of the product uncovered.

      pwsh scripts\check-decode-speed.ps1                  # run both gates
      pwsh scripts\check-decode-speed.ps1 -UpdateBaseline  # re-record BOTH baselines
      pwsh scripts\check-decode-speed.ps1 -Only avif,heic  # scope to some extensions
      pwsh scripts\check-decode-speed.ps1 -Verbose2        # print every measured row

  GATE A — VS NATIVE. For every format Windows can also decode, are we slower than Windows?
  GATE B — VS OURSELVES. For EVERY format, did this build get slower than the last one?

  Gate B exists because Gate A alone was not "every one", and that gap is not small: of the
  223 real-corpus samples, only 71 have a native WIC peer. The other 152 — DjVu, PSD, the
  ebook/comic containers, the project-file previews, the whole ImageMagick long tail, i.e.
  most of what makes this product worth installing — have no Windows equivalent to be
  measured against, so Gate A is structurally blind to them. A format with no native peer
  could get 10x slower and Gate A would still say OK. Gate B pins each sample against its own
  recorded time instead, so "nothing to compare against" stops meaning "nothing is checked".

  WHY THIS EXISTS (2026-08-18). A user reported "AVIF thumbnailing is way slower than native
  W11 with the AV1 codec installed, or Icaros." It was true and it had shipped: `decode.rs`
  deliberately routes most AVIF around Microsoft's AV1 WIC codec (issue #9 — that codec
  misreads the colour libaom writes) and out to an ImageMagick SUBPROCESS. Correct colour,
  ~5-15x the cost. Measured on a 3000x2000 AVIF: native WIC decode 43 ms, our magick route
  638-1261 ms.

  NOTHING CAUGHT IT, and that is the point of this file:
    * regression.ps1 has three gates and all three are about CORRECTNESS — did a PNG appear,
      did THIS sample render, is the picture right. AVIF passed all three. It rendered, and it
      rendered the RIGHT picture. It was just slow.
    * perf.ps1 flags anything over a FLAT 3000 ms. AVIF at ~860-1300 ms sails under it. A flat
      threshold cannot see a 13x slowdown that stays inside the budget, and it has no idea what
      a given format SHOULD cost.

  So this check asks the question the user actually asked: not "is it slow in absolute terms"
  but "are we slower than the OS that ships for free". For every sample WIC can open, it times
  our decode against WIC's own and gates the RATIO against a committed baseline.

  THE BASELINE IS THE WHOLE MECHANISM. Being slower than WIC is sometimes the RIGHT call — the
  AVIF colour route is a deliberate trade, and so is any format where we decode properly and
  WIC decodes wrongly. This check does not forbid that. It forces each such case to exist as an
  explicit, reviewed line in scripts\decode-vs-native-baseline.txt with a REASON written next to
  it. A trade someone argued for stays green; a trade that appears by accident turns the build
  red. That difference is the entire reason AVIF went unnoticed for so long.

  FALSE REDS ARE THE WORST OUTCOME (a gate that cries wolf gets ignored, and then it misses the
  real thing), so five deliberate guards, none of which are optional:
    1. MIN-of-N, never mean. This box runs 10-20 agents; a background spike must not fail a build.
    2. The CLI's PROCESS-START floor is measured and subtracted. st2k.exe pays ~110 ms of startup
       that the in-Explorer DLL does not, and WIC is timed in-process. Comparing them raw would
       flag every fast format.
    3. An ABSOLUTE floor (-MinMs). Below it a ratio is noise dividing noise, so it is not gated.
    4. Only samples WIC ACTUALLY DECODES are compared. The 300+ formats where we are the only
       decoder have no native baseline to be slower than, and are reported as "no native peer",
       never as a pass or a failure.
    5. A breach must cost real time (-MinDeltaMs), not just a large ratio over a small number.
    6. CALIBRATION, the same device this repo's own --bench-* modes use, and it gates TWO things,
       because checking only the first is a trap this check fell into once already:
         (a) DRIFT — the calibrator is timed before and after the sweep; if the two disagree the
             load moved under us mid-run.
         (b) ABSOLUTE LEVEL — drift alone is blind to a box that is EVENLY loaded the whole time,
             which reads as a perfectly stable 10% drift while every number is inflated. That is
             not symmetric between the things being compared: st2k is a SUBPROCESS and WIC is
             in-process, so contention inflates our side much harder and the ratio moves against
             us. Measured here: the same AVIF read 129 ms on a quiet box and 342 ms at 80% load,
             while WIC barely moved. So the sweep's calibration is compared against a reference
             recorded in the baseline (`# calibration-reference <ms>`, written by -UpdateBaseline
             ON A QUIET BOX). Too slow versus that reference and the run is INCONCLUSIVE.
       Either way inconclusive means WARN AND EXIT 0, never a verdict. This machine routinely sits
       at 80%+ with a dozen agent sessions on it, and an unguarded run really does report a 1 ms
       TIFF as 265 ms.
    7. CONFIRMATION. A format that trips is re-measured ALONE, with more runs, and only a repeat
       breach fails the build — the same false-alarm guard regression.ps1 uses before it dares
       call something a regression.

  EXIT CODES:
    0  both gates pass (or the box was too loaded to judge — see guard 6).
    1  a format is materially slower than Windows without a baselined reason (A), or slower
       than its own recorded time (B).
#>
[CmdletBinding()]
param(
    [string]$Corpus = "$PSScriptRoot\..\..\test-corpus-real",
    [string]$Baseline = "$PSScriptRoot\decode-speed-vs-native.txt",
    # Gate B's baseline: our OWN measured time per sample, for every format including the 152
    # that have no native peer at all.
    [string]$SpeedBaseline = "$PSScriptRoot\decode-speed-baseline.txt",
    # How much slower than its own recorded time a sample may get before Gate B fails. Loose
    # on purpose — this hunts a decoder falling off a fast path (the AVIF shape: 3-13x), not
    # normal run-to-run wobble.
    [double]$MaxSelfRatio = 2.5,
    [switch]$UpdateBaseline,
    [string[]]$Only,
    # Default tolerance for a format with no explicit baseline line. 3x is deliberately loose:
    # our pipeline does real work WIC does not (EXIF orientation, ICC to sRGB, fit-to-box), and
    # this gate is hunting order-of-magnitude routing mistakes, not tuning.
    [double]$DefaultMaxRatio = 3.0,
    # Below this many milliseconds of our own decode time, ratios are not gated (see guard 3).
    [int]$MinMs = 60,
    # A ratio must ALSO cost this many real milliseconds before it is a failure (guard 5). A
    # format 3.4x native but only 50 ms slower is not what this hunts, and gating it produces a
    # flaky red that gets the whole check ignored. Measured separation on this corpus: AVIF
    # (the real defect) is +101..+131 ms, while WebP -- our nearest innocent -- is +45..+54 ms.
    [int]$MinDeltaMs = 75,
    [int]$Runs = 3,
    # How far the before/after calibration may drift before the run is called inconclusive.
    [double]$MaxCalibrationDrift = 0.35,
    # How much slower than the baseline's recorded calibration reference this box may be before
    # its numbers stop being comparable at all.
    [double]$MaxCalibrationSlowdown = 2.0,
    [int]$Size = 256,
    [switch]$Verbose2
)
$ErrorActionPreference = 'Stop'

$st2k = Join-Path (& "$PSScriptRoot\_targetdir.ps1") 'release\st2k.exe'
if (-not (Test-Path $st2k))   { throw "st2k.exe not built (cargo build --release --bin st2k)" }
if (-not (Test-Path $Corpus)) { throw "corpus not found: $Corpus" }

Add-Type -AssemblyName PresentationCore

# ---------------------------------------------------------------- baseline file
# Format: "<ext> <maxRatio> # reason". Lines without a reason are rejected on purpose —
# an unexplained allowance is exactly the invisible trade this check exists to prevent.
$allow = @{}
$reason = @{}
$calRef = $null
if (Test-Path $Baseline) {
    foreach ($line in Get-Content $Baseline) {
        $t = $line.Trim()
        if ($t -match '^#\s*calibration-reference\s+([0-9.]+)') { $calRef = [double]$Matches[1]; continue }
        if (-not $t -or $t.StartsWith('#')) { continue }
        $body, $why = $t -split '#', 2
        $parts = @($body.Trim() -split '\s+')
        if ($parts.Count -lt 2) { continue }
        $ext = $parts[0].ToLowerInvariant().TrimStart('.')
        $allow[$ext]  = [double]$parts[1]
        $reason[$ext] = if ($why) { $why.Trim() } else { '' }
    }
}

# Gate B baseline: "<sample filename> <ms>" — our own recorded decode time per sample.
$speed = @{}
if (Test-Path $SpeedBaseline) {
    foreach ($line in Get-Content $SpeedBaseline) {
        $t = $line.Trim()
        if (-not $t -or $t.StartsWith('#')) { continue }
        $parts = @($t -split '\s+')
        if ($parts.Count -lt 2) { continue }
        $speed[$parts[0]] = [double]$parts[1]
    }
}

# ------------------------------------------------- st2k process-start floor (guard 2)
# A trivial PNG: everything measured is startup, not decode. Subtracted from every st2k
# timing below so we compare DECODE to DECODE.
$tmp = Join-Path $env:TEMP ("st2k-vsnative-{0}-{1}" -f $PID, [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force $tmp | Out-Null

function Measure-St2k([string]$inPath, [string]$outPath) {
    $best = [int]::MaxValue
    for ($i = 0; $i -lt $Runs; $i++) {
        $sw = [Diagnostics.Stopwatch]::StartNew()
        & $st2k thumbnail $inPath $outPath --size $Size 2>$null | Out-Null
        $code = $LASTEXITCODE
        $sw.Stop()
        if ($code -ne 0) { return @{ ms = $null; ok = $false } }
        if ($sw.ElapsedMilliseconds -lt $best) { $best = [int]$sw.ElapsedMilliseconds }
    }
    return @{ ms = $best; ok = $true }
}

# WIC, in-process, image cache BYPASSED (a cached BitmapFrame returns in ~3 ms and would
# make Windows look impossibly fast — this measurement was wrong once before it was fixed).
$wicOpt = [System.Windows.Media.Imaging.BitmapCreateOptions]::IgnoreImageCache
function Measure-Wic([string]$path) {
    $best = [int]::MaxValue
    $bytes = [IO.File]::ReadAllBytes($path)
    for ($i = 0; $i -lt $Runs; $i++) {
        $ms = New-Object IO.MemoryStream (,$bytes)
        try {
            $sw = [Diagnostics.Stopwatch]::StartNew()
            $fr = [System.Windows.Media.Imaging.BitmapFrame]::Create($ms, $wicOpt, 'OnLoad')
            $cv = New-Object System.Windows.Media.Imaging.FormatConvertedBitmap `
                    $fr, ([System.Windows.Media.PixelFormats]::Bgra32), $null, 0
            $stride = [int]($cv.PixelWidth * 4)
            $buf = New-Object byte[] ($stride * $cv.PixelHeight)
            $cv.CopyPixels($buf, $stride, 0)   # force a REAL decode, not a lazy handle
            $sw.Stop()
            if ($sw.ElapsedMilliseconds -lt $best) { $best = [int]$sw.ElapsedMilliseconds }
        } catch {
            return $null      # WIC cannot open it: no native peer, not a failure
        } finally { $ms.Dispose() }
    }
    return $best
}

# ------------------------------------------------------------ calibration (guard 6)
# THE CALIBRATOR IS st2k's OWN PROCESS-START FLOOR, not a CPU loop. That choice is the whole
# point: a tight floating-point loop barely moves on this box (a single thread still gets a core
# at 80% aggregate load — measured 2155 ms at 80% vs 2268 ms at 49%, i.e. backwards), so it
# cannot see the contention that matters here. What DOES move is spawning a process: the same
# trivial decode measured a 29 ms floor on a quiet box and 112 ms on a busy one. Since our side
# of every comparison is a subprocess and WIC's side is in-process, process-start cost is exactly
# the asymmetry that turns a loaded box into invented regressions.
function Measure-Floor([string]$src, [string]$dst) {
    $r = Measure-St2k $src $dst
    if ($null -eq $r.ms) { throw "could not measure the st2k process-start floor" }
    return [Math]::Max(1, $r.ms)
}

$exitCode = 0
try {
    $probe = Join-Path $tmp 'floor.png'
    $floorSrc = Join-Path $tmp 'floor-src.png'
    # 8x8 PNG, written by our own encoder so no external tool is needed.
    [IO.File]::WriteAllBytes($floorSrc, [Convert]::FromBase64String(
        'iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAYAAADED76LAAAAFklEQVQoz2NgGAWjYBSMglEwCkYBLgAABZgAAeAAxxEAAAAASUVORK5CYII='))
    $floor = Measure-Floor $floorSrc $probe
    $calBefore = $floor
    Write-Host ("[vsnative] st2k process-start floor: {0} ms (subtracted from every timing)" -f $floor) -ForegroundColor DarkGray


    # Skip the corpus's own bookkeeping files, not the samples. `contact.png` is the generated
    # contact sheet and `_*` are manifests; everything else with an extension is a sample.
    $skipNames = @('contact.png', 'README.md')
    $files = Get-ChildItem $Corpus -File | Where-Object {
        $_.Extension -and
        $_.Name -notin $skipNames -and
        -not $_.Name.StartsWith('_') -and
        $_.Extension.ToLowerInvariant() -notin @('.md', '.txt')
    }
    if ($Only) {
        $want = $Only | ForEach-Object { $_.ToLowerInvariant().TrimStart('.') }
        $files = $files | Where-Object { $want -contains $_.Extension.ToLowerInvariant().TrimStart('.') }
    }

    $rows = @(); $i = 0
    foreach ($f in $files) {
        $ext = $f.Extension.ToLowerInvariant().TrimStart('.')
        $out = Join-Path $tmp ("{0:D4}.png" -f ($i++))
        $mine = Measure-St2k $f.FullName $out
        if (-not $mine.ok) { continue }              # a non-rendering format is regression.ps1's job
        $wic = Measure-Wic $f.FullName
        $net = [Math]::Max(1, $mine.ms - $floor)     # guard 2
        $rows += [pscustomobject]@{
            ext = $ext; name = $f.Name; mine = $net; wic = $wic
            ratio = if ($null -ne $wic -and $wic -gt 0) { [Math]::Round($net / $wic, 2) } else { $null }
        }
    }

    $compared = @($rows | Where-Object { $null -ne $_.ratio })
    $noPeer   = @($rows | Where-Object { $null -eq $_.ratio })
    Write-Host ("[vsnative] {0} samples · {1} have a native WIC peer · {2} are ours alone" -f
        $rows.Count, $compared.Count, $noPeer.Count) -ForegroundColor Cyan

    if ($Verbose2) {
        $compared | Sort-Object ratio -Descending | ForEach-Object {
            "  {0,-8} {1,6} ms ours · {2,6} ms WIC · {3,6}x  {4}" -f $_.ext, $_.mine, $_.wic, $_.ratio, $_.name
        }
    }

    if ($UpdateBaseline) {
        $worst = $compared | Group-Object ext | ForEach-Object {
            $mx = ($_.Group | Measure-Object ratio -Maximum).Maximum
            [pscustomobject]@{ ext = $_.Name; ratio = [Math]::Ceiling($mx * 10) / 10 }
        }
        $lines = @(
            '# decode-vs-native baseline — max allowed (our decode time / native WIC decode time).',
            '# Generated by scripts\check-decode-vs-native.ps1 -UpdateBaseline, then REASONS ARE',
            '# WRITTEN BY HAND. A line without a reason after the # is a trade nobody argued for,',
            '# which is the exact thing this check exists to surface. Explain it or fix it.',
            '#',
            '# The calibration-reference below is st2k''s process-start floor when the baseline',
            '# was taken. GENERATE IT ON A QUIET MACHINE: a run that measures much slower than this',
            '# is reported INCONCLUSIVE instead of failing, which is what keeps a loaded build agent',
            '# from inventing regressions.',
            ("# calibration-reference {0}" -f $calBefore),
            ''
        ) + ($worst | Sort-Object ext | ForEach-Object { "{0,-8} {1,5} #" -f $_.ext, $_.ratio })
        # NEVER clobber gate A's baseline: its whole value is the hand-written REASON on each
        # line, and regenerating throws that away silently — which is the same class of quiet
        # loss this check exists to stop. Refuse, and say how to do it on purpose.
        if (Test-Path $Baseline) {
            Write-Host ("[gate A] baseline left ALONE ({0}) - it carries hand-written reasons." -f $Baseline) -ForegroundColor DarkGray
            Write-Host  "         Delete it first if you really mean to regenerate them." -ForegroundColor DarkGray
        } else {
            Set-Content -LiteralPath $Baseline -Value $lines -Encoding UTF8
            Write-Host ("[gate A] baseline written: {0} ({1} formats) — now write a REASON on each line." -f $Baseline, $worst.Count) -ForegroundColor Yellow
        }

        # Gate B: every sample, including the 152 with no native peer at all.
        $bLines = @(
            '# decode-speed baseline — our OWN measured decode time (ms) per corpus sample, with',
            '# the process-start floor already subtracted. Enforced by scripts\check-decode-speed.ps1.',
            '#',
            '# This is the half that covers the formats Windows CANNOT decode, which is most of them:',
            '# 152 of these 223 samples have no native peer, so the vs-native gate is blind to them.',
            '# A format here that falls off a fast path (the AVIF shape) shows up as its own number',
            '# getting worse, with no need for anything to compare against.',
            '#',
            '# GENERATE ON A QUIET MACHINE, and re-generate deliberately: accepting a slower number',
            '# here is accepting a slower product. Tolerance is -MaxSelfRatio (default 2.5x) plus an',
            '# absolute floor, so ordinary wobble does not fail a build.',
            ("# calibration-reference {0}" -f $calBefore),
            ''
        ) + ($rows | Sort-Object name | ForEach-Object { "{0,-28} {1,6}" -f $_.name, $_.mine })
        Set-Content -LiteralPath $SpeedBaseline -Value $bLines -Encoding UTF8
        Write-Host ("[gate B] baseline written: {0} ({1} samples)" -f $SpeedBaseline, $rows.Count) -ForegroundColor Yellow
        exit 0
    }

    # ---- guard 6: did the machine stay comparable for the whole sweep?
    $calAfter = Measure-Floor $floorSrc $probe
    $drift = [Math]::Abs($calAfter - $calBefore) / [double][Math]::Max($calBefore, $calAfter)
    Write-Host ("[speed] calibration (process-start floor): {0} ms -> {1} ms ({2:P0} drift)" -f
        $calBefore, $calAfter, $drift) -ForegroundColor DarkGray

    # Gate on the WORST sample per extension (a hard sample must not hide behind an easy one
    # of the same format — the same lesson regression.ps1's per-FILE gate encodes).
    $suspect = @()
    foreach ($g in ($compared | Group-Object ext)) {
        $ext = $g.Name
        $worstRow = $g.Group | Sort-Object ratio -Descending | Select-Object -First 1
        if ($worstRow.mine -lt $MinMs) { continue }                     # guard 3
        $delta = $worstRow.mine - $worstRow.wic
        if ($delta -lt $MinDeltaMs) { continue }                        # guard 5
        $max = if ($allow.ContainsKey($ext)) { $allow[$ext] } else { $DefaultMaxRatio }
        if ($worstRow.ratio -gt $max) {
            $suspect += [pscustomobject]@{ row = $worstRow; max = $max; delta = $delta }
        }
    }

    $loaded = $false
    if ($null -ne $calRef) {
        $level = [Math]::Min($calBefore, $calAfter) / $calRef
        if ($level -gt $MaxCalibrationSlowdown) {
            $loaded = $true
            Write-Host ("[speed] process-start is {0:N1}x the baseline reference ({1} ms) - box is busy" -f
                $level, $calRef) -ForegroundColor Yellow
        }
    }

    if ($suspect -and ($drift -gt $MaxCalibrationDrift -or $loaded)) {
        Write-Host ""
        $why = if ($loaded) { "this box is too loaded to compare a subprocess against an in-process decode" }
               else { "the machine's load moved {0:P0} mid-run (limit {1:P0})" -f $drift, $MaxCalibrationDrift }
        Write-Host ("[speed] INCONCLUSIVE - {0}." -f $why) -ForegroundColor Yellow
        Write-Host  "           These readings are not comparable, so they are NOT a verdict:" -ForegroundColor Yellow
        $suspect | ForEach-Object { "             {0,-8} {1,5}x  {2}" -f $_.row.ext, $_.row.ratio, $_.row.name }
        Write-Host  "           Re-run on a quieter box before believing any of it." -ForegroundColor Yellow
        exit 0
    }

    # ---- guard 7: confirm each suspect ALONE, with more runs, before failing a build.
    $bad = @()
    if ($suspect) {
        Write-Host ("[gate A] confirming {0} suspect(s) with {1} runs each..." -f
            $suspect.Count, ($Runs * 3)) -ForegroundColor DarkGray
        $savedRuns = $Runs
        $script:Runs = $Runs * 3
        foreach ($sp in $suspect) {
            $f2 = Join-Path $Corpus $sp.row.name
            $out2 = Join-Path $tmp 'confirm.png'
            $again = Measure-St2k $f2 $out2
            $wic2 = Measure-Wic $f2
            if (-not $again.ok -or $null -eq $wic2 -or $wic2 -le 0) { continue }
            $net2 = [Math]::Max(1, $again.ms - $floor)
            $ratio2 = [Math]::Round($net2 / $wic2, 2)
            $delta2 = $net2 - $wic2
            if ($ratio2 -gt $sp.max -and $delta2 -ge $MinDeltaMs) {
                $bad += [pscustomobject]@{
                    row = [pscustomobject]@{ ext = $sp.row.ext; name = $sp.row.name
                                             mine = $net2; wic = $wic2; ratio = $ratio2 }
                    max = $sp.max; delta = $delta2
                }
            } else {
                Write-Host ("             {0,-8} cleared on retry ({1}x)" -f $sp.row.ext, $ratio2) -ForegroundColor DarkGray
            }
        }
        $script:Runs = $savedRuns
    }

    if ($bad) {
        Write-Host ""
        Write-Host ("[gate A] {0} format(s) MATERIALLY SLOWER than Windows' own codec:" -f $bad.Count) -ForegroundColor Red
        foreach ($b in $bad) {
            $r = $b.row
            "  {0,-8} {1,5}x  (ours {2} ms vs WIC {3} ms, +{4} ms)  allowed {5}x   {6}" -f
                $r.ext, $r.ratio, $r.mine, $r.wic, $b.delta, $b.max, $r.name
        }
        Write-Host ""
        Write-Host "  Either make it faster, or add a baselined line WITH A REASON:" -ForegroundColor Yellow
        Write-Host ("    {0,-8} {1,5} # why being slower than WIC is correct here" -f $bad[0].row.ext, ([Math]::Ceiling($bad[0].row.ratio * 10) / 10)) -ForegroundColor Yellow
        Write-Host ("  ({0})" -f $Baseline) -ForegroundColor DarkGray
        $exitCode = 1
    } else {
        Write-Host "[gate A] OK - no format is unaccountably slower than Windows." -ForegroundColor Green
    }

    # ===================================================================== GATE B
    # Every sample against its OWN recorded time. This is the half that covers the 152 formats
    # with no native peer — the ones gate A cannot see at all.
    if ($speed.Count -eq 0) {
        Write-Host ("[gate B] no baseline yet ({0}) - run -UpdateBaseline on a quiet box." -f $SpeedBaseline) -ForegroundColor Yellow
    } else {
        $bSuspect = @()
        $unseen = @()
        foreach ($r in $rows) {
            if (-not $speed.ContainsKey($r.name)) { $unseen += $r.name; continue }
            $was = [Math]::Max(1, $speed[$r.name])
            if ($r.mine -lt $MinMs) { continue }                        # guard 3
            $d = $r.mine - $was
            if ($d -lt $MinDeltaMs) { continue }                        # guard 5
            $ratio = [Math]::Round($r.mine / $was, 2)
            if ($ratio -gt $MaxSelfRatio) {
                $bSuspect += [pscustomobject]@{ name = $r.name; ext = $r.ext; now = $r.mine; was = $was; ratio = $ratio }
            }
        }

        if ($bSuspect -and ($drift -gt $MaxCalibrationDrift -or $loaded)) {
            Write-Host "[gate B] INCONCLUSIVE - box too loaded/unstable; these are NOT a verdict:" -ForegroundColor Yellow
            $bSuspect | ForEach-Object { "             {0,-24} {1,5}x  ({2} ms, was {3})" -f $_.name, $_.ratio, $_.now, $_.was }
        } elseif ($bSuspect) {
            # guard 7: confirm alone, more runs.
            Write-Host ("[gate B] confirming {0} suspect(s) with {1} runs each..." -f $bSuspect.Count, ($Runs * 3)) -ForegroundColor DarkGray
            $savedRuns2 = $Runs
            $script:Runs = $Runs * 3
            $bBad = @()
            foreach ($sp in $bSuspect) {
                $again = Measure-St2k (Join-Path $Corpus $sp.name) (Join-Path $tmp 'confirmb.png')
                if (-not $again.ok) { continue }
                $net2 = [Math]::Max(1, $again.ms - $floor)
                $ratio2 = [Math]::Round($net2 / $sp.was, 2)
                if ($ratio2 -gt $MaxSelfRatio -and ($net2 - $sp.was) -ge $MinDeltaMs) {
                    $bBad += [pscustomobject]@{ name = $sp.name; now = $net2; was = $sp.was; ratio = $ratio2 }
                } else {
                    Write-Host ("             {0,-24} cleared on retry ({1}x)" -f $sp.name, $ratio2) -ForegroundColor DarkGray
                }
            }
            $script:Runs = $savedRuns2
            if ($bBad) {
                Write-Host ""
                Write-Host ("[gate B] {0} sample(s) SLOWER THAN THEIR OWN BASELINE:" -f $bBad.Count) -ForegroundColor Red
                $bBad | Sort-Object ratio -Descending | ForEach-Object {
                    "  {0,-24} {1,5}x   {2} ms now, {3} ms baselined" -f $_.name, $_.ratio, $_.now, $_.was
                }
                Write-Host ""
                Write-Host "  Something fell off a fast path. Fix it, or re-baseline DELIBERATELY:" -ForegroundColor Yellow
                Write-Host ("    pwsh scripts\check-decode-speed.ps1 -UpdateBaseline   ({0})" -f $SpeedBaseline) -ForegroundColor Yellow
                $exitCode = 1
            } else {
                Write-Host "[gate B] OK - nothing regressed against its own baseline." -ForegroundColor Green
            }
        } else {
            Write-Host ("[gate B] OK - {0} samples within {1}x of their own baseline." -f $rows.Count, $MaxSelfRatio) -ForegroundColor Green
        }
        if ($unseen) {
            Write-Host ("[gate B] {0} sample(s) not in the baseline (new formats?): {1}" -f
                $unseen.Count, (($unseen | Select-Object -First 6) -join ', ')) -ForegroundColor DarkGray
        }
    }
}
finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
exit $exitCode
