<#
  speed-report.ps1 — the stored speed record: every format we can measure, at every size we can
  build it, timed against Windows' own codec, written to a file you can read and diff.

      pwsh scripts\speed-report.ps1                 # measure + write docs\SPEED-BASELINE.md
      pwsh scripts\speed-report.ps1 -Only avif,webp # scope it
      pwsh scripts\speed-report.ps1 -Runs 9         # more repeats, less noise
      pwsh scripts\speed-report.ps1 -NoWrite        # print, change nothing

  WHY THIS EXISTS, and how it differs from check-decode-speed.ps1. That script is a GATE: it
  answers yes/no ("did anything get slower than it is allowed to be") and stores only the one
  number it needs to answer that. This is the RECORD: it stores what everything actually costs,
  including Windows' number, so the question "how fast are we, really" has a written answer
  instead of being re-measured from scratch by whoever asks next.

  Two axes the gate does not have:

  * SIZE. The gate times one sample per format at whatever size that sample happens to be, so
    a 320 px PCX and a 6 MP JPEG sit in the same column and neither tells you how the format
    SCALES. This reads the size-tiered corpus (scripts\build-speed-corpus.ps1): the identical
    picture at small / medium / large, so cost-per-megapixel is visible and a decoder that is
    fine on thumbnails but quadratic on real photos cannot hide.
  * WINDOWS' OWN NUMBER, STORED. The gate measures WIC live and throws it away once it has a
    ratio. Keeping it means the file answers "is this slow because WE are slow, or because the
    FORMAT is expensive" — a distinction that decides whether there is anything to fix.

  Formats Windows cannot open at all (most of ours — DjVu, PSD, the ebook and comic containers,
  the ImageMagick long tail) are recorded with a blank Windows column, never a zero. "No native
  peer" is a fact about the format, not a score.

  THE NUMBERS ARE MACHINE-SPECIFIC AND THE FILE SAYS SO. A record that gets compared across
  different hardware without anyone noticing is worse than no record, so the header stamps the
  CPU, the OS build, and which codec extensions were installed — the AV1 and HEIF extensions in
  particular decide whether several rows are measuring Windows at all.
#>
param(
    [string]$SpeedCorpus = "$PSScriptRoot\..\..\test-corpus-speed",
    [string]$RealCorpus  = "$PSScriptRoot\..\..\test-corpus-real",
    [string]$Out         = "$PSScriptRoot\..\docs\SPEED-BASELINE.md",
    [string[]]$Only,
    [int]$Runs = 5,
    [int]$Size = 256,
    # Looser than the gate's 0.20 on purpose. The gate runs ~3 minutes and fails a build, so it
    # must decline to judge at the first sign of movement. This runs ~15 minutes on a machine
    # someone is using, always sees some spread, and its ratio column is measured pairwise
    # within seconds so it survives that spread. Past this, though, the absolute milliseconds
    # stop being a record of the software.
    [double]$MaxDrift = 0.45,
    [switch]$NoWrite
)
$ErrorActionPreference = 'Stop'

$st2k = Join-Path (& "$PSScriptRoot\_targetdir.ps1") 'release\st2k.exe'
if (-not (Test-Path $st2k)) { throw "st2k.exe not built (cargo build --release --bin st2k)" }
Add-Type -AssemblyName PresentationCore
. "$PSScriptRoot\decode-speed-lib.ps1"

# ---------------------------------------------------------------- what to measure
# The tiered corpus is the point of this report; the real corpus fills in every format that
# cannot be synthesised (containers, ebooks, camera RAW), so the record covers everything
# measurable rather than only the easy half.
$rows = @()
$tierOrder = @{ small = 0; medium = 1; large = 2; single = 3 }

$tiered = @()
if (Test-Path $SpeedCorpus) {
    $tiered = Get-SpeedSamples $SpeedCorpus $null | Where-Object { $_.BaseName -match '-(small|medium|large)$' }
} else {
    Write-Host "[speed-report] no tiered corpus - run scripts\build-speed-corpus.ps1 first" -ForegroundColor Yellow
}
$real = @()
if (Test-Path $RealCorpus) { $real = Get-SpeedSamples $RealCorpus $null }

# A real-corpus format is only worth a row if the tiered corpus does not already cover it.
$tieredExts = @($tiered | ForEach-Object { $_.Extension.ToLowerInvariant().TrimStart('.') } | Sort-Object -Unique)
$realOnly = $real | Where-Object { $tieredExts -notcontains $_.Extension.ToLowerInvariant().TrimStart('.') }

$all = @($tiered) + @($realOnly)
if ($Only) {
    $want = $Only | ForEach-Object { $_.ToLowerInvariant().TrimStart('.') }
    $all = $all | Where-Object { $want -contains $_.Extension.ToLowerInvariant().TrimStart('.') }
}
if (-not $all) { throw "nothing to measure" }
Write-Host ("[speed-report] {0} samples ({1} tiered + {2} real-corpus-only), {3} runs each" -f
    $all.Count, @($tiered).Count, @($realOnly).Count, $Runs) -ForegroundColor Cyan

# ---------------------------------------------------------------- measure
# INTERLEAVED, IN SMALL CHUNKS, and that shape is the whole reason this report is trustworthy
# on a working machine. The first version timed every one of OUR decodes first and then every
# one of WINDOWS', which put the two halves of each comparison ~15 minutes apart; on a box
# whose load moved 42% across that window the ratio column was comparing two different
# machines. Now each chunk is measured ours-then-Windows within a couple of seconds, so
# whatever the load is doing, it is doing it to BOTH sides of the same row. The absolute
# milliseconds still wander with load; the RATIO does not, which is why the ratio is the
# column worth trusting and the header says so.
#
# The drift reference is re-timed once per chunk rather than only at the ends, so the header
# can report the real spread of conditions across the run instead of a single before/after
# guess that a long report will always fail.
$refIn  = ($all | Sort-Object Name | Select-Object -First 1).FullName
$refSub = ($all | Where-Object {
        $_.Extension.ToLowerInvariant().TrimStart('.') -in @('xpm', 'sun', 'miff', 'fits', 'cal', 'pcx', 'vicar', 'viff')
    } | Sort-Object Name | Select-Object -First 1).FullName
$refs = @($refIn) + @($refSub | Where-Object { $_ })

$chunkSize = 20
$refSamples = @()
$loadSamples = @()
$done = 0
$sorted = @($all | Sort-Object { $_.Extension }, Name)
for ($i = 0; $i -lt $sorted.Count; $i += $chunkSize) {
    $chunk = $sorted[$i..([Math]::Min($i + $chunkSize - 1, $sorted.Count - 1))]
    $refSamples += , (Measure-Drift $refs $Runs $Size)
    # One cheap reading per chunk, so the header can say what state the box was in.
    $loadSamples += (Get-CimInstance Win32_Processor |
        Measure-Object -Property LoadPercentage -Average).Average
    $mineChunk = Measure-Decode ($chunk.FullName) $Runs $Size
    foreach ($f in $chunk) {
        if (-not $mineChunk.ContainsKey($f.Name)) { continue }   # undecodable is regression.ps1's beat
        $wic = Measure-Wic $f.FullName $Runs                      # same chunk, seconds later
        $ext = $f.Extension.ToLowerInvariant().TrimStart('.')
        $tier = if ($f.BaseName -match '-(small|medium|large)$') { $Matches[1] } else { 'single' }
        $rows += [pscustomobject]@{
            ext   = $ext
            tier  = $tier
            bytes = $f.Length
            mine  = $mineChunk[$f.Name]
            wic   = $wic
            ratio = if ($null -ne $wic -and $wic -gt 0) { [Math]::Round($mineChunk[$f.Name] / $wic, 2) } else { $null }
        }
    }
    $done += $chunk.Count
    Write-Host ("  ...{0}/{1}" -f $done, $sorted.Count) -ForegroundColor DarkGray
}

# Conditions across the whole run: the spread of the in-process reference. Reported, not used
# to refuse, because a long report on a working machine will always see SOME spread and the
# ratio column survives it. Only an extreme spread means the run measured the load instead.
$refFirsts = @($refSamples | ForEach-Object { $_ | Select-Object -First 1 } | Where-Object { $_ })
$loadMean = if ($loadSamples.Count) {
    ($loadSamples | Measure-Object -Average).Average
} else {
    0
}
$drift = if ($refFirsts.Count -ge 2) {
    $lo = ($refFirsts | Measure-Object -Minimum).Minimum
    $hi = ($refFirsts | Measure-Object -Maximum).Maximum
    if ($hi -gt 0) { ($hi - $lo) / $hi } else { 0 }
} else { 0 }
Write-Host ("[speed-report] {0} rows measured, load spread {1:P0} across the run" -f $rows.Count, $drift) -ForegroundColor Cyan
if ($drift -gt $MaxDrift) {
    Write-Host ("[speed-report] REFUSED - the machine's speed varied {0:P0} across this run (limit {1:P0})." -f
        $drift, $MaxDrift) -ForegroundColor Red
    Write-Host  "               Absolute milliseconds would be a record of the load. Re-run when idle." -ForegroundColor Yellow
    exit 1
}

# ---------------------------------------------------------------- machine context
$cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name.Trim()
$os = (Get-CimInstance Win32_OperatingSystem).Caption + " (build " + [System.Environment]::OSVersion.Version.Build + ")"
$codecs = Get-AppxPackage | Where-Object { $_.Name -match 'AV1|HEIF|HEVC|WebP|Raw' } |
    ForEach-Object { "$($_.Name.Replace('Microsoft.','')) $($_.Version)" } | Sort-Object
$ver = (& $st2k --version) -replace '^st2k\s*', ''

# ---------------------------------------------------------------- render
$withPeer = @($rows | Where-Object { $null -ne $_.ratio })
$noPeer   = @($rows | Where-Object { $null -eq $_.ratio })
$faster   = @($withPeer | Where-Object { $_.ratio -le 1.0 })

function Fmt([double]$v) { if ($null -eq $v) { '' } else { '{0:N1}' -f $v } }

$md = @()
$md += "# Decode speed baseline"
$md += ""
$md += "How long SageThumbs 2K takes to turn each format into a 256 px thumbnail, and how long"
$md += "Windows' own codec takes on the same file. Generated by ``scripts\speed-report.ps1``."
$md += ""
$md += "**These numbers are specific to the machine below.** Comparing them against a run on"
$md += "different hardware, or against a machine without the same codec extensions installed,"
$md += "measures the hardware rather than the software."
$md += ""
$md += "| | |"
$md += "|---|---|"
$md += "| Version | $ver |"
$md += "| CPU | $cpu |"
$md += "| OS | $os |"
$md += "| Codec extensions | $(if ($codecs) { $codecs -join ', ' } else { 'none detected' }) |"
$md += "| Runs per sample | $Runs (fastest kept) |"
$md += "| Machine load spread during run | $('{0:P0}' -f $drift) |"
# LEVEL, not just spread. Drift answers "did the machine change mid-run", which a box that is
# BUSY THROUGHOUT answers with a reassuring 3% while every reading in the table is inflated.
# Anyone comparing this file against another run needs to know which state it was taken in, and
# nothing else in the header says so.
$md += "| Machine load during run (mean of samples) | $('{0:N0}%' -f $loadMean) |"
$md += ""
$md += "## How to read it"
$md += ""
$md += "* **Ours** and **Windows** are milliseconds to decode and fit to a 256 px box. Both are"
$md += "  measured in-process and take the fastest of $Runs runs, so neither carries process"
$md += "  startup and background load is squeezed out as far as it can be."
$md += "* **Windows** is blank where WIC cannot open the format at all. That is most of what this"
$md += "  product is for, and it is a fact about the format, not a score."
$md += "* **x** is ours ÷ Windows, and it is **the column to trust**: the two sides of each row are"
$md += "  measured seconds apart, so background load hits both equally and cancels. The absolute"
$md += "  milliseconds are honest but wander with whatever else the machine is doing."
$md += "* Ratios over a few milliseconds of absolute difference are not worth reading — a 7x on a"
$md += "  3 ms decode is 3 ms."
$md += "* Sizes: **small** 320×240 (0.08 MP), **medium** 1600×1200 (1.9 MP), **large** 4000×3000"
$md += "  (12 MP), all the same source picture. **single** means the format cannot be synthesised,"
$md += "  so it is the one real corpus sample at whatever size it is."
$md += ""
$md += "## Summary"
$md += ""
$md += "* $($rows.Count) samples measured across $((@($rows | Select-Object -ExpandProperty ext -Unique)).Count) formats."
$md += "* $($withPeer.Count) have a Windows equivalent; **$($faster.Count) of those are at least as fast as Windows**."
$md += "* $($noPeer.Count) have no Windows equivalent at all — for those, this is the only record that exists."
if ($withPeer) {
    $med = ($withPeer | Sort-Object ratio)[[int]($withPeer.Count / 2)].ratio
    $md += "* Median ratio where a comparison exists: **${med}x**."
}
$md += ""
$md += "## Every format"
$md += ""
$md += "| Format | Size | File | Ours (ms) | Windows (ms) | x |"
$md += "|---|---|---:|---:|---:|---:|"
foreach ($r in ($rows | Sort-Object ext, @{ e = { $tierOrder[$_.tier] } })) {
    $kb = if ($r.bytes -ge 1MB) { '{0:N1} MB' -f ($r.bytes / 1MB) } else { '{0:N0} KB' -f ($r.bytes / 1KB) }
    $md += "| $($r.ext) | $($r.tier) | $kb | $(Fmt $r.mine) | $(Fmt $r.wic) | $(if ($null -ne $r.ratio) { "$($r.ratio)x" } else { '' }) |"
}
$md += ""

if ($NoWrite) {
    Write-Host "[speed-report] -NoWrite: nothing written" -ForegroundColor Yellow
} else {
    New-Item -ItemType Directory -Force (Split-Path $Out -Parent) | Out-Null
    Set-Content -LiteralPath $Out -Value $md -Encoding UTF8
    Write-Host ("[speed-report] written: {0}" -f (Resolve-Path $Out)) -ForegroundColor Green
}

# Console summary so a run is useful without opening the file.
Write-Host ""
Write-Host "  slowest vs Windows (where a comparison exists):" -ForegroundColor Cyan
# Write-Host for the rows too: mixing Write-Host headers with output-stream rows makes the
# host print every header first and every row afterwards, which reads as though the numbers
# belong to the wrong list.
$withPeer | Sort-Object ratio -Descending | Select-Object -First 8 | ForEach-Object {
    Write-Host ("    {0,-8} {1,-7} {2,8:N1} ms ours · {3,8:N1} ms Windows · {4}x" -f $_.ext, $_.tier, $_.mine, $_.wic, $_.ratio)
}
Write-Host "  slowest overall:" -ForegroundColor Cyan
$rows | Sort-Object mine -Descending | Select-Object -First 8 | ForEach-Object {
    Write-Host ("    {0,-8} {1,-7} {2,8:N1} ms" -f $_.ext, $_.tier, $_.mine)
}
