<#
  check-av.ps1 - what antivirus engines say about installers we have ALREADY PUBLISHED.

  The release pipeline scans every installer on VirusTotal BEFORE publishing it
  (`push_to_vt.py --gate`, release.ps1 step 4b) and refuses to publish a bad one. That gate
  works. What was missing is the other half:

    Engines keep scoring a file for as long as it exists, and the number MOVES after release.
    v2.5.0's x64 installer passed the gate on 2026-08-27 and read 9 detections four days later.
    Nothing on our side ever looked again, so the way we found out was a user commenting on
    GitHub issue #30 that our flags "are usually high". They were right, and we had no way of
    knowing.

  This is the same failure shape as the winget submissions - see check-winget.ps1, which exists
  because "the submission broke and we found out from an email" is a defect in LOOKING, not in
  the thing being looked at. The fix is the same: make the answer a command.

  WHAT IT IS AND IS NOT A GATE FOR
  --------------------------------
  This project is an UNSIGNED Inno Setup installer with a fresh low-prevalence hash every
  release, so a handful of heuristic/ML detections is its permanent background state, not news.
  A check that fails on those is a check everyone learns to ignore, and then it misses the real
  thing. So, by default this is INFORMATIONAL and exits 0.

  It fails (with -Gate) on exactly one condition: a TIER-1 ENGINE REPORTING A REAL SIGNATURE
  MATCH on a published artifact. That classification is not duplicated here - it comes from
  push_to_vt.py, which owns TIER1 and ML_MARKERS, so there is no second list to drift.

  What it REPORTS loudly without failing:
    * any published installer Microsoft currently flags, with the exact detection name, because
      that is the verdict that actually blocks users (Defender is on by default everywhere) and
      it is the field the Microsoft false-positive portal requires;
    * how each published artifact compares with the rest of its own architecture, since x64 and
      arm64 have measurably different bands and one global number would be wrong for both.

  Usage:
    pwsh scripts/check-av.ps1                     # the last 6 releases
    pwsh scripts/check-av.ps1 -Releases 12        # go further back
    pwsh scripts/check-av.ps1 -Version 2.5.0      # just one
    pwsh scripts/check-av.ps1 -Gate               # exit 1 on a tier-1 signature match
    pwsh scripts/check-av.ps1 -Json               # machine-readable

  Needs the GitHub CLI (for the published asset digests) and VIRUSTOTAL_API_KEY in .env.
  It only ever LOOKS UP hashes; it never uploads a published binary.
#>
[CmdletBinding()]
param(
    [string]$Version,
    [int]$Releases = 6,
    [switch]$Gate,
    [switch]$Json,
    [string]$Repo = 'LunarWerxs/SageThumbs-2k'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent

function Note([string]$m, [string]$c = 'Gray') { Write-Host "[av] $m" -ForegroundColor $c }

function Skip([string]$why) {
    # A missing prerequisite must never read as "the installers are fine". Say so and exit 0:
    # this is a reporting tool, and a machine that cannot run it has learned nothing either way.
    Note "SKIPPED - $why" 'Yellow'
    Note 'This is not a pass. Nothing was checked.' 'Yellow'
    exit 0
}

if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
    Skip 'the GitHub CLI (gh) is not on PATH, so the published asset digests cannot be read'
}
$vt = Join-Path $root 'push_to_vt.py'
if (-not (Test-Path $vt)) { Skip "push_to_vt.py is not present at $vt" }
if (-not (Test-Path (Join-Path $root '.env'))) { Skip 'no .env, so there is no VirusTotal key' }

# ---------------------------------------------------------------- published artifacts
$tags = if ($Version) { @("v$($Version.TrimStart('v'))") } else {
    (gh release list --repo $Repo --limit $Releases --json tagName --jq '.[].tagName') -split "`n" |
        Where-Object { $_ }
}
if (-not $tags) { Skip "no releases found for $Repo" }

$artifacts = foreach ($tag in $tags) {
    $raw = gh api "repos/$Repo/releases/tags/$tag" 2>$null
    if (-not $raw) { Note "no release data for $tag" 'Yellow'; continue }
    foreach ($a in ($raw | ConvertFrom-Json).assets) {
        # Installers only. The portable zips are a different distribution shape with a
        # different (much quieter) detection profile, and mixing them into one band would
        # flatter the number that actually matters.
        if ($a.name -notlike 'SageThumbs2K-Setup-*') { continue }
        if (-not $a.digest) { continue }
        [pscustomobject]@{
            Tag       = $tag
            Name      = $a.name
            Arch      = if ($a.name -like '*arm64*') { 'arm64' } else { 'x64' }
            Sha256    = $a.digest -replace '^sha256:', ''
            Downloads = $a.download_count
        }
    }
}
if (-not $artifacts) { Skip 'no installer assets with digests found' }

# ---------------------------------------------------------------- VirusTotal, by hash
Note "looking up $($artifacts.Count) published installer(s) on VirusTotal (no uploads)"
$rows = foreach ($a in $artifacts) {
    # The public VirusTotal API allows 4 requests a minute; anything faster gets 429s that
    # would read as failures. Pace it rather than retrying into the limit.
    if ($rows) { Start-Sleep -Seconds 16 }
    $out = & python $vt --hash $a.Sha256 --label "$($a.Tag) $($a.Arch)" --json 2>&1
    $line = ($out | Where-Object { $_ -match '^\s*\{' } | Select-Object -First 1)
    if (-not $line) {
        Note "lookup failed for $($a.Name): $out" 'Yellow'
        continue
    }
    $r = $line | ConvertFrom-Json
    $r | Add-Member -NotePropertyName Tag -NotePropertyValue $a.Tag -PassThru |
         Add-Member -NotePropertyName Arch -NotePropertyValue $a.Arch -PassThru |
         Add-Member -NotePropertyName Downloads -NotePropertyValue $a.Downloads -PassThru |
         Add-Member -NotePropertyName Name -NotePropertyValue $a.Name -PassThru
}
$rows = @($rows)
if (-not $rows) { Skip 'every VirusTotal lookup failed' }

if ($Json) { $rows | ConvertTo-Json -Depth 6; exit 0 }

# ---------------------------------------------------------------- report
$scanned = @($rows | Where-Object { $_.status -eq 'ok' })
Write-Host ''
Write-Host ('{0,-16} {1,-6} {2,7}  {3,-8} {4}' -f 'release', 'arch', 'flags', 'downloads', 'engines') -ForegroundColor Cyan
foreach ($r in $rows) {
    if ($r.status -ne 'ok') {
        Write-Host ('{0,-16} {1,-6} {2,7}  {3,-8} {4}' -f $r.Tag, $r.Arch, '-', $r.Downloads, 'never scanned')
        continue
    }
    $engines = ($r.flagged.PSObject.Properties | ForEach-Object { $_.Name }) -join ', '
    $colour = if ($r.tier1_signature) { 'Red' } elseif ($r.microsoft_verdict) { 'Yellow' } else { 'Gray' }
    Write-Host ('{0,-16} {1,-6} {2,7}  {3,-8} {4}' -f
        $r.Tag, $r.Arch, "$($r.malicious)/$($r.total)", $r.Downloads, $engines) -ForegroundColor $colour
}

# Per-architecture context. x64 and arm64 genuinely differ - most engines model x86-64 code far
# more deeply than ARM64 - so an x64 number is only meaningful against other x64 numbers.
Write-Host ''
foreach ($arch in @('x64', 'arm64')) {
    $set = @($scanned | Where-Object { $_.Arch -eq $arch })
    if (-not $set) { continue }
    $counts = @($set | ForEach-Object { $_.malicious } | Sort-Object)
    $median = $counts[[int][math]::Floor($counts.Count / 2)]
    Note ("{0,-5} band across {1} release(s): {2}-{3}, median {4}" -f
        $arch, $set.Count, $counts[0], $counts[-1], $median)
    foreach ($r in $set | Where-Object { $_.malicious -gt $median + 3 }) {
        Note ("  {0} is well above that band at {1} - worth a look at the permalink" -f
            $r.Tag, $r.malicious) 'Yellow'
    }
}

# The one that costs users something. Defender ships on by default, so its verdict is the one
# that turns into "I could not install this", and its name is the field the portal asks for.
$ms = @($scanned | Where-Object { $_.microsoft_verdict })
if ($ms) {
    Write-Host ''
    Note 'Microsoft currently flags these PUBLISHED installers:' 'Yellow'
    foreach ($r in $ms) {
        Note ("  {0,-14} {1,-6} {2}" -f $r.Tag, $r.Arch, $r.microsoft_verdict) 'Yellow'
        Note ("     sha256 {0}" -f $r.sha256)
    }
    Note 'Each is a ready-to-file false positive: https://www.microsoft.com/en-us/wdsi/filesubmission' 'Yellow'
    Note 'Submission type "Software developer", incorrectly detected = Yes. Fields and the' 'Yellow'
    Note 'notes paragraph are in docs/AV-SUBMISSION.md.' 'Yellow'
}

# A tier-1 signature match matters, but WHICH release it is on decides whether it should stop
# anything. The newest release is what users are being offered right now; an older one is a
# historical artifact nobody is downloading by default, and letting it fail forever would make
# this check the thing everyone passes with -Gate omitted. check-winget.ps1 draws the same line
# for the same reason: "a stale failing PR for a superseded version must never block the
# release that supersedes it".
$latestTag = $tags | Select-Object -First 1
$sig = @($scanned | Where-Object { $_.tier1_signature })
if ($sig) {
    Write-Host ''
    foreach ($r in $sig) {
        $where = if ($r.Tag -eq $latestTag) { 'CURRENT RELEASE' } else { 'superseded release' }
        Note ("SIGNATURE MATCH on published {0} {1} ({2}): {3}" -f
            $r.Tag, $r.Arch, $where, ($r.tier1_signature -join ', ')) 'Red'
        Note ("     {0}" -f $r.permalink)
    }
    Note 'A signature-led engine claims it recognises something specific rather than guessing.' 'Red'
    Note 'Check the permalink: a random-looking suffix on a generic family name (Trojan.Agent,' 'Red'
    Note 'GenKryptik) is usually an auto-generated cluster id rather than real analysis, but' 'Red'
    Note 'that judgement belongs to a person, which is why this prints instead of deciding.' 'Red'

    $current = @($sig | Where-Object { $_.Tag -eq $latestTag })
    if ($Gate -and $current) {
        Note "failing: the CURRENT release ($latestTag) carries a tier-1 signature match." 'Red'
        exit 1
    }
    if ($Gate) {
        Note "not failing: the current release ($latestTag) is clean of tier-1 signatures." 'Yellow'
    }
    exit 0
}

Write-Host ''
Note 'no tier-1 signature match on any published installer.'
Note 'Heuristic/ML detections above are this project''s standing background state as an'
Note 'unsigned installer with a new hash every release; see docs/AV-SUBMISSION.md before'
Note 'treating a number as news.'
exit 0
