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

  ── WHY GATE A GETS THE TIERED CORPUS, AND GATE B DOES NOT (2026-08-19) ─────────────────────
  Both gates ran only on the real corpus: ONE sample per format, at whatever size that sample
  happened to be. Every routing mistake found in 2026-08-19's sweep was therefore invisible to
  this script and was found by the REPORT instead — BMP at 11.7x native, GIF at 6.1x, DDS at
  7.3x, AVIF at 3.0x, all of them at 12 MP and all of them unremarkable at 0.08 MP, because
  the thing that goes wrong (materialise every pixel, then shrink) costs nothing until there
  are pixels to materialise. A gate that cannot see the size where the bug lives is not
  guarding the fix.

  So Gate A now also sweeps `test-corpus-speed`, whose whole purpose is one identical picture
  at small / medium / large, so that the 12 MP sample governs its format and a fast small one
  cannot hide behind it.

  That last part was NOT free, and it is the reason to read the GATE A block below before
  changing it: the "worst sample per extension" rule picked the worst ratio and only THEN
  applied the noise floors, so the first thing this corpus did was prove the gate could hold a
  slow format and report a clean run. Fixed there.

  Gate B deliberately does NOT, and the reason is a practical one. Gate B compares against a
  RECORDED number, so it needs a baseline captured on an IDLE machine — and a development box
  running builds is not reliably idle, which is why the baseline this file enforces goes stale
  and stays stale. Gate A needs no baseline at all: it times WIC and us in the same process, in the
  same run, under the same load, so the LOAD DIVIDES OUT of the ratio. That is what makes it
  the half that still works on a busy box, and the half worth extending.

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
    5. DRIFT, checked TWICE and in BOTH execution worlds. Two reference files - one decoded
       in-process, one through the ImageMagick subprocess - are re-timed after the sweep and
       again after the confirmation pass; the WORST drift of the pair judges the run, and past
       -MaxDrift it is INCONCLUSIVE (warn, exit 0) rather than a verdict. Both worlds are
       load-bearing: subprocess decodes inflate ~3x under background load while in-process
       ones barely move, so a run once read "drift 1%" while five magick-tier formats sat at
       2.5-2.9x their baselines - pure load, which then "confirmed" under the same load. And
       both CHECKS are load-bearing: a 29% drift run once "confirmed" a 377 ms AVIF against
       its true ~150 ms because the retry re-measured the same spike.
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
    # The SIZE-TIERED corpus (scripts\build-speed-corpus.ps1), swept by GATE A ONLY. See the
    # "WHY GATE A GETS THE TIERED CORPUS" note below. Absent = skipped, loudly, since it is
    # 3.3 GB and deliberately not in git.
    [string]$TieredCorpus = "$PSScriptRoot\..\..\test-corpus-speed",
    # Which tiers to fold in. `large` is the default because every routing mistake found so
    # far lived at 12 MP and vanished at 0.08 MP; `all` triples the sweep for the two tiers
    # that have never yet been the one to catch something.
    [ValidateSet('none', 'large', 'medium', 'all')]
    [string]$Tiered = 'large',
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
# Shared with scripts\speed-report.ps1 — one definition of "how do we time a decode", so the
# numbers the report publishes are the same ones this gate enforces.
. "$PSScriptRoot\decode-speed-lib.ps1"

$exitCode = 0

$files = Get-SpeedSamples $Corpus $Only
$tieredNames = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)

# The size-tiered corpus, gate A only. It is regenerable and 3.3 GB, so it is NOT in git and a
# clean checkout simply will not have it — SKIP LOUDLY rather than silently, because a skipped
# check that reads like a passing one is the failure mode this whole file exists to avoid.
if ($Tiered -ne 'none') {
    $wantTiers = if ($Tiered -eq 'all') { @('small', 'medium', 'large') } else { @($Tiered) }
    if (Test-Path $TieredCorpus) {
        $extra = @(Get-SpeedSamples $TieredCorpus $Only | Where-Object {
            # `<ext>-<tier>.<ext>`, the layout build-speed-corpus.ps1 writes.
            $tier = ($_.BaseName -split '-')[-1]
            $wantTiers -contains $tier.ToLowerInvariant()
        })
        # `bench-decode` reports per FILE NAME, and so does the baseline, so two corpora
        # sharing a name would silently overwrite each other's timing and the gate would
        # judge one file's number against the other's. Today the layouts cannot collide
        # (`<ext>-<tier>.<ext>` against `sample-*.<ext>`); this refuses rather than trusts it.
        $realNames = [System.Collections.Generic.HashSet[string]]::new(
            [string[]]@($files.Name), [StringComparer]::OrdinalIgnoreCase)
        $clash = @($extra | Where-Object { $realNames.Contains($_.Name) })
        if ($clash) {
            Write-Host ("[speed] {0} tiered sample(s) share a NAME with the real corpus and were dropped: {1}" -f
                $clash.Count, (($clash.Name | Select-Object -First 4) -join ', ')) -ForegroundColor Yellow
            $extra = @($extra | Where-Object { -not $realNames.Contains($_.Name) })
        }
        foreach ($e in $extra) { [void]$tieredNames.Add($e.Name) }
        $files = @($files) + $extra
        Write-Host ("[speed] + {0} size-tiered sample(s) ({1}) for gate A" -f $extra.Count, ($wantTiers -join '/')) -ForegroundColor DarkGray
    } else {
        Write-Host ("[speed] NO size-tiered corpus at {0} - gate A sees only whatever size each real sample happens to be." -f $TieredCorpus) -ForegroundColor Yellow
        Write-Host  "        Build it with scripts\build-speed-corpus.ps1 (or pass -Tiered none to stop asking)." -ForegroundColor DarkGray
    }
}

# Guard 5: a fixed reference file, timed before and after, detects the box moving under us.
# TWO drift references, one per execution world. An in-process decode barely moves under
# background load while a SUBPROCESS decode inflates ~3x (spawn + magick init x contention),
# so a single in-process reference is blind to exactly the load that invents regressions in
# the magick-tier formats. Measured 2026-08-19: a run at "drift 1%" (in-process ref stable)
# simultaneously showed five magick formats at 2.5-2.9x their baselines, all load, all of
# which then "confirmed" because the retry re-ran under the same sustained load. The magick
# reference is any sample of a known subprocess format; absent (scoped -Only runs), the
# in-process reference alone still guards.
#
# Checked AFTER the tiered merge, not before: a `-Only` scoped to a format that exists only in
# the tiered corpus has real work to do, and throwing on the real corpus alone refused it.
if (-not $files) { throw "no samples matched" }
#
# Both references are drawn from the REAL corpus only. Drift is a comparison of one run's
# reference against the same run's later reading, so it is meaningless if -Tiered can change
# WHICH file that is: a tiered name sorts ahead of every `sample-*.ext`, so including them
# would silently swap the reference and make two runs' drift figures incomparable.
$refPool = @($files | Where-Object { -not $tieredNames.Contains($_.Name) })
# No real-corpus sample means no drift reference, and `Get-WorstDrift` over nothing returns
# 0.0 — which reads as "the box held perfectly still" and would let guard 5 wave through a run
# it never actually checked. Say so instead.
if (-not $refPool) {
    Write-Host "[speed] no real-corpus sample in scope - NO DRIFT REFERENCE, so guard 5 cannot judge this run." -ForegroundColor Yellow
}
$refFile = ($refPool | Sort-Object Name | Select-Object -First 1).FullName
$magickRef = ($refPool | Where-Object {
    $_.Extension.ToLowerInvariant().TrimStart('.') -in @('xpm', 'sun', 'miff', 'fits', 'cal', 'pcx')
} | Sort-Object Name | Select-Object -First 1).FullName
$refs = @($refFile) + @($magickRef | Where-Object { $_ })
$driftBefore = Measure-Drift $refs ($Runs * 2) $Size

$mine = Measure-Decode ($files.FullName) $Runs $Size
$rows = @()
foreach ($f in $files) {
    if (-not $mine.ContainsKey($f.Name)) { continue }   # undecodable is regression.ps1's job
    $w = Measure-Wic $f.FullName $Runs
    $rows += [pscustomobject]@{
        name = $f.Name; ext = $f.Extension.ToLowerInvariant().TrimStart('.')
        # Carried per row so the confirmation pass and the gate-A re-measure can find the file
        # again. They used to rebuild the path as `Join-Path $Corpus $name`, which is only true
        # while every sample lives in ONE directory.
        path = $f.FullName; tiered = $tieredNames.Contains($f.Name)
        mine = $mine[$f.Name]; wic = $w
        ratio = if ($null -ne $w -and $w -gt 0) { [Math]::Round($mine[$f.Name] / $w, 2) } else { $null }
    }
}

$driftAfter = Measure-Drift $refs ($Runs * 2) $Size
$drift = Get-WorstDrift $driftBefore $driftAfter

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
    # Real corpus only. The tiered samples are a gate-A input; recording them here would put
    # every tiered sample into a file whose whole contract is "captured on an idle box",
    # and gate B would then start failing on the corpus that is hardest to measure quietly.
    $bRows = @($rows | Where-Object { -not $_.tiered })
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
    ) + ($bRows | Sort-Object name | ForEach-Object { "{0,-28} {1,9:N3}" -f $_.name, $_.mine })
    Set-Content -LiteralPath $SpeedBaseline -Value $bLines -Encoding UTF8
    Write-Host ("[gate B] baseline written: {0} ({1} samples)" -f $SpeedBaseline, $bRows.Count) -ForegroundColor Yellow
    exit 0
}

# ============================================================================ GATE A
# Worst sample per extension: a hard sample must not hide behind an easy one of the same
# format — the lesson regression.ps1's per-FILE gate encodes.
$aSuspect = @()
foreach ($g in ($compared | Group-Object ext)) {
    # THE FLOORS COME FIRST, THEN THE PICK. Doing it the other way round — take the worst
    # ratio, then test it against -MinMs / -MinDeltaMs — lets a sample that is EXEMPT shadow
    # one that is not, and skips the whole format in silence.
    #
    # Not hypothetical; it is what adding the tiered corpus found on its first run. `sample.dds`
    # is 15.3 ms against 2.7 ms of WIC: a 5.61x ratio, and 12.6 ms of absolute difference, so
    # the floors correctly say "too small to be the hunt". It also sorts FIRST on ratio, so it
    # was the row the floors were applied to — and `continue` then dropped dds entirely,
    # including `dds-large.dds` at 75.6 ms vs 19.1 ms, a 3.97x with 56 ms behind it and no
    # baselined line to excuse it. The gate reported "no format is unaccountably slower than
    # Windows" while holding exactly such a format in its hand.
    #
    # This was always reachable — any format with two samples of very different sizes could hit
    # it — but the real corpus has one sample per format, so it needed the tiered corpus to
    # become the normal case rather than a latent one.
    $eligible = @($g.Group | Where-Object { $_.mine -ge $MinMs -and ($_.mine - $_.wic) -ge $MinDeltaMs })
    if (-not $eligible) { continue }
    $w = $eligible | Sort-Object ratio -Descending | Select-Object -First 1
    $max = if ($allow.ContainsKey($g.Name)) { $allow[$g.Name] } else { $DefaultMaxRatio }
    if ($w.ratio -gt $max) { $aSuspect += [pscustomobject]@{ row = $w; max = $max } }
}

# ============================================================================ GATE B
$bSuspect = @(); $unseen = @()
foreach ($r in $rows) {
    # Tiered samples are gate A's, and only gate A's — they have no recorded time and are not
    # given one (see -UpdateBaseline above). Skipping them here also keeps them out of
    # `$unseen`, which would otherwise report every tiered sample as a "new format?" each run.
    if ($r.tiered) { continue }
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
# Paths come from the rows, never from `Join-Path $Corpus $name` — with two corpora in play a
# rebuilt path is wrong for every tiered sample, and `Measure-Decode` answers a missing file by
# simply omitting it, so the suspect would fall through to its ORIGINAL reading and "confirm"
# itself. A silent self-confirmation is the worst shape a false red can take.
$byName = @{}
foreach ($r in $rows) { $byName[$r.name] = $r.path }
$confirmNames = @($aSuspect.row.name) + @($bSuspect.name) | Sort-Object -Unique
$again = @{}
if ($confirmNames) {
    Write-Host ("[speed] confirming {0} suspect(s) at {1} runs..." -f $confirmNames.Count, ($Runs * 3)) -ForegroundColor DarkGray
    $again = Measure-Decode (@($confirmNames | ForEach-Object { $byName[$_] })) ($Runs * 3) $Size
}

# The confirmation pass is only worth anything if the box held still FOR it. Re-time the
# reference once more: run 1 of the 2026-08-18 stability check drifted 29% and its "confirmed"
# AVIF number was 377 ms against a true ~150 ms, i.e. the retry re-measured the same load
# rather than clearing it.
if ($confirmNames) {
    $driftConfirm = Measure-Drift $refs ($Runs * 2) $Size
    $cDrift = Get-WorstDrift $driftBefore $driftConfirm
    if ($cDrift -gt $MaxDrift) {
        Write-Host ("[speed] INCONCLUSIVE - the box moved {0:P0} during confirmation; NOT a verdict." -f $cDrift) -ForegroundColor Yellow
        Write-Host  "         Re-run on a quieter box." -ForegroundColor Yellow
        exit 0
    }
}

$aBad = @()
foreach ($sp in $aSuspect) {
    $now = if ($again.ContainsKey($sp.row.name)) { $again[$sp.row.name] } else { $sp.row.mine }
    $w = Measure-Wic $sp.row.path ($Runs * 3)
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
    Write-Host ("[gate B] OK - {0} samples within {1}x of their own baseline." -f
        @($rows | Where-Object { -not $_.tiered }).Count, $MaxSelfRatio) -ForegroundColor Green
}
if ($unseen) {
    Write-Host ("[gate B] {0} sample(s) not in the baseline (new formats?): {1}" -f
        $unseen.Count, (($unseen | Select-Object -First 6) -join ', ')) -ForegroundColor DarkGray
}
exit $exitCode
