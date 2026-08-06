<#
  set-sourceforge-default.ps1 - point SourceForge's green Download button at the x64 installer.

  SourceForge stores "default download for <platform>" on the FILE, not on the project, and
  every release uploads new files. So a release starts with no default and SourceForge picks
  one by its own heuristic - which on v1.7.5 picked `SageThumbs2K-Setup-1.7.5-arm64.exe` and
  handed it to every Windows visitor, nearly all of whom cannot run it.

  This is deliberately IDEMPOTENT and SELF-VERIFYING: it reads the live state first, does
  nothing if it is already correct, and re-reads afterwards to prove the change took. A silent
  no-op would be worse than not running at all, because the failure is invisible from here -
  SourceForge tailors the button on the project page to each visitor's own platform, so it
  looks right to the maintainer while being wrong for everyone else.

  Usage:  pwsh scripts\set-sourceforge-default.ps1              # version from Cargo.toml
          pwsh scripts\set-sourceforge-default.ps1 -Version 1.7.5
          pwsh scripts\set-sourceforge-default.ps1 -CheckOnly   # report, change nothing

  The API key is a RELEASES API key, which is not the same thing as a password or an SSH key.
  Generate one at https://sourceforge.net/auth/ -> Account -> Services -> "Releases API Key",
  then put it in the repo-root `.env` (already gitignored, already holds the other keys):

      SOURCEFORGE_API_KEY=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
#>
[CmdletBinding()]
param(
    [string]$Version,
    [string]$Project = 'sagethumbs-2k',
    [switch]$CheckOnly,
    # Re-assert the default even when it already reads correctly. Exists to prove the API key
    # works WITHOUT waiting for a release to be wrong: re-setting the file that is already the
    # Windows default is a genuine authenticated write that changes nothing observable.
    [switch]$Force,
    # How long to keep retrying while SourceForge has not registered the upload yet (see the
    # loop below). 0 makes it a single attempt.
    [int]$WaitMinutes = 20
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent

if (-not $Version) {
    $Version = ([regex]::Match((Get-Content "$root\Cargo.toml" -Raw),
        '(?m)^\s*version\s*=\s*"([^"]+)"')).Groups[1].Value
    if (-not $Version) { throw 'could not read version from Cargo.toml' }
}
$wanted = "SageThumbs2K-Setup-$Version.exe"
$folder = "v$Version"

function Get-WindowsDefault {
    # `best_release.json` is what a visitor's download button resolves to, so it is the only
    # honest way to check this. Cache-bust: SourceForge serves it through a CDN and a stale
    # copy would make a failed change look successful.
    $url = "https://sourceforge.net/projects/$Project/best_release.json?cb=$([guid]::NewGuid())"
    try {
        $json = Invoke-RestMethod -Uri $url -Headers @{ 'Cache-Control' = 'no-cache' } -TimeoutSec 30
    } catch {
        Write-Host "  could not read best_release.json: $_" -ForegroundColor DarkYellow
        return $null
    }
    $win = $json.platform_releases.windows
    if (-not $win) { return $null }
    # `filename` is a full path on some responses and a bare name on others; compare the leaf.
    return (Split-Path -Leaf ([string]$win.filename))
}

Write-Host "SourceForge default download - $Project $Version" -ForegroundColor Cyan
$current = Get-WindowsDefault
Write-Host "  windows default is currently: $(if ($current) { $current } else { '(unknown)' })"
Write-Host "  it should be:                 $wanted"

if ($current -eq $wanted -and -not $Force) {
    Write-Host "  already correct - nothing to do." -ForegroundColor Green
    exit 0
}
if ($current -eq $wanted) {
    Write-Host "  already correct, but -Force given: re-asserting to exercise the API key." -ForegroundColor DarkGray
}
if ($CheckOnly) {
    Write-Host "  WRONG (check-only, not changing it)" -ForegroundColor Red
    exit 1
}

# The key lives in .env beside the other release keys. Read it here rather than requiring it
# in the environment, so this works the same way push_to_vt.py already does.
$key = $env:SOURCEFORGE_API_KEY
if (-not $key -and (Test-Path "$root\.env")) {
    $line = Get-Content "$root\.env" | Where-Object { $_ -match '^\s*SOURCEFORGE_API_KEY\s*=' } | Select-Object -First 1
    if ($line) { $key = ($line -split '=', 2)[1].Trim().Trim('"').Trim("'") }
}
if (-not $key) {
    Write-Host ''
    Write-Host 'No SOURCEFORGE_API_KEY found.' -ForegroundColor Red
    Write-Host '  1. https://sourceforge.net/auth/  ->  Account  ->  Services'
    Write-Host '  2. under "Releases API Key", click Generate, copy it'
    Write-Host "  3. add this line to $root\.env  (gitignored):"
    Write-Host '       SOURCEFORGE_API_KEY=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx'
    Write-Host '  then re-run this script. Until then, set it by hand:'
    Write-Host "       https://sourceforge.net/projects/$Project/files/$folder/"
    Write-Host '       (i) on the x64 installer  ->  Default Download For  ->  tick Windows'
    exit 1
}

$target = "https://sourceforge.net/projects/$Project/files/$folder/$wanted"
Write-Host "  PUT default=windows -> $target" -ForegroundColor DarkGray
# Send a PRE-ENCODED form body with an explicit content type. Handing Invoke-RestMethod a
# hashtable here looks equivalent and is not: on a PUT it does not produce the
# `application/x-www-form-urlencoded` payload SourceForge's release API expects, and the
# server answers "You are not authorized for this operation" - an auth error for what is
# actually a malformed request, which sends you hunting a perfectly good key. Verified
# 2026-08-06: identical key, identical URL, this shape returns 200 and the hashtable
# shape returns that lie.
$body = "default=windows&api_key=$([uri]::EscapeDataString($key))"

# RETRY, because SourceForge does not have the files at the moment the GitHub release goes
# public - that upload runs on its own schedule. On v1.8.0 the installer registered about six
# minutes after release.ps1 reached this step, and a PUT against a file SourceForge does not
# know about yet is rejected with "Value for arg failed verification regex" - which reads like
# a malformed request and is really just "too early". Without this wait the release ends with
# the button still pointing at the previous version until someone re-runs the script by hand,
# and removing that manual step is the entire reason this script exists.
$deadline = (Get-Date).AddMinutes($WaitMinutes)
$applied = $null
$failure = $null
for ($attempt = 1; ; $attempt++) {
    try {
        $response = Invoke-RestMethod -Method Put -Uri $target `
            -Headers @{ Accept = 'application/json' } `
            -ContentType 'application/x-www-form-urlencoded' `
            -Body $body -TimeoutSec 60
        # The response echoes the file's stored flags, so confirm the write landed on the
        # platform we asked for rather than trusting the status code alone.
        $applied = $response.result.x_sf.default
        break
    } catch {
        $failure = $_
        $code = try { [int]$_.Exception.Response.StatusCode } catch { 0 }
        # A rejected key will not start working in ten minutes, so don't sit here waiting on
        # one. Only the "not there yet" class is worth retrying.
        if ($code -eq 401 -or $code -eq 403) { break }
        if ((Get-Date) -ge $deadline) { break }
        Write-Host "  attempt ${attempt}: rejected, waiting for SourceForge to register the upload" -ForegroundColor DarkGray
        Start-Sleep -Seconds 30
    }
}
if (-not $applied) {
    Write-Host "  the API call failed: $failure" -ForegroundColor Red
    Write-Host '  (a 401/403 here means the key is wrong, belongs to an account without release' -ForegroundColor DarkYellow
    Write-Host '   permission on this project, or was sent in a body SourceForge could not read;' -ForegroundColor DarkYellow
    Write-Host "   anything else after $WaitMinutes min means the upload never showed up)" -ForegroundColor DarkYellow
    exit 1
}
Write-Host "  API reports default = $applied" -ForegroundColor DarkGray

# Prove it. SourceForge can take a moment to reflect the change, so give it a few tries rather
# than declaring success off the back of a 200 that may not have applied to the platform map.
foreach ($attempt in 1..5) {
    Start-Sleep -Seconds ([math]::Min(2 * $attempt, 8))
    $now = Get-WindowsDefault
    if ($now -eq $wanted) {
        Write-Host "  verified: windows default is now $now" -ForegroundColor Green
        exit 0
    }
    Write-Host "  attempt ${attempt}: still reads '$now'" -ForegroundColor DarkGray
}
Write-Host "  API reported success but best_release.json still says '$(Get-WindowsDefault)'." -ForegroundColor Red
Write-Host "  Set it by hand: https://sourceforge.net/projects/$Project/files/$folder/" -ForegroundColor Yellow
exit 1
