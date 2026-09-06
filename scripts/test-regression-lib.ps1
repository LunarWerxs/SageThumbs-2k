<#
  Offline tests for regression-lib.ps1's required-gate classification (finding F37, 2026-09-05
  audit): a content gate that returns exit 2 ("could not run" - missing prerequisites) must not
  read as a pass, and the baseline-coverage line must not claim more than a corpus actually
  exercised. No build, no st2k.exe, no corpus - pure logic plus one real stub-script subprocess.

  BEFORE THIS FIX: regression.ps1 only checked `-eq 1` after calling check-render-sanity.ps1, so
  an exit-2 "could not run" fell through to the final "OK - all N baseline extensions still
  render" line exactly like a real pass. The source-contract assertions at the bottom of this
  file fail against that pre-fix regression.ps1 (verified by hand - see the finding's
  verification notes): none of Invoke-RequiredGate / Get-GateQualification /
  AllowInconclusiveGates existed there, so the FAIL-closed check-render-sanity classification
  simply was not present.
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$script:passed = 0
. (Join-Path $PSScriptRoot 'test-assert-lib.ps1')
. (Join-Path $PSScriptRoot 'regression-lib.ps1')

# ---------------------------------------------------------------- Get-GateVerdict
Assert-Passes 'Get-GateVerdict: exit 0 is a pass' {
    $v = Get-GateVerdict -Name 'x' -ExitCode 0
    if ($v.Status -ne 'pass') { throw "expected pass, got $($v.Status)" }
}
Assert-Passes 'Get-GateVerdict: exit 1 is a fail' {
    $v = Get-GateVerdict -Name 'x' -ExitCode 1
    if ($v.Status -ne 'fail') { throw "expected fail, got $($v.Status)" }
}
Assert-Passes 'Get-GateVerdict: exit 2 is inconclusive, not a pass' {
    $v = Get-GateVerdict -Name 'x' -ExitCode 2
    if ($v.Status -ne 'inconclusive') { throw "expected inconclusive, got $($v.Status)" }
}
Assert-Passes 'Get-GateVerdict: any other exit code is inconclusive too, never silently a pass' {
    $v = Get-GateVerdict -Name 'x' -ExitCode 42
    if ($v.Status -ne 'inconclusive') { throw "expected inconclusive, got $($v.Status)" }
}

# ---------------------------------------------------------------- Invoke-RequiredGate (real subprocess, stub child script)
$scratch = Join-Path ([IO.Path]::GetTempPath()) ("st2k-regression-lib-tests-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $scratch -Force | Out-Null
try {
    $stub = Join-Path $scratch 'stub-gate.ps1'
    Set-Content -LiteralPath $stub -Value 'param([int]$ExitCode = 0) exit $ExitCode' -Encoding utf8NoBOM

    foreach ($case in @(
        @{ code = 0; want = 'pass' },
        @{ code = 1; want = 'fail' },
        @{ code = 2; want = 'inconclusive' }
    )) {
        Assert-Passes "Invoke-RequiredGate: a stub child returning $($case.code) is classified $($case.want)" {
            $v = Invoke-RequiredGate -Name 'stub' -ScriptPath $stub -ScriptArgs @('-ExitCode', $case.code)
            if ($v.Status -ne $case.want) { throw "expected $($case.want) for exit $($case.code), got $($v.Status)" }
            if ($v.ExitCode -ne $case.code) { throw "expected ExitCode $($case.code), got $($v.ExitCode)" }
        }
    }
} finally {
    if (Test-Path -LiteralPath $scratch) { Remove-Item -LiteralPath $scratch -Recurse -Force }
}

# ---------------------------------------------------------------- Get-GateQualification
Assert-Passes 'Get-GateQualification: all-pass verdicts fully qualify the run' {
    $q = Get-GateQualification -Verdicts @(
        (Get-GateVerdict -Name 'a' -ExitCode 0),
        (Get-GateVerdict -Name 'b' -ExitCode 0)
    )
    if (-not $q.Qualified -or $q.ShouldFail) { throw "expected a qualified, non-failing pass" }
}
Assert-Passes 'Get-GateQualification: a real finding fails the run and names the gate' {
    $q = Get-GateQualification -Verdicts @(
        (Get-GateVerdict -Name 'a' -ExitCode 0),
        (Get-GateVerdict -Name 'broken-gate' -ExitCode 1)
    )
    if ($q.Qualified -or -not $q.ShouldFail) { throw "expected a failing, unqualified run" }
    if ($q.Summary -notlike '*broken-gate*') { throw "summary did not name the failing gate: $($q.Summary)" }
}
Assert-Passes 'Get-GateQualification: DEFAULT - an inconclusive gate fails the run (finding F37)' {
    $q = Get-GateQualification -Verdicts @(
        (Get-GateVerdict -Name 'a' -ExitCode 0),
        (Get-GateVerdict -Name 'missing-tool-gate' -ExitCode 2)
    )
    if ($q.Qualified) { throw "an inconclusive gate must never read as qualified" }
    if (-not $q.ShouldFail) { throw "by default an inconclusive gate must fail the run, not silently pass" }
    if ($q.Summary -notlike '*missing-tool-gate*') { throw "summary did not name the gate that did not run: $($q.Summary)" }
}
Assert-Passes 'Get-GateQualification: -AllowInconclusiveGates keeps the run from failing but still refuses Qualified' {
    $q = Get-GateQualification -Verdicts @(
        (Get-GateVerdict -Name 'a' -ExitCode 0),
        (Get-GateVerdict -Name 'missing-tool-gate' -ExitCode 2)
    ) -AllowInconclusiveGates
    if ($q.ShouldFail) { throw "-AllowInconclusiveGates must let the run continue" }
    if ($q.Qualified) { throw "-AllowInconclusiveGates must not claim a full qualification" }
    if ($q.Summary -notlike '*missing-tool-gate*') { throw "summary did not name the gate that did not run: $($q.Summary)" }
    if ($q.Summary -notlike '*NOT QUALIFIED*') { throw "summary must say NOT QUALIFIED, not a plain pass: $($q.Summary)" }
}
Assert-Passes 'Get-GateQualification: a real finding still fails even with -AllowInconclusiveGates' {
    $q = Get-GateQualification -Verdicts @((Get-GateVerdict -Name 'broken-gate' -ExitCode 1)) -AllowInconclusiveGates
    if (-not $q.ShouldFail) { throw "-AllowInconclusiveGates must not paper over a real finding" }
}
Assert-Passes 'Get-GateQualification: no gates at all fully qualifies (vacuous true, matches an empty required-gate list)' {
    $q = Get-GateQualification -Verdicts @()
    if (-not $q.Qualified -or $q.ShouldFail) { throw "an empty gate list should not fail the run" }
}

# ---------------------------------------------------------------- Format-BaselineCoverageLine
Assert-Passes 'Format-BaselineCoverageLine: nothing missing keeps the original wording' {
    $line = Format-BaselineCoverageLine -BaselineCount 308 -PresentCount 308 -MissingCount 0
    if ($line -ne 'all 308 baseline extensions still render.') { throw "unexpected line: $line" }
}
Assert-Passes 'Format-BaselineCoverageLine: FALSE-CLAIM GUARD - absent baseline fixtures are reported as absent, never folded into "still render" (finding F37 repro: 226-file corpus, 308-extension baseline)' {
    $line = Format-BaselineCoverageLine -BaselineCount 308 -PresentCount 226 -MissingCount 82
    if ($line -like 'all 308*') { throw "must not claim all 308 baseline extensions render when 82 have no sample: $line" }
    if ($line -notlike '*226/308*') { throw "line must report the present/baseline split (226/308): $line" }
    if ($line -notlike '*82*') { throw "line must name the missing count (82): $line" }
}

# ---------------------------------------------------------------- source-contract: regression.ps1 is actually wired to this lib
$regressionText = Get-Content -LiteralPath (Join-Path $root 'scripts\regression.ps1') -Raw
Assert-Passes 'regression.ps1 dot-sources regression-lib.ps1 and declares -AllowInconclusiveGates' {
    if ($regressionText -notmatch [regex]::Escape('. "$PSScriptRoot\regression-lib.ps1"') -or
        $regressionText -notmatch '\[switch\]\$AllowInconclusiveGates') {
        throw 'regression.ps1 is not wired to regression-lib.ps1'
    }
}
Assert-Passes 'regression.ps1 classifies check-render-sanity.ps1 through Invoke-RequiredGate, not a bare -eq 1 check' {
    if ($regressionText -notmatch 'Invoke-RequiredGate[\s\S]{0,200}check-render-sanity\.ps1') {
        throw 'check-render-sanity.ps1 is no longer routed through Invoke-RequiredGate'
    }
    # The pre-fix shape this finding closes: the ONLY check after the call was `-eq 1`, with no
    # handling at all for any other exit code.
    if ($regressionText -match 'check-render-sanity\.ps1"\s*\r?\n\s*if \(\$LASTEXITCODE -eq 1\) \{') {
        throw 'the old exit-2-is-silently-fine shape is still present'
    }
}
Assert-Passes 'regression.ps1 computes its final verdict through Get-GateQualification' {
    if ($regressionText -notmatch 'Get-GateQualification' -or $regressionText -notmatch 'Format-BaselineCoverageLine') {
        throw 'final summary is not routed through the shared qualification/coverage functions'
    }
}

Write-Host "Regression gate-classification tests passed: $script:passed" -ForegroundColor Green
# The stub-child cases above deliberately leave $LASTEXITCODE at 1 and 2. CI runs this under
# "shell: pwsh", which appends "exit $LASTEXITCODE", so a green run must end with an explicit 0
# or the step fails on the last stub's code (the exact trap CLAUDE.md 6.1 warns about).
exit 0
