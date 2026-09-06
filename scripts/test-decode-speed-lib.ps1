<#
  Offline tests for decode-speed-lib.ps1's Get-ConfirmedMeasurement (finding F37, 2026-09-05
  audit): a suspect's confirmation re-decode can simply FAIL (Measure-Decode omits a failed
  decode from its map), and check-decode-speed.ps1 used to fall back to the ORIGINAL suspect
  reading in that case - silently confirming or clearing a format using a number the re-decode
  never reproduced. No build, no st2k.exe, no corpus.

  BEFORE THIS FIX: both confirmation loops in check-decode-speed.ps1 read
  `if ($again.ContainsKey($name)) { $again[$name] } else { <the original suspect reading> }`.
  The source-contract assertions at the bottom of this file fail against that shape (verified
  by hand - see the finding's verification notes).
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$script:passed = 0
. (Join-Path $PSScriptRoot 'test-assert-lib.ps1')

# decode-speed-lib.ps1's header requires PresentationCore to be loaded before it is dot-sourced
# (the WIC timing side uses it at file scope) - same contract check-decode-speed.ps1 follows.
Add-Type -AssemblyName PresentationCore
. (Join-Path $PSScriptRoot 'decode-speed-lib.ps1')

# ---------------------------------------------------------------- Get-ConfirmedMeasurement
Assert-Passes 'Get-ConfirmedMeasurement: a fresh re-decode reading is used, not treated as inconclusive' {
    $again = @{ 'sample.avif' = 12.5 }
    $r = Get-ConfirmedMeasurement -Name 'sample.avif' -Confirmed $again
    if ($r.Inconclusive) { throw 'a present confirmation reading must not be Inconclusive' }
    if ($r.Value -ne 12.5) { throw "expected 12.5, got $($r.Value)" }
}
Assert-Passes 'Get-ConfirmedMeasurement: a MISSING re-decode reading (failed decode) is Inconclusive, not the old value' {
    $again = @{ 'other.avif' = 99.0 }   # a different sample confirmed fine; ours is simply absent
    $r = Get-ConfirmedMeasurement -Name 'sample.avif' -Confirmed $again
    if (-not $r.Inconclusive) { throw 'a missing confirmation reading must be Inconclusive' }
    if ($null -ne $r.Value) { throw "expected no Value for an inconclusive reading, got $($r.Value)" }
}
Assert-Passes 'Get-ConfirmedMeasurement: an empty confirmation map is Inconclusive for any name' {
    $r = Get-ConfirmedMeasurement -Name 'anything.png' -Confirmed @{}
    if (-not $r.Inconclusive) { throw 'an empty confirmation map must report Inconclusive' }
}

# ---------------------------------------------------------------- source-contract: check-decode-speed.ps1 is actually wired to it
$checkText = Get-Content -LiteralPath (Join-Path $root 'scripts\check-decode-speed.ps1') -Raw
Assert-Passes 'check-decode-speed.ps1 routes BOTH confirmation loops through Get-ConfirmedMeasurement' {
    $calls = [regex]::Matches($checkText, 'Get-ConfirmedMeasurement').Count
    if ($calls -lt 2) { throw "expected Get-ConfirmedMeasurement called for both gate A and gate B, found $calls call(s)" }
}
Assert-Passes 'check-decode-speed.ps1 no longer falls back to the original suspect reading on a missing confirmation' {
    # The pre-fix shape this finding closes, in both loops: use the stale reading as if it were
    # a fresh one instead of reporting the missing measurement.
    if ($checkText -match [regex]::Escape('if ($again.ContainsKey($sp.row.name)) { $again[$sp.row.name] } else { $sp.row.mine }')) {
        throw 'gate A confirmation still falls back to the original (pre-suspect) reading'
    }
    if ($checkText -match [regex]::Escape('if ($again.ContainsKey($sp.name)) { $again[$sp.name] } else { $sp.now }')) {
        throw 'gate B confirmation still falls back to the original (pre-suspect) reading'
    }
}
Assert-Passes 'check-decode-speed.ps1 reports a failed re-decode as its own INCONCLUSIVE outcome' {
    if ($checkText -notmatch 'aInconclusive' -or $checkText -notmatch 'bInconclusive') {
        throw 'no separate inconclusive bucket for a suspect whose confirmation re-decode failed'
    }
    if ($checkText -notmatch '(?i)INCONCLUSIVE.*re-decode failed') {
        throw 'no message distinguishing a failed re-decode from a cleared-on-retry pass'
    }
}

Write-Host "Decode-speed confirmation-logic tests passed: $script:passed" -ForegroundColor Green
