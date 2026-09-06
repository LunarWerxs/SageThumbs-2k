<#
  regression-lib.ps1 - shared "required content gate" primitives for regression.ps1.

  Dot-sourced by regression.ps1 and by scripts\test-regression-lib.ps1 (offline unit tests,
  no build/corpus needed - see that file).

  WHY THIS EXISTS (2026-09-05 audit, finding F37). A content gate that regression.ps1 shells
  out to (check-render-sanity.ps1) documents THREE exit codes, not two: 0 clean, 1 a real
  finding, 2 "could not run" (missing python/Pillow, nothing rendered yet). Before this file
  existed, regression.ps1 only ever checked `-eq 1`, so exit 2 fell through both the fail
  branch and any record of it - the run reached the final "OK - all N baseline extensions
  still render" line exactly as if the gate had passed, on a machine where it never actually
  checked anything. The known-colour gate has the same shape inline (it silently prints
  "SKIPPED" and moves on when python/Pillow are absent).

  These functions turn "ran a checker, got an exit code" into one of three outcomes -
  pass / fail / inconclusive - and turn a LIST of such outcomes into one verdict for the
  whole run, so a required gate that never ran can no longer look identical to one that
  passed.
#>

# Classifies an already-known exit code into the gate's outcome. 0 is a clean pass, 1 is a
# real finding (regression.ps1 fails the run), and anything else - 2 is what
# check-render-sanity.ps1 documents for "could not run", but this is deliberately not an
# allowlist of exactly {0,1,2} - is INCONCLUSIVE: the gate did not actually verify anything
# this run, which must never look like a pass.
function Get-GateVerdict {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][int]$ExitCode
    )
    $status = switch ($ExitCode) {
        0 { 'pass' }
        1 { 'fail' }
        default { 'inconclusive' }
    }
    return [pscustomobject]@{ Name = $Name; ExitCode = $ExitCode; Status = $status }
}

# For a required gate that could not even be attempted (a missing interpreter/library
# checked inline, rather than a child script that ran and returned a code) - same
# INCONCLUSIVE status, with a reason instead of an exit code.
function New-InconclusiveGate {
    param(
        [Parameter(Mandatory)][string]$Name,
        [string]$Reason
    )
    return [pscustomobject]@{ Name = $Name; ExitCode = $null; Status = 'inconclusive'; Reason = $Reason }
}

# Runs a required content-gate script (a separate pwsh process, exactly how regression.ps1
# already invoked check-render-sanity.ps1) and classifies the result. Kept as its own
# function - rather than inlined at each call site - so a test can point $ScriptPath at a
# stub that just does `exit <n>` and exercise the real classification path without a build.
function Invoke-RequiredGate {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string]$ScriptPath,
        [string[]]$ScriptArgs = @()
    )
    & pwsh -NoProfile -File $ScriptPath @ScriptArgs
    return Get-GateVerdict -Name $Name -ExitCode $LASTEXITCODE
}

# Aggregates every required gate's verdict into the run's overall qualification. A gate that
# never ran must not let the run reach a fully-qualified pass by silence: by default an
# inconclusive gate fails the run (ShouldFail = $true) just as a real finding does.
# -AllowInconclusiveGates is the explicit opt-in for a deliberately reduced-scope run - it
# keeps the run from failing, but Qualified stays $false and the summary names every gate
# that did not execute, so a reduced run can never read as a complete one.
function Get-GateQualification {
    param(
        [Parameter(Mandatory)][AllowEmptyCollection()][object[]]$Verdicts,
        [switch]$AllowInconclusiveGates
    )
    $failed = @($Verdicts | Where-Object { $_.Status -eq 'fail' })
    $inconclusive = @($Verdicts | Where-Object { $_.Status -eq 'inconclusive' })

    if ($failed.Count) {
        $names = ($failed | ForEach-Object Name) -join ', '
        return [pscustomobject]@{
            Qualified  = $false
            ShouldFail = $true
            Summary    = "required gate(s) reported a real finding: $names"
        }
    }
    if ($inconclusive.Count) {
        $names = ($inconclusive | ForEach-Object Name) -join ', '
        if ($AllowInconclusiveGates) {
            return [pscustomobject]@{
                Qualified  = $false
                ShouldFail = $false
                Summary    = "NOT QUALIFIED - required gate(s) did not run, allowed via -AllowInconclusiveGates: $names"
            }
        }
        return [pscustomobject]@{
            Qualified  = $false
            ShouldFail = $true
            Summary    = "required gate(s) could not run and -AllowInconclusiveGates was not passed: $names"
        }
    }
    return [pscustomobject]@{
        Qualified  = $true
        ShouldFail = $false
        Summary    = 'every required gate ran and passed'
    }
}

# Renders the final baseline-coverage line so it never claims more than this corpus actually
# exercised. A baseline extension whose sample is absent from the CURRENT corpus was never
# rendered this run and must not be folded into "still render" - that is what let a 226-file
# corpus conclude "all 308 baseline extensions still render" (finding F37 repro).
function Format-BaselineCoverageLine {
    param(
        [Parameter(Mandatory)][int]$BaselineCount,
        [Parameter(Mandatory)][int]$PresentCount,
        [Parameter(Mandatory)][int]$MissingCount
    )
    if ($MissingCount -le 0) {
        return "all $BaselineCount baseline extensions still render."
    }
    return "$PresentCount/$BaselineCount baseline extensions present in this corpus render ($MissingCount baseline extension(s) have no sample in this corpus and were not exercised this run)."
}
