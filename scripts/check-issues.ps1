<#
  check-issues.ps1 - the pre-release issue review, made mechanical.

  CLAUDE.md 6.2 has said since 2026-08-13 to read the OPEN issues before every release AND the
  CLOSED ones that were recently commented, because a reporter's follow-up usually lands on a
  thread that was already closed. Running `gh issue list --state open` only covers the first
  half, and the second half is the half that gets missed: v2.1.0 shipped while a long, detailed
  follow-up sat unread on the CLOSED issue #26.

  A rule that lives only in a document is a rule someone forgets at 1am. This prints the exact
  review list instead, so "did anyone say anything since the last release" is a command rather
  than a memory.

  Informational by design - it does NOT block a release. Blocking on open issues would have
  blocked the very release that FIXED the open issue, and blocking on comments would fire on
  every "thanks, that worked". The value is that the list is unmissable, not that it is a gate.

  Usage:
    pwsh scripts/check-issues.ps1              # since the last published release
    pwsh scripts/check-issues.ps1 -Days 14     # or a fixed window
    pwsh scripts/check-issues.ps1 -Json        # machine-readable, for a wrapper
#>
[CmdletBinding()]
param(
    [int]$Days = 0,
    [switch]$Json,
    [string]$Repo = 'LunarWerxs/SageThumbs-2k'
)

$ErrorActionPreference = 'Stop'

function Fail([string]$m) { Write-Host "[issues] $m" -ForegroundColor Red; exit 1 }

if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
    Fail 'the GitHub CLI (gh) is not on PATH - cannot review issues'
}

# The window: since the last published release, unless -Days overrides it. A release is the
# right default boundary because "what changed since users last got a build" is the question.
if ($Days -gt 0) {
    $since = (Get-Date).ToUniversalTime().AddDays(-$Days)
    $windowLabel = "the last $Days day(s)"
} else {
    $lastRelease = gh release list --repo $Repo --limit 1 --json publishedAt --jq '.[0].publishedAt' 2>$null
    if ([string]::IsNullOrWhiteSpace($lastRelease)) {
        $since = (Get-Date).ToUniversalTime().AddDays(-30)
        $windowLabel = 'the last 30 days (no published release found)'
    } else {
        $since = ([datetime]$lastRelease).ToUniversalTime()
        $windowLabel = "the last release ($($since.ToString('u')))"
    }
}
$sinceIso = $since.ToString('yyyy-MM-ddTHH:mm:ssZ')

# Open issues: the half everyone remembers.
$open = @()
$openRaw = gh issue list --repo $Repo --state open --limit 100 --json number,title,updatedAt,author 2>$null
if (-not [string]::IsNullOrWhiteSpace($openRaw)) { $open = @($openRaw | ConvertFrom-Json) }

# Every issue comment since the window, in ONE call. This is the half that gets missed: it
# catches follow-ups on CLOSED threads, which `gh issue list` can never show.
$comments = @()
$cRaw = gh api --paginate "repos/$Repo/issues/comments?since=$sinceIso&per_page=100" 2>$null
if (-not [string]::IsNullOrWhiteSpace($cRaw)) {
    $parsed = $cRaw | ConvertFrom-Json
    foreach ($c in $parsed) {
        # issue_url tail is the issue number; PR review comments use a different endpoint, so
        # everything here is a genuine issue/PR conversation comment.
        $num = ($c.issue_url -split '/')[-1]
        $comments += [pscustomobject]@{
            Issue  = [int]$num
            Author = $c.user.login
            At     = ([datetime]$c.created_at).ToUniversalTime()
            Url    = $c.html_url
            Length = ($c.body ?? '').Length
        }
    }
}

# Group the commented threads, and mark which are already closed - those are the traps.
$touched = @()
foreach ($g in ($comments | Group-Object Issue | Sort-Object { [int]$_.Name })) {
    $num = [int]$g.Name
    $meta = gh issue view $num --repo $Repo --json state,title,author 2>$null
    if ([string]::IsNullOrWhiteSpace($meta)) { continue }
    $m = $meta | ConvertFrom-Json
    $others = @($g.Group | Where-Object { $_.Author -ne 'github-actions[bot]' })
    if ($others.Count -eq 0) { continue }
    $touched += [pscustomobject]@{
        Number   = $num
        Title    = $m.title
        State    = $m.state
        Comments = $others.Count
        Longest  = ($others | Measure-Object Length -Maximum).Maximum
        Latest   = ($others | Measure-Object At -Maximum).Maximum
        Authors  = (($others | Select-Object -ExpandProperty Author -Unique) -join ', ')
        Url      = $others[-1].Url
    }
}

if ($Json) {
    [pscustomobject]@{ since = $sinceIso; open = $open; commented = $touched } | ConvertTo-Json -Depth 6
    exit 0
}

Write-Host ''
Write-Host '================ PRE-RELEASE ISSUE REVIEW ================' -ForegroundColor Cyan
Write-Host "window: since $windowLabel" -ForegroundColor DarkGray
Write-Host ''

Write-Host "OPEN issues ($($open.Count))" -ForegroundColor Yellow
if ($open.Count -eq 0) {
    Write-Host '  none' -ForegroundColor DarkGray
} else {
    foreach ($i in $open) { "  #{0,-4} {1}" -f $i.number, $i.title | Write-Host }
}
Write-Host ''

$closedTouched = @($touched | Where-Object { $_.State -eq 'CLOSED' })
Write-Host "CLOSED issues commented since then ($($closedTouched.Count)) <- the ones that get missed" -ForegroundColor Yellow
if ($closedTouched.Count -eq 0) {
    Write-Host '  none' -ForegroundColor DarkGray
} else {
    foreach ($t in $closedTouched) {
        "  #{0,-4} {1}" -f $t.Number, $t.Title | Write-Host -ForegroundColor White
        "        {0} comment(s) by {1}, longest {2} chars, latest {3}" -f `
            $t.Comments, $t.Authors, $t.Longest, $t.Latest.ToString('u') | Write-Host -ForegroundColor DarkGray
        "        $($t.Url)" | Write-Host -ForegroundColor DarkGray
        # A long comment on a closed thread is almost always a real report, not a thank-you.
        if ($t.Longest -gt 1200) {
            '        ^^ LONG follow-up - read this before releasing' | Write-Host -ForegroundColor Red
        }
    }
}

$openTouched = @($touched | Where-Object { $_.State -eq 'OPEN' })
if ($openTouched.Count -gt 0) {
    Write-Host ''
    Write-Host "OPEN issues with new comments ($($openTouched.Count))" -ForegroundColor Yellow
    foreach ($t in $openTouched) {
        "  #{0,-4} {1}  ({2} new by {3})" -f $t.Number, $t.Title, $t.Comments, $t.Authors | Write-Host
    }
}

Write-Host ''
if ($open.Count -eq 0 -and $touched.Count -eq 0) {
    Write-Host '[issues] nothing to review.' -ForegroundColor Green
} else {
    Write-Host '[issues] READ THE ABOVE before cutting a release. Informational, not a gate.' -ForegroundColor Cyan
}
Write-Host '==========================================================' -ForegroundColor Cyan
Write-Host ''
exit 0
