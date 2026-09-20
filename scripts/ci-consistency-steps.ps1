<#
  ci-consistency-steps.ps1 - the ONE list of CI's consistency-job scripts, DERIVED from
  .github/workflows/ci.yml rather than copied out of it.

  WHY THIS EXISTS. `preflight.ps1` and `scripts/refactor/gate.py` each carried their own
  hand-typed copy of this list, and both copies were written when CI's consistency job had
  eleven steps. The job grew to twenty-two; the copies did not. On 2026-09-20 that gap put four
  commits of red CI on a PUBLIC repo's front page - `check-registration-symmetry.ps1` went stale
  the moment a refactor moved a registry write one call deeper, and NOTHING on this desk ran it
  before the push, because it was one of the eleven the local lists had never heard of.

  A hand-copied list of gates is a gate that silently shrinks. So there is no list here either:
  this script READS the workflow and prints what CI actually runs, in CI's order, one step per
  line, as `<script.ps1> [args...]`. Add a step to ci.yml and the preflight runs it the same day.

  Usage:
      pwsh -File scripts\ci-consistency-steps.ps1            # one step per line
      pwsh -File scripts\ci-consistency-steps.ps1 -Run       # run them all, report, exit non-zero on any failure

  Exit 0 on success, 1 on a failing step (-Run), 2 if the workflow cannot be parsed.
#>
[CmdletBinding()]
param(
    # Run every derived step instead of just listing it.
    [switch] $Run,
    # Steps to leave out of a -Run sweep (still listed). Each needs a reason in the caller.
    [string[]] $Skip = @()
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$workflow = Join-Path $root '.github/workflows/ci.yml'

if (-not (Test-Path $workflow)) {
    Write-Host "[ci-steps] cannot find $workflow" -ForegroundColor Red
    exit 2
}

# The job's own block: from `  consistency:` to the next job at the same indent. Parsed as text
# on purpose - this has to work on a bare runner with no YAML module installed, the same
# constraint that made scripts/complexity-scan.py stdlib-only.
$lines = Get-Content $workflow
$start = -1
for ($i = 0; $i -lt $lines.Count; $i++) {
    if ($lines[$i] -match '^  consistency:\s*$') { $start = $i; break }
}
if ($start -lt 0) {
    Write-Host '[ci-steps] no `consistency:` job in ci.yml - has the workflow been restructured?' -ForegroundColor Red
    exit 2
}
$end = $lines.Count
for ($i = $start + 1; $i -lt $lines.Count; $i++) {
    if ($lines[$i] -match '^  [A-Za-z_][\w-]*:\s*$') { $end = $i; break }
}

$steps = @()
for ($i = $start; $i -lt $end; $i++) {
    if ($lines[$i] -match '^\s*run:\s*\./scripts/(\S+)(.*)$') {
        $steps += ($Matches[1] + $Matches[2]).TrimEnd()
    }
}

if ($steps.Count -lt 10) {
    # Failing closed is the whole point: a parser that quietly matched nothing would hand every
    # caller an empty gate, which is exactly the shape of the bug this file replaces.
    Write-Host "[ci-steps] parsed only $($steps.Count) step(s) from the consistency job - refusing to report that as the gate." -ForegroundColor Red
    exit 2
}

if (-not $Run) {
    $steps | ForEach-Object { Write-Output $_ }
    exit 0
}

$failed = @()
foreach ($step in $steps) {
    $parts = $step -split '\s+'
    $name = $parts[0]
    if ($Skip -contains $name) {
        Write-Host ("  SKIP  {0}" -f $step) -ForegroundColor DarkYellow
        continue
    }
    # `$parts[1..0]` COUNTS DOWN in PowerShell, so a step with no arguments would hand the
    # script its own name as a positional parameter (measured: `check-complexity.ps1` read it as
    # `-Root check-complexity.ps1` and failed "does not exist"). And `$args` is an automatic
    # variable - never assign to it.
    $stepArgs = if ($parts.Count -gt 1) { @($parts[1..($parts.Count - 1)] | Where-Object { $_ }) } else { @() }
    $path = Join-Path $PSScriptRoot $name
    if (-not (Test-Path $path)) {
        Write-Host ("  FAIL  {0} - no such script" -f $step) -ForegroundColor Red
        $failed += $step
        continue
    }
    # ⚠ `-Command`, NEVER `-File`. GitHub's `shell: pwsh` appends `exit $LASTEXITCODE` to the
    # step, so a script that ENDS on a failed native command without calling `exit` fails on CI
    # while `-File` would report the script's own clean exit. The hand-typed loop this file
    # replaced got that right (CLAUDE.md 6.1, "in-session, not -File"), and the first cut of
    # this file lost it - which would have been this gate claiming a fidelity it did not have.
    $argText = if ($stepArgs.Count) { ' ' + ($stepArgs -join ' ') } else { '' }
    $invoke = "& '$path'$argText; if (Test-Path variable:\LASTEXITCODE) { exit `$LASTEXITCODE }"
    $out = & pwsh -NoProfile -ExecutionPolicy Bypass -Command $invoke 2>&1
    if ($LASTEXITCODE -eq 0) {
        Write-Host ("  ok    {0}" -f $step) -ForegroundColor Green
    }
    else {
        Write-Host ("  FAIL  {0} (exit $LASTEXITCODE)" -f $step) -ForegroundColor Red
        $out | Select-Object -Last 25 | ForEach-Object { Write-Host "        $_" }
        $failed += $step
    }
}

if ($failed.Count) {
    Write-Host "[ci-steps] $($failed.Count)/$($steps.Count) FAILED: $($failed -join ', ')" -ForegroundColor Red
    exit 1
}
Write-Host "[ci-steps] all $($steps.Count) consistency step(s) clean" -ForegroundColor Green
exit 0
