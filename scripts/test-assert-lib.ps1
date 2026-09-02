<#
  test-assert-lib.ps1 - the shared PASS/FAIL assertion helpers for this repo's scripts/test-*.ps1
  harness scripts. Dot-source it (`. (Join-Path $PSScriptRoot 'test-assert-lib.ps1')`) rather
  than redefining these functions locally.

  Before this file existed, ~10 scripts each carried their own copy of the same handful of
  functions with small, easy-to-miss behavioural differences (one Assert-Passes forgot to
  suppress the body's own output with `*> $null`; one Assert-FailsLike threw its "expected
  FAILURE" sentinel INSIDE the try block it was itself catching, so a body that unexpectedly
  succeeded reported a confusing wrong-pattern error instead of a clean one). One copy, one
  behaviour, fixed once here.

  Every function increments $script:passed IN THE CALLER'S SCOPE: dot-sourcing this file runs
  it inside the caller's own script scope (PowerShell does not give a dot-sourced file its own
  scope), so a function's `$script:` here resolves to whichever script dot-sourced it - each
  caller gets its own independent counter. Initialise `$script:passed = 0` in the caller before
  the first assertion (every migrated script already does, for its final summary line).
#>

# Run $Body, suppressing its own output, and count it as PASS. Re-throws (with the original
# message attached) if $Body itself throws - a scriptblock that was expected to succeed but
# did not.
function Assert-Passes {
    param([Parameter(Mandatory)][string]$Name, [Parameter(Mandatory)][scriptblock]$Body)
    try {
        & $Body *> $null
    } catch {
        throw "expected PASS for '$Name', got: $($_.Exception.Message)"
    }
    Write-Host "  PASS  $Name" -ForegroundColor Green
    $script:passed++
}

# Run $Body, suppressing its own output, and require it to throw a message matching the
# wildcard $Pattern (PowerShell -like syntax: '*substring*'). Fails closed on either wrong
# outcome: the body did not throw at all, or it threw the wrong thing. The "did not throw" path
# is checked OUTSIDE the try/catch that examines the thrown message, so a body that unexpectedly
# succeeds always reports the clean "expected FAILURE" message rather than a confusing
# pattern-mismatch built from a sentinel this function threw at itself.
function Assert-FailsLike {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string]$Pattern,
        [Parameter(Mandatory)][scriptblock]$Body
    )
    $threw = $false
    $message = $null
    try {
        & $Body *> $null
    } catch {
        $threw = $true
        $message = $_.Exception.Message
    }
    if (-not $threw) { throw "expected FAILURE for '$Name'" }
    if ($message -notlike $Pattern) {
        throw "expected '$Name' to fail like '$Pattern', got: $message"
    }
    Write-Host "  PASS  $Name (failed closed)" -ForegroundColor Green
    $script:passed++
}

# Same shape as Assert-FailsLike, but $Pattern is a REGEX (-match/-notmatch), for the two
# callers (test-magick-dependency-freshness.ps1, test-magick-packaging.ps1) whose existing
# patterns rely on regex semantics (e.g. anchors, character classes) rather than wildcard glob.
# Parameter order matches those callers' existing (Name, Action, Pattern) call sites.
function Assert-ThrowsLike {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][scriptblock]$Action,
        [Parameter(Mandatory)][string]$Pattern
    )
    $threw = $false
    $message = $null
    try {
        & $Action
    } catch {
        $threw = $true
        $message = $_.Exception.Message
    }
    if (-not $threw) { throw "Expected failure did not occur: $Name" }
    if ($message -notmatch $Pattern) {
        throw "${Name}: expected /$Pattern/, got: $message"
    }
    Write-Host "  PASS $Name" -ForegroundColor Green
    $script:passed++
}

# Exact (case-sensitive) equality check, for the parser/ordering unit tests in
# test-magick-dependency-freshness.ps1.
function Assert-Equal {
    param([Parameter(Mandatory)][string]$Name, $Actual, $Expected)
    if ($Actual -cne $Expected) { throw "${Name}: expected '$Expected', got '$Actual'" }
    Write-Host "  PASS $Name" -ForegroundColor Green
    $script:passed++
}
