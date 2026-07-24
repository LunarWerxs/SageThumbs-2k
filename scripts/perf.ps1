<#
  perf.ps1 — time every real-content corpus sample through st2k and flag slow outliers.

  The "trip every slow filter at once" guard: run it after any decode change to catch a
  format that suddenly got slow — e.g. the legacy-Office WMF render that quietly cost ~5 s
  for a release before anyone noticed. It prints the slowest files (so you always see the
  tail) AND flags anything over -Threshold for a closer look (embedded preview missing?
  wrong decode tier? a render that hangs?).

      pwsh scripts\perf.ps1                 # default: real corpus, 3000 ms threshold
      pwsh scripts\perf.ps1 -Threshold 1500 # stricter
      pwsh scripts\perf.ps1 -Corpus <dir>   # a different sample set

  NOTE: uses ..\..\test-corpus-real (the ~795 MB real-content set) — that's the one whose
  decode cost mirrors what Explorer actually pays. The synthetic regression corpus has tiny
  samples and is for CORRECTNESS (scripts\regression.ps1), not speed.
#>
param(
    [int]$Threshold = 3000,
    [string]$Corpus = "$PSScriptRoot\..\..\test-corpus-real",
    [int]$Size = 256
)
$ErrorActionPreference = 'Stop'

$st2k = Join-Path (& "$PSScriptRoot\_targetdir.ps1") 'release\st2k.exe'
if (-not (Test-Path $st2k)) { throw "st2k.exe not built (cargo build --release --bin st2k)" }
if (-not (Test-Path $Corpus)) { throw "corpus not found: $Corpus" }

$tmp = Join-Path $env:TEMP ("st2k-perf-{0}-{1}" -f $PID, [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force $tmp | Out-Null
$exitCode = 0
$i = 0
try {
    $rows = foreach ($f in Get-ChildItem $Corpus -File | Where-Object { $_.Extension -notin '.md', '.txt' }) {
        # Keep every probe isolated. The old fixed `<basename>.png` path survived between runs
        # (and collided across sample.jpg/sample.png), so a failed decode could be reported as
        # successful merely because yesterday's output still existed.
        $out = Join-Path $tmp ("{0:D4}-{1}.png" -f ($i++), $f.Name)
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        & $st2k thumbnail $f.FullName $out --size $Size 2>$null | Out-Null
        $code = $LASTEXITCODE
        $sw.Stop()
        $output = Get-Item -LiteralPath $out -ErrorAction SilentlyContinue
        [pscustomobject]@{
            ms = [int]$sw.ElapsedMilliseconds
            name = $f.Name
            code = $code
            ok = ($code -eq 0 -and $null -ne $output -and $output.Length -gt 0)
        }
    }
    $rows = @($rows | Sort-Object ms -Descending)

    if ($rows.Count -eq 0) {
        throw "corpus contains no timed files: $Corpus"
    }

    $ordered = @($rows.ms | Sort-Object)
    function Percentile([double]$p) {
        # Nearest-rank percentile (p95 = the observation at ceil(0.95*n)).
        $idx = [math]::Max(0, [math]::Ceiling($p * $ordered.Count) - 1)
        return $ordered[$idx]
    }
    $failed = @($rows | Where-Object { -not $_.ok })
    Write-Host (
        "[perf] {0} files: median {1} ms · p90 {2} ms · p95 {3} ms · p99 {4} ms · {5} failed" -f
        $rows.Count, (Percentile 0.50), (Percentile 0.90), (Percentile 0.95),
        (Percentile 0.99), $failed.Count
    ) -ForegroundColor Cyan
    Write-Host "[perf] slowest 15:" -ForegroundColor Cyan
    $rows | Select-Object -First 15 | ForEach-Object {
        "  {0,7} ms  {1}{2}" -f $_.ms, $_.name,
            $(if (-not $_.ok) { " (FAILED, exit $($_.code))" } else { '' })
    }

    $slow = @($rows | Where-Object { $_.ms -gt $Threshold })
    if ($slow) {
        Write-Host ("[perf] {0} OVER {1} ms — investigate (missing embedded preview? wrong tier? render hang?):" -f $slow.Count, $Threshold) -ForegroundColor Yellow
        $slow | ForEach-Object { "  {0,7} ms  {1}" -f $_.ms, $_.name }
        $exitCode = 1
    } else {
        Write-Host ("[perf] OK - nothing over {0} ms." -f $Threshold) -ForegroundColor Green
    }
}
finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
exit $exitCode
