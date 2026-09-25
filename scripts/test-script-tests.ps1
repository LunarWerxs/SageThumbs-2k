<#
  test-script-tests.ps1 - runs the co-located tests of the repo's support scripts:
  scripts/test_<name>.py (stdlib unittest, loading its target by path) and scripts/<name>.test.mjs
  (node:test, no dependencies). The Architect's churn-hotspots check counts a script as tested only
  when such a sibling exists; a sibling nobody runs is a lie, so this is the runner, wired into
  preflight.ps1, gate.py prepush and CI beside the other consistency scripts (added 2026-09-20).
  Missing interpreter with test files present = red, never a silent skip.
#>
$ErrorActionPreference = 'Stop'
$scripts = $PSScriptRoot
$failed = 0
$ran = 0

$pyTests = @(Get-ChildItem -LiteralPath $scripts -Filter 'test_*.py' -File | Sort-Object Name)
if ($pyTests.Count -gt 0) {
    $py = Get-Command python -ErrorAction SilentlyContinue
    if (-not $py) {
        Write-Host "[script-tests] python not found; $($pyTests.Count) Python test file(s) NOT run" -ForegroundColor Red
        exit 1
    }
    foreach ($t in $pyTests) {
        & $py.Source $t.FullName
        if ($LASTEXITCODE -ne 0) {
            Write-Host "[script-tests] FAIL $($t.Name)" -ForegroundColor Red
            $failed++
        } else {
            $ran++
        }
    }
}

$mjsTests = @(Get-ChildItem -LiteralPath $scripts -Filter '*.test.mjs' -File | Sort-Object Name)
if ($mjsTests.Count -gt 0) {
    $node = Get-Command node -ErrorAction SilentlyContinue
    if (-not $node) {
        Write-Host "[script-tests] node not found; $($mjsTests.Count) JS test file(s) NOT run" -ForegroundColor Red
        exit 1
    }
    foreach ($t in $mjsTests) {
        & $node.Source --test $t.FullName
        if ($LASTEXITCODE -ne 0) {
            Write-Host "[script-tests] FAIL $($t.Name)" -ForegroundColor Red
            $failed++
        } else {
            $ran++
        }
    }
}

if ($failed -gt 0) {
    Write-Host "[script-tests] $failed of $($ran + $failed) test file(s) FAILED" -ForegroundColor Red
    exit 1
}
Write-Host "[script-tests] ALL GREEN ($ran test file(s))" -ForegroundColor Green
exit 0
