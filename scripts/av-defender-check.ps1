<#
  av-defender-check.ps1 - scan built installers with the REAL local Windows Defender and
  decide, without a human, whether an AV false-positive submission is even warranted.

  Why this exists: VirusTotal's Microsoft engine runs without the cloud/reputation context a
  real Defender install has, so it reports an ML generic (Wacatac!ml / Wacapew!ml) on every
  unsigned low-prevalence Inno installer this project ships. docs/AV-SUBMISSION.md's own rule
  is that a submission is only meaningful once REAL Defender reports a threat name on a real
  machine - and the portal form requires that name. Every release therefore ended with a
  manual "go check Defender yourself" step whose answer has always been "clean, nothing to do".

  This runs that check. Clean -> says so and exits 0, nothing to submit. Flagged -> prints the
  filled-in submission fields (file, SHA-256, threat name) and exits 1, because that is the one
  case that actually needs a human at https://www.microsoft.com/en-us/wdsi/filesubmission.

  Usage:
    pwsh scripts\av-defender-check.ps1                 # every dist\SageThumbs2K-Setup-*.exe
    pwsh scripts\av-defender-check.ps1 -Path <file>    # one specific installer
#>
[CmdletBinding()]
param([string[]]$Path)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent

# Defender ships as a versioned platform directory; the newest one is the live engine.
# `Get-MpThreat`-style cmdlets are not used because they report the machine's threat HISTORY,
# not a verdict for one file. MpCmdRun -Scan -ScanType 3 is the per-file scan.
$platform = Join-Path $env:ProgramData 'Microsoft\Windows Defender\Platform'
$mp = if (Test-Path $platform) {
    Join-Path (Get-ChildItem $platform -Directory | Sort-Object Name -Descending | Select-Object -First 1).FullName 'MpCmdRun.exe'
} else { $null }
if (-not $mp -or -not (Test-Path $mp)) {
    # Absent scanner is a tooling gap, never a verdict - same rule as the VirusTotal gate.
    Write-Host "[av] SKIPPED - Windows Defender platform not found on this machine." -ForegroundColor Yellow
    exit 0
}

if (-not $Path) {
    $Path = @(Get-ChildItem (Join-Path $root 'dist') -Filter 'SageThumbs2K-Setup-*.exe' -EA SilentlyContinue |
        Where-Object { $_.Name -notmatch '\.release\.json$' } | ForEach-Object FullName)
}
if (-not $Path) { Write-Host "[av] no installers in dist\ - nothing to scan." -ForegroundColor Yellow; exit 0 }

$flagged = @()
foreach ($f in $Path) {
    $item = Get-Item -LiteralPath $f
    $out = (& $mp -Scan -ScanType 3 -File $item.FullName 2>&1 | Out-String)
    if ($out -match 'found no threats') {
        Write-Host ("[av] CLEAN   {0}" -f $item.Name) -ForegroundColor Green
    } else {
        # `-match` on the singular/plural forms Defender uses for a per-file scan hit.
        $name = ([regex]::Match($out, '(?m)^\s*Threat\s+(?:name:\s*)?(\S+)')).Groups[1].Value
        if (-not $name) { $name = '(see the output above)' }
        $sha = (Get-FileHash -LiteralPath $item.FullName -Algorithm SHA256).Hash
        Write-Host ("[av] FLAGGED {0}" -f $item.Name) -ForegroundColor Red
        $flagged += [pscustomobject]@{ Name = $item.Name; Sha = $sha; Threat = $name; Raw = $out }
    }
}

if (-not $flagged) {
    Write-Host "[av] local Defender reports every installer clean. NOTE: this does NOT rule out cloud-ML quarantines on end users (block-at-first-sight fires on low-reputation hashes at install time and never reproduces locally - see issue #12 / docs/AV-SUBMISSION.md). An end-user report WITH a Defender threat name is grounds to submit even when this prints CLEAN." -ForegroundColor Green
    Write-Host "[av] a VirusTotal '!ml' hit alone is NOT grounds to submit (docs/AV-SUBMISSION.md)."
    exit 0
}

Write-Host ""
Write-Host "[av] REAL Defender flagged an installer. THIS is the case worth submitting." -ForegroundColor Red
Write-Host "     https://www.microsoft.com/en-us/wdsi/filesubmission  ->  Software developer  ->  false positive: Yes"
foreach ($x in $flagged) {
    Write-Host ""
    Write-Host ("     file          : {0}" -f $x.Name)
    Write-Host ("     sha256        : {0}" -f $x.Sha)
    Write-Host ("     detection name: {0}" -f $x.Threat)
    Write-Host  "     notes         : SageThumbs 2K is a source-available Windows shell extension (thumbnail +"
    Write-Host  "                     context-menu provider) for image files. The Inno Setup installer registers"
    Write-Host  "                     its COM handlers and installs an optional self-updater. Source and releases:"
    Write-Host  "                     https://github.com/LunarWerxs/SageThumbs-2k"
    Write-Host ("     raw scan      :`n{0}" -f ($x.Raw.Trim() -replace '(?m)^', '                     '))
}
exit 1
