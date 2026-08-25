<#
  check-winget.ps1 - what happened to the winget PRs AFTER we opened them.

  `winget-submit.ps1` opens the pull request and exits. Nothing ever looked at it again, so
  the only way we ever learned that a submission had failed was a GitHub notification landing
  in Michael's inbox days later. That is exactly how it went wrong:

    - v2.3.0 (PR 421299) failed Installation Validation on 2026-08-20 because Microsoft
      Defender BLOCKED the arm64 installer download on the validation VM
      (0x8A15002D APPINSTALLER_CLI_ERROR_INSTALLER_SECURITY_CHECK_FAILED, label
      Validation-Defender-Error). A retry robot re-posts that failure roughly every 18 hours,
      forever, until the PR is closed.
    - v2.3.1 (PR 422556) failed Installation Validation on 2026-08-22.
    - Both sat OPEN and failing for days while 2.3.2, 2.4.0 and 2.4.1 sailed past them, so
      nothing was actually broken for users and nobody had any reason to look.

  The failure mode is not "the submission broke". It is "the submission broke and we found
  out from an email". This makes the answer a command.

  It is INFORMATIONAL by default and deliberately not a gate: a stale failing PR for a
  superseded version must never block the release that supersedes it. Use -Gate in a context
  that genuinely wants to stop.

  Usage:
    pwsh scripts/check-winget.ps1                    # state of every open winget PR of ours
    pwsh scripts/check-winget.ps1 -Version 2.4.1     # just that one
    pwsh scripts/check-winget.ps1 -Version 2.4.1 -Watch   # follow it until validation ends
    pwsh scripts/check-winget.ps1 -Gate              # exit 1 if any open PR is failing
    pwsh scripts/check-winget.ps1 -Json              # machine-readable
#>
[CmdletBinding()]
param(
    [string]$Version,
    [switch]$Watch,
    [int]$TimeoutMinutes = 90,
    [int]$PollSeconds = 120,
    [switch]$Gate,
    [switch]$Json,
    [string]$Upstream = 'microsoft/winget-pkgs',
    [string]$PkgId = 'LunarWerxs.SageThumbs2K'
)

$ErrorActionPreference = 'Stop'

function Fail([string]$m) { Write-Host "[winget] $m" -ForegroundColor Red; exit 1 }

if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
    Fail 'the GitHub CLI (gh) is not on PATH - cannot inspect the winget PRs'
}

# A label whose name ends in -Error, plus the two triage labels the bot applies when it wants
# the AUTHOR to act, are the machine-readable form of "this one is not going to merge on its
# own". Checking labels as well as check-runs matters because the retry robot can re-open a
# verdict on a PR whose check-runs have long since gone green-or-stale.
$BadLabels = @('Needs-Author-Feedback', 'Needs-Attention', 'Validation-Guide')

function Get-WingetPrs {
    $q = "repo:$Upstream is:pr is:open $PkgId in:title"
    $raw = gh api "search/issues?q=$([uri]::EscapeDataString($q))&per_page=50" 2>$null
    if ([string]::IsNullOrWhiteSpace($raw)) { return @() }
    $items = ($raw | ConvertFrom-Json).items
    if (-not $items) { return @() }
    # in:title is a full-text match, not an anchor, so confirm the title really is ours.
    @($items | Where-Object { $_.title -match [regex]::Escape($PkgId) })
}

function Get-PrState([int]$Number) {
    $meta = gh pr view $Number --repo $Upstream --json number,title,url,createdAt,labels,headRefOid 2>$null
    if ([string]::IsNullOrWhiteSpace($meta)) { return $null }
    $m = $meta | ConvertFrom-Json

    $checks = @()
    $raw = gh api "repos/$Upstream/commits/$($m.headRefOid)/check-runs?per_page=100" 2>$null
    if (-not [string]::IsNullOrWhiteSpace($raw)) {
        $checks = @(($raw | ConvertFrom-Json).check_runs |
            Where-Object { $_.conclusion -ne 'skipped' } |
            ForEach-Object {
                [pscustomobject]@{ Name = $_.name; Status = $_.status; Conclusion = $_.conclusion }
            } | Sort-Object Name)
    }

    $labels = @($m.labels | ForEach-Object { $_.name })
    $failedChecks = @($checks | Where-Object { $_.Conclusion -in @('failure', 'timed_out', 'cancelled', 'action_required') })
    $runningChecks = @($checks | Where-Object { $_.Status -ne 'completed' })
    $errLabels = @($labels | Where-Object { $_ -match '-Error$' -or $_ -in $BadLabels })

    # The most recent human-or-bot comment, first line only. The winget bots explain the
    # failure in prose that no label carries, and that prose is usually the actionable part.
    $lastComment = $null
    $cRaw = gh api "repos/$Upstream/issues/$Number/comments?per_page=100" 2>$null
    if (-not [string]::IsNullOrWhiteSpace($cRaw)) {
        $cs = @($cRaw | ConvertFrom-Json)
        if ($cs.Count -gt 0) {
            $c = $cs[-1]
            $body = ($c.body ?? '') -replace '\r', ''
            $firstReal = @($body -split "`n" | Where-Object { $_.Trim() -ne '' -and $_ -notmatch '^\s*>' }) |
                Select-Object -First 1
            $lastComment = [pscustomobject]@{
                Author = $c.user.login
                At     = ([datetime]$c.created_at).ToUniversalTime()
                Line   = ($firstReal ?? '').Trim()
            }
        }
    }

    $version = if ($m.title -match 'version\s+(\S+)\s*$') { $Matches[1] } else { '?' }
    $ageHours = [math]::Round(((Get-Date).ToUniversalTime() - ([datetime]$m.createdAt).ToUniversalTime()).TotalHours, 1)

    # 'Validation-Completed' + 'Azure-Pipeline-Passed' is winget's own way of saying the
    # submission cleared every check and is only waiting on their merge queue. Without this
    # arm a fully-passed PR reads as WAITING, identical to one that has not been looked at
    # yet, which is the same "can't tell success from silence" defect this script exists to
    # end. Measured on v2.4.1: 11/11 green, both labels applied, mergedAt still null.
    $passedLabels = @('Validation-Completed', 'Azure-Pipeline-Passed')
    $verdict =
        if ($failedChecks.Count -gt 0 -or $errLabels.Count -gt 0) { 'FAILING' }
        elseif ($runningChecks.Count -gt 0) { 'VALIDATING' }
        elseif (@($labels | Where-Object { $_ -in $passedLabels }).Count -gt 0) { 'PASSED' }
        elseif ($ageHours -gt 24) { 'STALE' }
        else { 'WAITING' }

    [pscustomobject]@{
        Number      = $m.number
        Version     = $version
        Url         = $m.url
        AgeHours    = $ageHours
        Labels      = $labels
        Checks      = $checks
        Failed      = @($failedChecks | ForEach-Object { $_.Name })
        Running     = @($runningChecks | ForEach-Object { $_.Name })
        ErrorLabels = $errLabels
        LastComment = $lastComment
        Verdict     = $verdict
    }
}

function Write-PrLine($p) {
    $colour = switch ($p.Verdict) {
        'FAILING'    { 'Red' }
        'STALE'      { 'Yellow' }
        'VALIDATING' { 'Cyan' }
        'PASSED'     { 'Green' }
        default      { 'DarkGray' }
    }
    "  {0,-11} #{1,-7} v{2,-8} {3,6}h old" -f $p.Verdict, $p.Number, $p.Version, $p.AgeHours |
        Write-Host -ForegroundColor $colour
    "              $($p.Url)" | Write-Host -ForegroundColor DarkGray
    if ($p.Failed.Count -gt 0) {
        "              failed: $($p.Failed -join ', ')" | Write-Host -ForegroundColor Red
    }
    if ($p.ErrorLabels.Count -gt 0) {
        "              labels: $($p.ErrorLabels -join ', ')" | Write-Host -ForegroundColor Red
    }
    if ($p.Running.Count -gt 0) {
        "              running: $($p.Running -join ', ')" | Write-Host -ForegroundColor DarkGray
    }
    if ($p.LastComment) {
        "              last: $($p.LastComment.Author) $($p.LastComment.At.ToString('u'))" |
            Write-Host -ForegroundColor DarkGray
        if ($p.LastComment.Line) {
            $line = $p.LastComment.Line
            if ($line.Length -gt 110) { $line = $line.Substring(0, 110) + '...' }
            "                    $line" | Write-Host -ForegroundColor DarkGray
        }
    }
}

# ------------------------------------------------------------------------------ gather
$prs = Get-WingetPrs
if ($Version) {
    $prs = @($prs | Where-Object { $_.title -match ("version\s+" + [regex]::Escape($Version) + '\s*$') })
    if ($prs.Count -eq 0) {
        Write-Host "[winget] no OPEN PR for version $Version - it either merged already or was never submitted." -ForegroundColor Yellow
        exit 0
    }
}

$states = @()
foreach ($pr in $prs) {
    $s = Get-PrState -Number $pr.number
    if ($s) { $states += $s }
}

# ------------------------------------------------------------------------------ watch
if ($Watch) {
    if (-not $Version) { Fail '-Watch needs -Version (there is nothing to follow otherwise)' }
    if ($states.Count -eq 0) { Fail "could not read the state of the PR for $Version" }
    $deadline = (Get-Date).AddMinutes($TimeoutMinutes)
    $target = $states[0]
    Write-Host ''
    Write-Host "[winget] following PR #$($target.Number) (v$($target.Version)) until validation ends" -ForegroundColor Cyan
    Write-Host "         $($target.Url)" -ForegroundColor DarkGray
    while ($true) {
        $target = Get-PrState -Number $target.Number
        if (-not $target) { Fail 'the PR disappeared while watching it' }
        $stamp = (Get-Date).ToUniversalTime().ToString('HH:mm:ss')
        if ($target.Verdict -eq 'FAILING') {
            Write-Host ''
            Write-Host "[winget] $stamp  VALIDATION FAILED" -ForegroundColor Red
            Write-PrLine $target
            Write-Host ''
            Write-Host '         Read the bot comment on the PR - it names the actual cause.' -ForegroundColor Yellow
            Write-Host '         A Validation-Defender-Error is a cloud reputation verdict on an' -ForegroundColor Yellow
            Write-Host '         unsigned zero-prevalence binary, NOT a signature hit. See docs/AV-SUBMISSION.md.' -ForegroundColor Yellow
            if ($Gate) { exit 1 }
            exit 0
        }
        if ($target.Running.Count -eq 0) {
            Write-Host ''
            $verb = if ($target.Verdict -eq 'PASSED') {
                'VALIDATION PASSED - waiting on Microsoft''s merge queue now'
            } else {
                'validation finished with no failures'
            }
            Write-Host "[winget] $stamp  $verb" -ForegroundColor Green
            Write-PrLine $target
            exit 0
        }
        Write-Host "         $stamp  still running: $($target.Running -join ', ')" -ForegroundColor DarkGray
        if ((Get-Date) -gt $deadline) {
            Write-Host ''
            Write-Host "[winget] gave up after $TimeoutMinutes minutes - validation is still running." -ForegroundColor Yellow
            Write-Host "         Re-check later: pwsh scripts/check-winget.ps1 -Version $Version" -ForegroundColor Yellow
            exit 0
        }
        Start-Sleep -Seconds $PollSeconds
    }
}

# ------------------------------------------------------------------------------ report
if ($Json) {
    [pscustomobject]@{ upstream = $Upstream; package = $PkgId; prs = $states } | ConvertTo-Json -Depth 6
    $failingJson = @($states | Where-Object { $_.Verdict -eq 'FAILING' })
    if ($Gate -and $failingJson.Count -gt 0) { exit 1 }
    exit 0
}

Write-Host ''
Write-Host '================ WINGET SUBMISSION STATUS ================' -ForegroundColor Cyan
Write-Host "open PRs for $PkgId in $Upstream" -ForegroundColor DarkGray
Write-Host ''

if ($states.Count -eq 0) {
    Write-Host '  none open - every submission has merged or been closed.' -ForegroundColor Green
} else {
    foreach ($s in ($states | Sort-Object Number)) { Write-PrLine $s; Write-Host '' }
}

$failing = @($states | Where-Object { $_.Verdict -eq 'FAILING' })
$stale = @($states | Where-Object { $_.Verdict -eq 'STALE' })

Write-Host ''
if ($failing.Count -gt 0) {
    Write-Host "[winget] $($failing.Count) submission(s) are FAILING and will not merge by themselves." -ForegroundColor Red
    Write-Host '         If the version is already superseded, CLOSE the PR: a failing winget PR' -ForegroundColor Yellow
    Write-Host '         is re-tested by a robot roughly every 18 hours and notifies you each time.' -ForegroundColor Yellow
    Write-Host "         gh pr close <n> --repo $Upstream --comment 'Superseded by vX.Y.Z.'" -ForegroundColor Yellow
} elseif ($stale.Count -gt 0) {
    Write-Host "[winget] $($stale.Count) submission(s) have been open over 24h with no failure - Microsoft's clock." -ForegroundColor Yellow
} else {
    $passed = @($states | Where-Object { $_.Verdict -eq 'PASSED' })
    if ($passed.Count -gt 0) {
        Write-Host "[winget] nothing needs attention. $($passed.Count) passed and waiting on Microsoft's merge queue." -ForegroundColor Green
    } else {
        Write-Host '[winget] nothing needs attention.' -ForegroundColor Green
    }
}
Write-Host '==========================================================' -ForegroundColor Cyan
Write-Host ''

if ($Gate -and $failing.Count -gt 0) { exit 1 }
exit 0
