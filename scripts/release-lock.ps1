<#
.SYNOPSIS
  Wait while a release is being cut from this checkout, unless this process is that release.

.DESCRIPTION
  release.ps1 writes .git\RELEASE-IN-PROGRESS at its [2/6] guard (tag, pid, token, start time),
  hands the token to everything it runs through the RELEASE_LOCK_TOKEN environment variable,
  and removes the marker in its `finally`. The gates that build or test in this checkout -
  preflight.ps1 (so every push), verify.ps1, regression.ps1 and scripts/refactor/gate.py - call
  this first, and fairjob does the same for any command run in the tree, so another session's
  cargo run waits for the release instead of racing it. Twice in September 2026 that race cost a
  release run: Cargo.lock rewritten between the x64 and ARM64 legs (3.2.0's first run), and the
  tree edited mid-build (2026-09-15). The pre-commit hook already refused commits; this is the
  other half, decided by Michael on 2026-09-22 ("Build the lock first").

  Nothing the release does not own is locked: a marker whose pid is gone, or that is three hours
  old (the pre-commit hook's rule), is a dead run's leftover and is ignored, not honoured.

  Exits 0 once the tree is free - at once, or after waiting. -Check reports without waiting and
  exits 1 while a release holds the tree.
#>
param(
    [switch]$Check,
    [ValidateRange(1, 300)] [int]$PollSeconds = 15
)
$ErrorActionPreference = 'Stop'

$marker = Join-Path (Split-Path -Parent $PSScriptRoot) '.git\RELEASE-IN-PROGRESS'

# The release holding this tree, or $null when there is none this process has to wait for.
function Get-ReleaseLock {
    if (-not (Test-Path -LiteralPath $marker -PathType Leaf)) { return $null }
    if (((Get-Date) - (Get-Item -LiteralPath $marker).LastWriteTime).TotalHours -ge 3) { return $null }
    $text = [string](Get-Content -LiteralPath $marker -Raw -ErrorAction SilentlyContinue)
    # Only a marker that names its owner can lock: without a token the release's own children
    # could not tell themselves apart from anyone else, and would wait on their own parent.
    if ($text -notmatch 'token=(\S+)') { return $null }
    # The release itself, and everything it started, carry its token.
    if ($env:RELEASE_LOCK_TOKEN -eq $Matches[1]) { return $null }
    $owner = if ($text -match 'pid=(\d+)') { [int]$Matches[1] } else { 0 }
    if ($owner -and -not (Get-Process -Id $owner -ErrorAction SilentlyContinue)) { return $null }
    $text.Trim()
}

$held = Get-ReleaseLock
if (-not $held) { exit 0 }
if ($Check) {
    Write-Host "release lock: HELD by a release being cut from this tree ($held)"
    exit 1
}
[Console]::Error.WriteLine("release lock: a release is being cut from this tree ($held). Waiting for it to finish before building here; checking every $PollSeconds s.")
$since = Get-Date
$polls = 0
while (Get-ReleaseLock) {
    Start-Sleep -Seconds $PollSeconds
    $polls++
    if ($polls % 20 -eq 0) {
        [Console]::Error.WriteLine("release lock: still waiting ($([int]((Get-Date) - $since).TotalMinutes) min so far)")
    }
}
[Console]::Error.WriteLine("release lock: the release finished after $([int]((Get-Date) - $since).TotalMinutes) min; carrying on.")
exit 0
