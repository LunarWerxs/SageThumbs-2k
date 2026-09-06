<#
  check-complexity.ps1 - the TRACKED half of the complexity gate (2026-09-05 audit, finding F22).

  Before this script existed, the per-function cognitive/cyclomatic gate lived ONLY inside the
  local, untracked `.git/hooks/pre-push` (it shells out to Odin's `probe.py` directly, with the
  pass/fail counting logic inline in bash). `.arkitect/arkitect.config.json` does not declare
  those two checks either - see its own `$note`, which points `native-toolchain-gates` at this
  repo's OWN declared gates (fmt/deny/clippy), not at Odin's complexity scan. So a clean clone,
  or a release cut on a machine that never recreated the hook, enforces NOTHING here: the
  backlog regrew from 0 to 19 findings in the week after the 2026-08-28 burndown with every
  visible gate green (CLAUDE.md 2.2). Pulling the logic into a tracked script does not, by
  itself, make a fresh clone run it - see the "still machine-local" note below for what does.

  USAGE
    pwsh scripts\check-complexity.ps1                     # measure the working tree
    pwsh scripts\check-complexity.ps1 -Root <path>         # measure a worktree/scratch copy
    pwsh scripts\check-complexity.ps1 -ProveItFails        # prove all three outcomes for real

  LOCATING ODIN. `probe.py` lives in the PRIVATE sibling repo `Lunarwerx/odin`, so there is no
  vendoring it here. Resolved in this order:
      1. $env:ODIN_ROOT\probe.py, IF the variable is set - and if set, it is AUTHORITATIVE:
         an explicit override that turns out wrong is reported as missing, never silently
         re-routed to a machine default the caller did not ask for.
      2. D:\NEWProjects\shared\odin\probe.py           - this machine's normal clone.
      3. <repo root>\..\..\shared\odin\probe.py        - a portable sibling-of-the-repo clone,
         for a machine laid out as <parent>\shared\odin beside <parent>\<repo>\<repo>.
  A missing probe is reported and gated on (exit 2) - it is NEVER treated as a pass. That is
  the whole point: a probe with no verdict means nothing was measured, not that the code is
  clean.

  -Gate/-Warn document, they do not change, the thresholds `probe.py` itself enforces (its own
  --help: "the gate is 30 and the warn line is 15", enforced identically by the pre-push hook
  this script replaces the inline logic of). Passing something else here only changes how THIS
  script buckets the scores probe.py returns; it does not change what probe.py itself measures.

  ⛔ THIS SCRIPT IS THE ENFORCEMENT POINT. CI CANNOT RUN IT. GitHub's runners can never check
  out `Lunarwerx/odin` (it is private), so no workflow here will ever call this script, and
  `.arkitect`'s own roster has nowhere to declare it either. The LOCAL `.git/hooks/pre-push` -
  itself untracked, recreated by hand after a fresh clone per CLAUDE.md 2.2 - is the only thing
  that can call it on every push. A clone that never recreates that hook (or a machine with no
  Odin checkout) pushes with NO complexity gate at all, and nothing here can detect that from
  inside the repo. See this finding's "remaining" note for exactly what stays machine-local.

  EXIT CODES
    0  clean - no finding at or over the gate
    1  at least one finding at or over the gate (printed, worst first)
    2  cannot measure - the resolved probe.py (or a python interpreter) was not found.
       NEVER treated as a pass.
    3  cannot measure - probe.py ran but exited non-zero, or its output did not parse as the
       expected JSON array. NEVER treated as a pass.
#>
[CmdletBinding()]
param(
    [string]$Root = (Split-Path $PSScriptRoot -Parent),
    # The values the pre-push hook enforces (see the header). Not independently tunable in
    # probe.py itself - overriding these here only changes how THIS script classifies the
    # scores it gets back, which is useful for -ProveItFails and for triage, not for loosening
    # the real gate.
    [int]$Gate = 30,
    [int]$Warn = 15,
    [string]$OdinKey = 'sagethumbs-2k',
    # Self-test: prove the three exit codes for real, against a real (missing) probe and two
    # real synthetic fixtures - not by asserting the code path, by observing the exit code a
    # fresh subprocess actually returns. Same convention as check-render-sanity.ps1's
    # -ProveItFails: a gate nobody has ever seen fail is indistinguishable from one that cannot.
    [switch]$ProveItFails
)

$ErrorActionPreference = 'Stop'

function Resolve-OdinProbe {
    param([Parameter(Mandatory)][string]$RepoRoot)
    if ($env:ODIN_ROOT) {
        # Explicit override: authoritative, no fallthrough. See the header comment for why.
        $viaEnv = Join-Path $env:ODIN_ROOT 'probe.py'
        if (Test-Path -LiteralPath $viaEnv -PathType Leaf) { return (Resolve-Path -LiteralPath $viaEnv).Path }
        return $null
    }
    foreach ($candidate in @(
            'D:\NEWProjects\shared\odin\probe.py',
            (Join-Path $RepoRoot '..\..\shared\odin\probe.py')
        )) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) { return (Resolve-Path -LiteralPath $candidate).Path }
    }
    return $null
}

# Runs the probe and returns a classified result; never throws for a measurement failure (only
# for genuinely unexpected PowerShell errors) so the caller can turn each case into the exit
# code documented above.
function Invoke-ComplexityProbe {
    param(
        [Parameter(Mandatory)][string]$ProbePath,
        [Parameter(Mandatory)][string]$MeasureRoot,
        [Parameter(Mandatory)][string]$Key
    )
    $python = Get-Command python -ErrorAction SilentlyContinue
    if (-not $python) {
        return [pscustomobject]@{ Status = 'MissingTool'; Detail = 'no python interpreter found on PATH'; Findings = $null }
    }
    $raw = & $python.Source $ProbePath $Key '--json' '--warnings' '--root' $MeasureRoot 2>&1
    $code = $LASTEXITCODE
    if ($code -ne 0) {
        return [pscustomobject]@{ Status = 'NoVerdict'; Detail = "probe.py exited $code`: $($raw -join ' | ')"; Findings = $null }
    }
    try {
        $parsed = ($raw -join "`n") | ConvertFrom-Json
    } catch {
        return [pscustomobject]@{ Status = 'NoVerdict'; Detail = "probe.py output did not parse as JSON: $($_.Exception.Message)"; Findings = $null }
    }
    if ($null -eq $parsed) { $parsed = @() }
    return [pscustomobject]@{ Status = 'Ok'; Detail = $null; Findings = @($parsed) }
}

# The real check, factored out so -ProveItFails can drive it via a fresh subprocess (proving the
# actual CLI contract, not just an internal function call) exactly like the plain invocation
# below does.
function Invoke-CheckComplexity {
    param(
        [Parameter(Mandatory)][string]$MeasureRoot,
        [Parameter(Mandatory)][int]$GateValue,
        [Parameter(Mandatory)][int]$WarnValue,
        [Parameter(Mandatory)][string]$Key
    )
    if (-not (Test-Path -LiteralPath $MeasureRoot -PathType Container)) {
        Write-Host "check-complexity: -Root does not exist: $MeasureRoot" -ForegroundColor Red
        return 3
    }
    $probePath = Resolve-OdinProbe -RepoRoot $MeasureRoot
    if (-not $probePath) {
        Write-Host "check-complexity: odin's probe.py was not found - nothing was measured." -ForegroundColor Red
        if ($env:ODIN_ROOT) {
            Write-Host "  `$env:ODIN_ROOT is set to '$env:ODIN_ROOT' and has no probe.py there (authoritative - no fallback tried)." -ForegroundColor Yellow
        } else {
            Write-Host "  looked at D:\NEWProjects\shared\odin\probe.py and <repo>\..\..\shared\odin\probe.py." -ForegroundColor Yellow
        }
        Write-Host "  this is NOT a pass. Clone Lunarwerx/odin, or set `$env:ODIN_ROOT to point at it." -ForegroundColor Yellow
        return 2
    }
    Write-Host "check-complexity: measuring $MeasureRoot (key '$Key') via $probePath" -ForegroundColor DarkGray
    $result = Invoke-ComplexityProbe -ProbePath $probePath -MeasureRoot $MeasureRoot -Key $Key
    if ($result.Status -eq 'MissingTool') {
        Write-Host "check-complexity: $($result.Detail) - this is NOT a pass." -ForegroundColor Red
        return 2
    }
    if ($result.Status -eq 'NoVerdict') {
        Write-Host "check-complexity: $($result.Detail)" -ForegroundColor Red
        Write-Host "  a probe with no verdict is NOT a pass." -ForegroundColor Yellow
        return 3
    }
    $overGate = @($result.Findings | Where-Object { $_.score -ge $GateValue })
    $warnBand = @($result.Findings | Where-Object { $_.score -ge $WarnValue -and $_.score -lt $GateValue })
    foreach ($w in $warnBand) {
        Write-Host ("  WARN  {0,3}  {1}:{2}  {3}  ({4})" -f $w.score, $w.file, $w.line, $w.function, $w.metric) -ForegroundColor DarkYellow
    }
    if ($overGate.Count -eq 0) {
        Write-Host "check-complexity: clean - 0 finding(s) at or over the gate ($GateValue); $($warnBand.Count) in the warn band ($WarnValue..$($GateValue - 1))." -ForegroundColor Green
        return 0
    }
    foreach ($f in $overGate) {
        Write-Host ("  FAIL  {0,3}  {1}:{2}  {3}  ({4})" -f $f.score, $f.file, $f.line, $f.function, $f.metric) -ForegroundColor Red
    }
    Write-Host "check-complexity: $($overGate.Count) finding(s) at or over the gate ($GateValue). Split them - a helper you extract must itself land under the gate too." -ForegroundColor Red
    return 1
}

if ($ProveItFails) {
    Write-Host "check-complexity -ProveItFails: proving all three outcomes against real subprocesses..." -ForegroundColor Cyan
    $selfFailures = 0
    $work = Join-Path ([IO.Path]::GetTempPath()) ("st2k-complexity-selftest-" + $PID)
    $emptyOdin = Join-Path $work 'empty-odin'
    $passFixture = Join-Path $work 'pass\src'
    $failFixture = Join-Path $work 'fail\src'
    New-Item -ItemType Directory -Force -Path $emptyOdin, $passFixture, $failFixture | Out-Null
    try {
        # A trivial function: no branches at all, cognitive/cyclomatic both effectively 1.
        Set-Content -LiteralPath (Join-Path $passFixture 'trivial.rs') -Encoding utf8 -Value @(
            'fn add(a: i32, b: i32) -> i32 {'
            '    a + b'
            '}'
        )
        # Deeply nested branching/looping/matching: comfortably over the gate on both metrics.
        # (Verified 2026-09-05 against this exact shape: cognitive 174, cyclomatic 30.)
        $deep = @(
            'fn deeply_nested(a: i32, b: i32, c: i32, d: i32, e: i32) -> i32 {'
            '    let mut total = 0;'
            '    if a > 0 {'
            '        if b > 0 {'
            '            if c > 0 {'
            '                if d > 0 {'
            '                    if e > 0 {'
            '                        for i in 0..a {'
            '                            if i % 2 == 0 {'
            '                                if i % 3 == 0 {'
            '                                    if i % 5 == 0 {'
            '                                        while total < b {'
            '                                            match i {'
            '                                                0 => total += 1,'
            '                                                1 => total += 2,'
            '                                                2 => total += 3,'
            '                                                3 => total += 4,'
            '                                                4 => total += 5,'
            '                                                5 => total += 6,'
            '                                                6 => total += 7,'
            '                                                7 => total += 8,'
            '                                                8 => total += 9,'
            '                                                9 => total += 10,'
            '                                                _ => {'
            '                                                    if total > 100 {'
            '                                                        if total > 200 {'
            '                                                            if total > 300 {'
            '                                                                if total > 400 {'
            '                                                                    break;'
            '                                                                } else { total += 1; }'
            '                                                            } else { total += 2; }'
            '                                                        } else { total += 3; }'
            '                                                    } else { total += 4; }'
            '                                                }'
            '                                            }'
            '                                        }'
            '                                    } else if e < -5 { total -= 1; } else { total -= 2; }'
            '                                } else if d < -5 { total -= 3; } else { total -= 4; }'
            '                            } else if c < -5 { total -= 5; } else { total -= 6; }'
            '                        }'
            '                    } else if b < -5 { total -= 7; } else { total -= 8; }'
            '                } else if a < -5 { total -= 9; } else { total -= 10; }'
            '            } else { total -= 11; }'
            '        } else { total -= 12; }'
            '    } else { total -= 13; }'
            '    total'
            '}'
        )
        Set-Content -LiteralPath (Join-Path $failFixture 'deep.rs') -Encoding utf8 -Value $deep

        function Invoke-SelfTestCase([string]$Name, [int]$Expect, [string]$MeasureRoot, [hashtable]$Env) {
            $old = @{}
            foreach ($k in $Env.Keys) { $old[$k] = [System.Environment]::GetEnvironmentVariable($k) }
            try {
                foreach ($k in $Env.Keys) { [System.Environment]::SetEnvironmentVariable($k, $Env[$k]) }
                & pwsh -NoProfile -File $PSCommandPath -Root $MeasureRoot *> $null
                $got = $LASTEXITCODE
            } finally {
                foreach ($k in $Env.Keys) { [System.Environment]::SetEnvironmentVariable($k, $old[$k]) }
            }
            if ($got -eq $Expect) {
                Write-Host "  PASS  $Name (exit $got)" -ForegroundColor Green
            } else {
                Write-Host "  FAIL  $Name - expected exit $Expect, got $got" -ForegroundColor Red
                $script:selfFailures++
            }
        }

        Invoke-SelfTestCase -Name 'tool missing (ODIN_ROOT points at an empty dir)' -Expect 2 `
            -MeasureRoot (Split-Path $passFixture -Parent) -Env @{ ODIN_ROOT = $emptyOdin }
        Invoke-SelfTestCase -Name 'pass (trivial function, real odin)' -Expect 0 `
            -MeasureRoot (Split-Path $passFixture -Parent) -Env @{}
        Invoke-SelfTestCase -Name 'fail (deeply nested function, real odin)' -Expect 1 `
            -MeasureRoot (Split-Path $failFixture -Parent) -Env @{}
    } finally {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }
    if ($selfFailures -gt 0) {
        Write-Host "check-complexity -ProveItFails: FAILED - $selfFailures case(s) did not exit as expected." -ForegroundColor Red
        exit 1
    }
    Write-Host "check-complexity -ProveItFails: OK - tool-missing, pass, and fail all exit distinctly." -ForegroundColor Green
    exit 0
}

exit (Invoke-CheckComplexity -MeasureRoot $Root -GateValue $Gate -WarnValue $Warn -Key $OdinKey)
