<#
  check-complexity.ps1 - the TRACKED half of the complexity gate (2026-09-05 audit, finding F22;
  CI wiring + vendored fallback added since - see the "TWO ENGINES" section below).

  Before this script existed, the per-function cognitive/cyclomatic gate lived ONLY inside the
  local, untracked `.git/hooks/pre-push` (it shells out to Odin's `probe.py` directly, with the
  pass/fail counting logic inline in bash). `.arkitect/arkitect.config.json` does not declare
  those two checks either - see its own `$note`, which points `native-toolchain-gates` at this
  repo's OWN declared gates (fmt/deny/clippy), not at Odin's complexity scan. So a clean clone,
  or a release cut on a machine that never recreated the hook, enforced NOTHING here: the
  backlog regrew from 0 to 19 findings in the week after the 2026-08-28 burndown with every
  visible gate green (CLAUDE.md 2.2). Pulling the logic into a tracked script did not, by
  itself, make a fresh clone run it - see the "still machine-local" note below for what does.

  USAGE
    pwsh scripts\check-complexity.ps1                     # measure the working tree
    pwsh scripts\check-complexity.ps1 -Root <path>         # measure a worktree/scratch copy
    pwsh scripts\check-complexity.ps1 -ProveItFails        # prove every outcome for real

  TWO ENGINES, ONE SCRIPT (2026-09-08). `probe.py` lives in the PRIVATE sibling repo
  `Lunarwerx/odin`, so it can never be vendored here, and a GitHub runner can never clone a
  private repo to reach it - which is exactly why the gate used to be enforceable ONLY on this
  desk. This script now tries BOTH, in order:

      1. ODIN'S probe.py, if resolvable (see LOCATING ODIN below) - the REFERENCE
         implementation. Preferred whenever it is present, because it is what
         `docs/DEVELOPMENT_GOTCHAS.md`'s complexity rules are measured against.
      2. THE VENDORED FALLBACK, `scripts\complexity-scan.py` (stdlib-only Python, tracked,
         self-contained) - a calibrated port of odin's own Rust-complexity rules (see that
         script's own header for exactly which rules and why). This is what runs whenever odin
         is not on the machine, which on a GitHub runner is EVERY time - so CI gets the same
         gate this hook has always enforced locally, automatically, via this same script.

  Only if NEITHER resolves does this count as "nothing was measured" (exit 2, never a pass).
  Which engine ran is always printed, so a report never has to be taken on faith.

  LOCATING ODIN. Resolved in this order:
      1. $env:ODIN_ROOT\probe.py, IF the variable is set - and if set, it is AUTHORITATIVE:
         an explicit override that turns out wrong is reported as missing (falling through to
         the vendored engine), never silently re-routed to a machine default the caller did
         not ask for.
      2. D:\NEWProjects\shared\odin\probe.py           - this machine's normal clone.
      3. <repo root>\..\..\shared\odin\probe.py        - a portable sibling-of-the-repo clone,
         for a machine laid out as <parent>\shared\odin beside <parent>\<repo>\<repo>.

  -Gate/-Warn document, they do not change, the thresholds BOTH engines enforce (odin's own
  --help: "the gate is 30 and the warn line is 15"; `complexity-scan.py`'s GATE/WARN constants
  are pinned to the same values). Passing something else here only changes how THIS script
  buckets the scores an engine returns; it does not change what either engine itself measures.

  CALIBRATION. The vendored engine is a line-by-line port of odin's Rust-only legacy scan path,
  verified against a REAL odin run on this repo (2026-09-08): identical error set (both find
  zero functions at or over the gate), zero score mismatches on every function both engines
  found. See `scripts\complexity-scan.py`'s header for the full rule-by-rule mapping and its
  own `--self-test` for a standing proof against known fixtures.

  EXIT CODES
    0  clean - no finding at or over the gate
    1  at least one finding at or over the gate (printed, worst first)
    2  cannot measure - NEITHER engine resolved (no odin checkout AND the vendored script is
       missing/unreadable, or no python interpreter at all). NEVER treated as a pass.
    3  cannot measure - an engine ran but exited non-zero, or its output did not parse as the
       expected JSON array. NEVER treated as a pass.
#>
[CmdletBinding()]
param(
    [string]$Root = (Split-Path $PSScriptRoot -Parent),
    # The values both engines enforce (see the header). Not independently tunable in either
    # engine itself - overriding these here only changes how THIS script classifies the scores
    # it gets back, which is useful for -ProveItFails and for triage, not for loosening the
    # real gate.
    [int]$Gate = 30,
    [int]$Warn = 15,
    [string]$OdinKey = 'sagethumbs-2k',
    # The vendored fallback's location - overridable so -ProveItFails can point it at a
    # nonexistent path to prove the "neither engine resolves" outcome for real, without
    # deleting the tracked script.
    [string]$VendoredScript = (Join-Path $PSScriptRoot 'complexity-scan.py'),
    # THE WARN-BAND RATCHET. The band (15..29) was printed and never enforced, which is exactly
    # how it regrew: `complexity-scan.py`'s own header records the backlog going 0 -> 19 in a
    # week with every visible gate green. This file holds the highest band size each ENGINE is
    # allowed to report, so the count can only fall. Keyed by engine because the two engines do
    # not have to agree on a score, and a ceiling seeded from one would be a false red on the
    # other; an engine with no entry is reported as NOT ratcheted rather than silently passing.
    [string]$WarnCeilingFile = (Join-Path $PSScriptRoot 'complexity-warn-ceiling.json'),
    # Re-seed this engine's ceiling to what the tree measures right now. Only ever run this
    # after the count has gone DOWN - re-seeding upward is how a ratchet becomes a rubber stamp.
    [switch]$WriteWarnCeiling,
    # Self-test: prove every exit code for real, against real subprocesses (both engines where
    # available) and known fixtures - not by asserting the code path, by observing the exit
    # code a fresh subprocess actually returns. Same convention as check-render-sanity.ps1's
    # -ProveItFails: a gate nobody has ever seen fail is indistinguishable from one that cannot.
    [switch]$ProveItFails
)

$ErrorActionPreference = 'Stop'

function Resolve-OdinProbe {
    param([Parameter(Mandatory)][string]$RepoRoot)
    if ($env:ODIN_ROOT) {
        # Explicit override: authoritative, no fallthrough to a machine default. See the header
        # comment for why - it still falls through to the VENDORED engine below, just never to
        # a different odin location the caller did not ask for.
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

function Resolve-VendoredScanner {
    param([Parameter(Mandatory)][string]$ScriptPath)
    if (Test-Path -LiteralPath $ScriptPath -PathType Leaf) { return (Resolve-Path -LiteralPath $ScriptPath).Path }
    return $null
}

# Runs whichever engine script it is handed (odin's probe.py with its Key argument, or the
# vendored fallback without one) and returns a classified result; never throws for a
# measurement failure (only for genuinely unexpected PowerShell errors) so the caller can turn
# each case into the exit code documented above. Shared between both engines because their
# JSON contract is identical BY DESIGN - see complexity-scan.py's header.
function Invoke-ComplexityEngine {
    param(
        [Parameter(Mandatory)][string]$ScriptPath,
        [Parameter(Mandatory)][string[]]$EngineArgs
    )
    $python = Get-Command python -ErrorAction SilentlyContinue
    if (-not $python) {
        return [pscustomobject]@{ Status = 'MissingTool'; Detail = 'no python interpreter found on PATH'; Findings = $null }
    }
    $raw = & $python.Source $ScriptPath @EngineArgs 2>&1
    $code = $LASTEXITCODE
    if ($code -ne 0) {
        return [pscustomobject]@{ Status = 'NoVerdict'; Detail = "$(Split-Path -Leaf $ScriptPath) exited $code`: $($raw -join ' | ')"; Findings = $null }
    }
    try {
        $parsed = ($raw -join "`n") | ConvertFrom-Json
    } catch {
        return [pscustomobject]@{ Status = 'NoVerdict'; Detail = "$(Split-Path -Leaf $ScriptPath) output did not parse as JSON: $($_.Exception.Message)"; Findings = $null }
    }
    if ($null -eq $parsed) { $parsed = @() }
    return [pscustomobject]@{ Status = 'Ok'; Detail = $null; Findings = @($parsed) }
}

# The real check, factored out so -ProveItFails can drive it via a fresh subprocess (proving the
# actual CLI contract, not just an internal function call) exactly like the plain invocation
# below does.
# The warn-band ratchet. Returns 0 when the band is at or under this engine's ceiling (or when
# the engine has no ceiling to compare against, which is REPORTED, never silently passed), and 1
# when the band has grown. A band that has SHRUNK is a pass that names the command to re-seed -
# the number is not lowered automatically, because a gate that edits its own floor cannot be
# read later as evidence of anything.
function Test-WarnBandRatchet {
    param(
        [Parameter(Mandatory)][string]$CeilingFile,
        [Parameter(Mandatory)][AllowEmptyString()][string]$EngineKey,
        [Parameter(Mandatory)][string]$EngineLabel,
        [Parameter(Mandatory)][int]$Count,
        [Parameter(Mandatory)][string]$Band,
        [switch]$Write
    )
    $data = [ordered]@{}
    if (Test-Path -LiteralPath $CeilingFile) {
        try {
            $raw = Get-Content -LiteralPath $CeilingFile -Raw | ConvertFrom-Json
            foreach ($p in $raw.PSObject.Properties) { $data[$p.Name] = $p.Value }
        } catch {
            Write-Host "check-complexity: $CeilingFile is not readable JSON ($($_.Exception.Message)) - the warn band is NOT ratcheted." -ForegroundColor Red
            return 3
        }
    }
    if ($Write) {
        if (-not $EngineKey) {
            Write-Host "check-complexity: -WriteWarnCeiling needs an engine that actually measured something." -ForegroundColor Red
            return 3
        }
        $data['$note'] = 'The highest warn-band (see --help) count each complexity engine may report. Ratchet DOWN only: re-seeding upward turns the gate into a rubber stamp. Re-seed with: pwsh scripts\check-complexity.ps1 -WriteWarnCeiling'
        $data[$EngineKey] = $Count
        ([pscustomobject]$data | ConvertTo-Json -Depth 4) | Set-Content -LiteralPath $CeilingFile -Encoding utf8
        Write-Host "check-complexity: wrote the $EngineKey warn-band ceiling as $Count to $(Split-Path -Leaf $CeilingFile)." -ForegroundColor Cyan
        return 0
    }
    if (-not $EngineKey -or -not $data.Contains($EngineKey)) {
        Write-Host "check-complexity: no warn-band ceiling recorded for '$EngineKey' - the band ($Band) is measured at $Count but NOT ratcheted. Seed it with: pwsh scripts\check-complexity.ps1 -WriteWarnCeiling" -ForegroundColor Yellow
        return 0
    }
    $ceiling = [int]$data[$EngineKey]
    if ($Count -gt $ceiling) {
        Write-Host "check-complexity: the warn band GREW - $Count finding(s) in $Band via $EngineLabel, against a ceiling of $ceiling ($($Count - $ceiling) more)." -ForegroundColor Red
        Write-Host "  Nothing here blocks a release on its own, and that is exactly why it regrew from 0 to 19 in a week before this ratchet existed." -ForegroundColor Yellow
        Write-Host "  Split the function you just added, or extract the branch you just nested. The ceiling only ever moves DOWN." -ForegroundColor Yellow
        return 1
    }
    if ($Count -lt $ceiling) {
        Write-Host "check-complexity: the warn band SHRANK - $Count in $Band against a ceiling of $ceiling. Lower it: pwsh scripts\check-complexity.ps1 -WriteWarnCeiling" -ForegroundColor Cyan
    }
    return 0
}

function Invoke-CheckComplexity {
    param(
        [Parameter(Mandatory)][string]$MeasureRoot,
        [Parameter(Mandatory)][int]$GateValue,
        [Parameter(Mandatory)][int]$WarnValue,
        [Parameter(Mandatory)][string]$Key,
        [Parameter(Mandatory)][string]$VendoredScriptPath,
        [Parameter(Mandatory)][string]$CeilingFile,
        [switch]$WriteCeiling
    )
    $EngineKey = ''
    if (-not (Test-Path -LiteralPath $MeasureRoot -PathType Container)) {
        Write-Host "check-complexity: -Root does not exist: $MeasureRoot" -ForegroundColor Red
        return 3
    }

    $probePath = Resolve-OdinProbe -RepoRoot $MeasureRoot
    if ($probePath) {
        Write-Host "check-complexity: measuring $MeasureRoot (key '$Key') via ODIN's probe.py (reference) - $probePath" -ForegroundColor DarkGray
        $result = Invoke-ComplexityEngine -ScriptPath $probePath -EngineArgs @($Key, '--json', '--warnings', '--root', $MeasureRoot)
        $engineLabel = 'odin probe.py'
        $engineKey = 'odin'
    } else {
        if ($env:ODIN_ROOT) {
            Write-Host "check-complexity: `$env:ODIN_ROOT is set to '$env:ODIN_ROOT' and has no probe.py there (authoritative - no machine-default tried)." -ForegroundColor Yellow
        }
        $vendoredPath = Resolve-VendoredScanner -ScriptPath $VendoredScriptPath
        if (-not $vendoredPath) {
            Write-Host "check-complexity: odin's probe.py was not found, AND the vendored fallback ($VendoredScriptPath) was not found either - nothing was measured." -ForegroundColor Red
            Write-Host "  this is NOT a pass. Clone Lunarwerx/odin (or set `$env:ODIN_ROOT), or restore scripts\complexity-scan.py." -ForegroundColor Yellow
            return 2
        }
        Write-Host "check-complexity: odin's probe.py not found on this machine - measuring $MeasureRoot via the VENDORED fallback - $vendoredPath" -ForegroundColor DarkGray
        Write-Host "  (this is the engine every CI run uses - GitHub can never clone the private odin checkout)" -ForegroundColor DarkGray
        $result = Invoke-ComplexityEngine -ScriptPath $vendoredPath -EngineArgs @('--json', '--warnings', '--root', $MeasureRoot)
        $engineLabel = 'the vendored scripts\complexity-scan.py'
        $engineKey = 'vendored'
    }

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
    $ratchet = Test-WarnBandRatchet -CeilingFile $CeilingFile -EngineKey $EngineKey -EngineLabel $engineLabel -Count $warnBand.Count -Write:$WriteCeiling -Band "$WarnValue..$($GateValue - 1)"
    if ($overGate.Count -eq 0) {
        if ($ratchet -ne 0) { return $ratchet }
        Write-Host "check-complexity: clean via $engineLabel - 0 finding(s) at or over the gate ($GateValue); $($warnBand.Count) in the warn band ($WarnValue..$($GateValue - 1))." -ForegroundColor Green
        return 0
    }
    foreach ($f in $overGate) {
        Write-Host ("  FAIL  {0,3}  {1}:{2}  {3}  ({4})" -f $f.score, $f.file, $f.line, $f.function, $f.metric) -ForegroundColor Red
    }
    Write-Host "check-complexity: $($overGate.Count) finding(s) at or over the gate ($GateValue) via $engineLabel. Split them - a helper you extract must itself land under the gate too." -ForegroundColor Red
    return 1
}

if ($ProveItFails) {
    Write-Host "check-complexity -ProveItFails: proving every outcome against real subprocesses..." -ForegroundColor Cyan
    $selfFailures = 0
    $work = Join-Path ([IO.Path]::GetTempPath()) ("st2k-complexity-selftest-" + $PID)
    $emptyOdin = Join-Path $work 'empty-odin'
    $missingVendored = Join-Path $work 'no-such-scanner.py'
    $passFixture = Join-Path $work 'pass\src'
    $failFixture = Join-Path $work 'fail\src'
    $bandFixture = Join-Path $work 'band\src'
    New-Item -ItemType Directory -Force -Path $emptyOdin, $passFixture, $failFixture, $bandFixture | Out-Null
    try {
        # A trivial function: no branches at all, cognitive/cyclomatic both effectively 1.
        Set-Content -LiteralPath (Join-Path $passFixture 'trivial.rs') -Encoding utf8 -Value @(
            'fn add(a: i32, b: i32) -> i32 {'
            '    a + b'
            '}'
        )
        # Deeply nested branching/looping/matching: comfortably over the gate on both metrics.
        # (Verified 2026-09-05 against real odin: cognitive 174, cyclomatic 30. The vendored
        # engine's own `--self-test` re-proves this exact number against the identical fixture -
        # see scripts\complexity-scan.py.)
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

        function Invoke-SelfTestCase([string]$Name, [int]$Expect, [string]$MeasureRoot, [hashtable]$Env, [string[]]$ExtraArgs = @()) {
            $old = @{}
            foreach ($k in $Env.Keys) { $old[$k] = [System.Environment]::GetEnvironmentVariable($k) }
            try {
                foreach ($k in $Env.Keys) { [System.Environment]::SetEnvironmentVariable($k, $Env[$k]) }
                & pwsh -NoProfile -File $PSCommandPath -Root $MeasureRoot @ExtraArgs *> $null
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

        # The vendored engine's own standing proof against known fixtures (see its --self-test).
        $python = Get-Command python -ErrorAction SilentlyContinue
        if ($python) {
            & $python.Source $VendoredScript --self-test
            if ($LASTEXITCODE -eq 0) {
                Write-Host "  PASS  vendored scanner's own --self-test" -ForegroundColor Green
            } else {
                Write-Host "  FAIL  vendored scanner's own --self-test (exit $LASTEXITCODE)" -ForegroundColor Red
                $selfFailures++
            }
        } else {
            Write-Host "  SKIP  vendored scanner's own --self-test - no python on PATH" -ForegroundColor DarkYellow
        }

        # Both engines, where available: the vendored fallback is ALWAYS testable (tracked,
        # stdlib-only); odin is only testable on a machine that actually has the private
        # checkout - SKIP rather than false-fail when it does not, same declared-limitation
        # shape as this repo's other -ProveItFails scripts.
        if (Resolve-OdinProbe -RepoRoot (Split-Path $PSCommandPath -Parent | Split-Path -Parent)) {
            Invoke-SelfTestCase -Name 'pass (trivial function, real odin)' -Expect 0 `
                -MeasureRoot (Split-Path $passFixture -Parent) -Env @{}
            Invoke-SelfTestCase -Name 'fail (deeply nested function, real odin)' -Expect 1 `
                -MeasureRoot (Split-Path $failFixture -Parent) -Env @{}
        } else {
            Write-Host "  SKIP  odin-engine pass/fail cases - no odin checkout on this machine" -ForegroundColor DarkYellow
        }

        Invoke-SelfTestCase -Name 'pass (trivial function, vendored fallback forced)' -Expect 0 `
            -MeasureRoot (Split-Path $passFixture -Parent) -Env @{ ODIN_ROOT = $emptyOdin }
        Invoke-SelfTestCase -Name 'fail (deeply nested function, vendored fallback forced)' -Expect 1 `
            -MeasureRoot (Split-Path $failFixture -Parent) -Env @{ ODIN_ROOT = $emptyOdin }
        Invoke-SelfTestCase -Name 'neither engine resolves' -Expect 2 `
            -MeasureRoot (Split-Path $passFixture -Parent) -Env @{ ODIN_ROOT = $emptyOdin } -ExtraArgs @('-VendoredScript', $missingVendored)

        # THE WARN-BAND RATCHET, all four outcomes, against a fixture that sits IN the band
        # (a flat chain of branches: well past the warn line of 15, nowhere near the gate of
        # 30, so it can never be confused with the over-gate cases above). The ceilings are 0
        # and 999 rather than the fixture's exact score on purpose: this proves the comparison,
        # not the engine's arithmetic, which the vendored scanner's own --self-test already pins.
        $bandRs = @('fn flat_chain(v: i32) -> i32 {', '    let mut n = 0;')
        foreach ($i in 1..18) { $bandRs += "    if v == $i { n += $i; }" }
        $bandRs += @('    n', '}')
        Set-Content -LiteralPath (Join-Path $bandFixture 'band.rs') -Encoding utf8 -Value $bandRs
        $bandRoot = Split-Path $bandFixture -Parent
        $ceilGrew = Join-Path $work 'ceiling-zero.json'
        $ceilRoom = Join-Path $work 'ceiling-roomy.json'
        $ceilNone = Join-Path $work 'ceiling-empty.json'
        $ceilJunk = Join-Path $work 'ceiling-junk.json'
        Set-Content -LiteralPath $ceilGrew -Encoding utf8 -Value '{ "vendored": 0, "odin": 0 }'
        Set-Content -LiteralPath $ceilRoom -Encoding utf8 -Value '{ "vendored": 999, "odin": 999 }'
        Set-Content -LiteralPath $ceilNone -Encoding utf8 -Value '{}'
        Set-Content -LiteralPath $ceilJunk -Encoding utf8 -Value 'not json {{'
        Invoke-SelfTestCase -Name 'warn band over its ceiling' -Expect 1 `
            -MeasureRoot $bandRoot -Env @{ ODIN_ROOT = $emptyOdin } -ExtraArgs @('-WarnCeilingFile', $ceilGrew)
        Invoke-SelfTestCase -Name 'warn band under its ceiling' -Expect 0 `
            -MeasureRoot $bandRoot -Env @{ ODIN_ROOT = $emptyOdin } -ExtraArgs @('-WarnCeilingFile', $ceilRoom)
        Invoke-SelfTestCase -Name 'warn band with no ceiling for this engine (reported, not a fail)' -Expect 0 `
            -MeasureRoot $bandRoot -Env @{ ODIN_ROOT = $emptyOdin } -ExtraArgs @('-WarnCeilingFile', $ceilNone)
        Invoke-SelfTestCase -Name 'warn-band ceiling file unreadable' -Expect 3 `
            -MeasureRoot $bandRoot -Env @{ ODIN_ROOT = $emptyOdin } -ExtraArgs @('-WarnCeilingFile', $ceilJunk)
    } finally {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }
    if ($selfFailures -gt 0) {
        Write-Host "check-complexity -ProveItFails: FAILED - $selfFailures case(s) did not exit as expected." -ForegroundColor Red
        exit 1
    }
    Write-Host "check-complexity -ProveItFails: OK - pass/fail/missing all exit distinctly, on every engine this machine can reach." -ForegroundColor Green
    exit 0
}

exit (Invoke-CheckComplexity -MeasureRoot $Root -GateValue $Gate -WarnValue $Warn -Key $OdinKey -VendoredScriptPath $VendoredScript -CeilingFile $WarnCeilingFile -WriteCeiling:$WriteWarnCeiling)
