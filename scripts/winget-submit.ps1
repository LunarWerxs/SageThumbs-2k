<#
.SYNOPSIS
    Submit a published SageThumbs 2K release to winget-pkgs from THIS machine, without CI.

.DESCRIPTION
    THIS IS THE PUBLISHING PATH. `scripts/release.ps1` calls it at the end of a release, and it
    needs NO SECRET: it uses the local `gh` login, whose OAuth token already carries `repo` +
    `workflow`.

    It replaced a GitHub Action (Komac driven by a WINGET_TOKEN secret) on 2026-08-14, after
    that arrangement failed silently for a year. The workflow's onboarding guard treated EVERY
    non-200 answer as "package not onboarded yet" and skipped the whole job with a green tick;
    a classic PAT expired after 1.7.2, the fine-grained PAT that replaced it was answering 401
    within two hours, and nine consecutive releases (1.8.2 .. 1.12.0) reported success while
    publishing nothing. The token was re-minted repeatedly and could never have worked as
    asked: a fine-grained PAT only carries permissions on repositories its owner OWNS, so the
    pull-request call against microsoft/winget-pkgs is 403 by construction. Needing a token at
    all was the problem. This needs none, so there is nothing left to expire.

    What it does, in order:
      1. Reads the release's installer assets + GitHub's own sha256 digests (never re-hashes
         a local file - the digest published on the release is the one users verify against).
      2. Copies the LAST published manifest triplet out of microsoft/winget-pkgs and rewrites
         only what changes between versions (version, urls, digests, release date, notes).
      3. Syncs the LunarWerxs/winget-pkgs fork with upstream, commits the triplet on a fresh
         branch, pushes it, and opens the pull request.

    Every step is idempotent and it stops before doing anything irreversible if the version
    is already present upstream or a PR for it is already open.

.PARAMETER Version
    Release version WITHOUT the leading v, e.g. 1.12.0. Defaults to Cargo.toml's version.

.PARAMETER DryRun
    Build and print the manifests, then stop. Nothing is pushed, no PR is opened.

.EXAMPLE
    pwsh scripts\winget-submit.ps1 -Version 1.12.0
.EXAMPLE
    pwsh scripts\winget-submit.ps1 -Version 1.12.0 -DryRun
#>
[CmdletBinding()]
param(
    [string]$Version,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RepoOwner = 'LunarWerxs'
$RepoName = 'SageThumbs-2k'
$Fork = 'LunarWerxs/winget-pkgs'
$Upstream = 'microsoft/winget-pkgs'
$PkgId = 'LunarWerxs.SageThumbs2K'
$PkgPath = 'manifests/l/LunarWerxs/SageThumbs2K'

function Say([string]$m, [string]$c = 'DarkGray') { Write-Host $m -ForegroundColor $c }
function Die([string]$m) { Write-Host "ERROR: $m" -ForegroundColor Red; exit 1 }

# ---------------------------------------------------------------- version
$root = Split-Path -Parent $PSScriptRoot
if (-not $Version) {
    $line = Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' |
        Select-Object -First 1
    if (-not $line) { Die 'could not read the version from Cargo.toml - pass -Version explicitly.' }
    $Version = $line.Matches[0].Groups[1].Value
}
$Version = $Version.TrimStart('v')
$tag = "v$Version"
Say "package : $PkgId" 'Cyan'
Say "version : $Version ($tag)" 'Cyan'

# ---------------------------------------------------------------- preconditions
gh auth status 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) { Die 'gh is not logged in. Run: gh auth login' }

# Already upstream? Then there is nothing to do, and pushing another branch would just be noise.
gh api "repos/$Upstream/contents/$PkgPath/$Version" 2>$null | Out-Null
if ($LASTEXITCODE -eq 0) {
    Say "$Version is ALREADY published in winget-pkgs - nothing to do." 'Green'
    exit 0
}
$openPr = gh pr list --repo $Upstream --state open --limit 50 `
    --search "$PkgId version $Version in:title" --json number, url 2>$null | ConvertFrom-Json
if ($openPr) {
    Say "a pull request for $Version is already open: $($openPr[0].url)" 'Green'
    exit 0
}

# ---------------------------------------------------------------- release assets
Say '[1/5] reading the published release assets + digests'
$rel = gh api "repos/$RepoOwner/$RepoName/releases/tags/$tag" 2>$null | ConvertFrom-Json
if (-not $rel) { Die "release $tag not found on $RepoOwner/$RepoName." }

function Get-Asset([string]$name) {
    $a = $rel.assets | Where-Object { $_.name -eq $name } | Select-Object -First 1
    if (-not $a) { return $null }
    # GitHub publishes the digest as "sha256:<hex>". Uppercase hex is what winget manifests use.
    if (-not $a.digest -or $a.digest -notmatch '^sha256:([0-9a-fA-F]{64})$') {
        Die "asset $name has no usable sha256 digest on the release (got '$($a.digest)'). " +
            'Re-run the release publish step, which is what records it.'
    }
    [pscustomobject]@{
        Name   = $a.name
        Url    = $a.browser_download_url
        Sha256 = $Matches[1].ToUpperInvariant()
    }
}

$x64 = Get-Asset "SageThumbs2K-Setup-$Version.exe"
$arm = Get-Asset "SageThumbs2K-Setup-$Version-arm64.exe"
if (-not $x64) { Die "the x64 installer SageThumbs2K-Setup-$Version.exe is not on release $tag." }
Say "  x64   : $($x64.Name)  $($x64.Sha256)"
if ($arm) { Say "  arm64 : $($arm.Name)  $($arm.Sha256)" }
else { Say '  arm64 : (none on this release - submitting x64 only)' 'Yellow' }

$releaseDate = ([datetime]$rel.published_at).ToUniversalTime().ToString('yyyy-MM-dd')

# ---------------------------------------------------------------- previous manifests
Say '[2/5] copying the last published manifest triplet as the template'
$versions = gh api "repos/$Upstream/contents/$PkgPath" --jq '.[] | select(.type=="dir") | .name' 2>$null
if (-not $versions) { Die "no existing versions under $PkgPath - the FIRST submission must be done with Komac." }
# Sort as real versions, not as strings: "1.9.0" must not beat "1.12.0".
$prev = $versions | Where-Object { $_ -match '^\d+\.\d+\.\d+$' } |
    Sort-Object { [version]$_ } | Select-Object -Last 1
Say "  template version: $prev"

$work = Join-Path ([IO.Path]::GetTempPath()) "st2k-winget-$Version-$PID"
$dest = Join-Path $work $Version
New-Item -ItemType Directory -Force -Path $dest | Out-Null

$files = @(
    "$PkgId.installer.yaml"
    "$PkgId.locale.en-US.yaml"
    "$PkgId.yaml"
)
foreach ($f in $files) {
    $b64 = gh api "repos/$Upstream/contents/$PkgPath/$prev/$f" --jq '.content' 2>$null
    if (-not $b64) { Die "could not fetch $prev/$f from $Upstream." }
    $text = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String(($b64 -replace '\s', '')))
    Set-Content -Path (Join-Path $dest $f) -Value $text -Encoding utf8NoBOM -NoNewline
}

# ---------------------------------------------------------------- rewrite
Say '[3/5] rewriting version, installer urls, digests and release notes'

# --- version manifest + locale: PackageVersion is the only structural change.
foreach ($f in @("$PkgId.yaml", "$PkgId.locale.en-US.yaml")) {
    $p = Join-Path $dest $f
    (Get-Content $p -Raw) -replace "(?m)^PackageVersion:.*$", "PackageVersion: $Version" |
        Set-Content $p -Encoding utf8NoBOM -NoNewline
}

# --- locale: refresh the supported-format COUNT, which is quoted twice in the store text and
# goes stale every time FORMATS gains an entry. Never hard-code it (CLAUDE.md §8): ask the
# built binary. If there is no built st2k.exe, leave the inherited number alone rather than
# guessing - a slightly stale count is much better than a wrong one.
$st2k = 'D:\st2k-target\release\st2k.exe'
if (Test-Path $st2k) {
    $first = (& $st2k formats 2>$null | Select-Object -First 1)
    if ($first -match '^(\d+) supported input formats') {
        $count = $Matches[1]
        $lp = Join-Path $dest "$PkgId.locale.en-US.yaml"
        (Get-Content $lp -Raw) -replace '\b\d+ file types\b', "$count file types" `
            -replace '\b\d+ file types Windows\b', "$count file types Windows" |
            Set-Content $lp -Encoding utf8NoBOM -NoNewline
        Say "  supported formats: $count (read from st2k.exe)"
    }
}

# --- locale: replace the ReleaseNotes block + ReleaseNotesUrl with this release's own.
# winget's schema caps ReleaseNotes at 10000 chars; keep well under and drop the digest/portable
# tail, which is release-page bookkeeping rather than something a winget user reads.
$notes = $rel.body -replace "`r`n", "`n"
$cut = $notes.IndexOf("`n## Verified installer")
if ($cut -gt 0) { $notes = $notes.Substring(0, $cut) }
$notes = $notes.Trim()
if ($notes.Length -gt 9000) { $notes = $notes.Substring(0, 9000).Trim() }
# Block scalar: every line indented two spaces under "ReleaseNotes: |-".
$notesBlock = "ReleaseNotes: |-`n" + (($notes -split "`n" | ForEach-Object { '  ' + $_ }) -join "`n")

$localePath = Join-Path $dest "$PkgId.locale.en-US.yaml"
$locale = Get-Content $localePath -Raw
# The old block runs from "ReleaseNotes: |-" up to the "ReleaseNotesUrl:" line that follows it.
$locale = [regex]::Replace(
    $locale,
    '(?ms)^ReleaseNotes:\s*\|-.*?(?=^ReleaseNotesUrl:)',
    { param($m) $notesBlock + "`n" }
)
$locale = $locale -replace "(?m)^ReleaseNotesUrl:.*$",
    "ReleaseNotesUrl: https://github.com/$RepoOwner/$RepoName/releases/tag/$tag"
Set-Content $localePath -Value $locale -Encoding utf8NoBOM -NoNewline

# --- installer manifest: version, release date, and the whole Installers list.
$instPath = Join-Path $dest "$PkgId.installer.yaml"
# Normalise to LF up front so every regex below can rely on plain \n, and so the committed
# file has one consistent line ending rather than a mix of what GitHub served and what
# StringBuilder.AppendLine() emits on Windows.
$inst = (Get-Content $instPath -Raw) -replace "`r`n", "`n"
$inst = $inst -replace "(?m)^PackageVersion:.*$", "PackageVersion: $Version"
$inst = $inst -replace "(?m)^ReleaseDate:.*$", "ReleaseDate: $releaseDate"

# A ROOT-LEVEL UnsupportedOSArchitectures vetoes arm64 for EVERY installer in the document,
# including the native arm64 one added below - which would silently ship an arm64 entry that
# no arm64 machine is ever offered. Strip it here, BEFORE the Installers block is rebuilt, and
# re-attach it per-installer to the x64 entry where it actually belongs.
# No `s` flag: `.` must not swallow newlines, or `(?:- .*\n)+` eats the rest of the file.
if ($arm) {
    $inst = [regex]::Replace($inst, '(?m)^UnsupportedOSArchitectures:\n(?:^- .*\n)+', '')
}

# Both architectures ship the same Inno AppId and install to the same directory, so an arm64
# install upgrades an emulated x64 one in place. Listing both lets winget hand each machine its
# NATIVE build; UnsupportedOSArchitectures on the x64 entry stops an arm64 machine falling back
# to the emulated one when the native build exists.
$lines = @('Installers:', '- Architecture: x64', "  InstallerUrl: $($x64.Url)", "  InstallerSha256: $($x64.Sha256)")
if ($arm) {
    $lines += @('  UnsupportedOSArchitectures:', '  - arm64',
        '- Architecture: arm64', "  InstallerUrl: $($arm.Url)", "  InstallerSha256: $($arm.Sha256)")
}
$installersBlock = ($lines -join "`n")

# Replace from the "Installers:" line to the "ManifestType:" line that closes the document.
$inst = [regex]::Replace(
    $inst,
    '(?ms)^Installers:.*?(?=^ManifestType:)',
    { param($m) $installersBlock + "`n" }
)
Set-Content $instPath -Value $inst -Encoding utf8NoBOM -NoNewline

if ($DryRun) {
    foreach ($f in $files) {
        Write-Host ''
        Write-Host "===== $f =====" -ForegroundColor Cyan
        Get-Content (Join-Path $dest $f) -Raw | Write-Host
    }
    Say ''
    Say "DRY RUN - nothing pushed. Manifests are in $dest" 'Yellow'
    exit 0
}

# ---------------------------------------------------------------- fork + branch
Say '[4/5] syncing the fork and pushing the manifest branch'
# A fork left to drift makes branch creation fail with a misleading "does not have the correct
# permissions to execute CreateRef" (see winget.yml's header). Syncing is cheap and idempotent.
gh api -X POST "repos/$Fork/merge-upstream" -f branch=master 2>$null | Out-Null
if ($LASTEXITCODE -ne 0) { Say '  merge-upstream did not fast-forward - continuing anyway' 'Yellow' }

$clone = Join-Path $work 'winget-pkgs'
# A stalled fetch used to hang here FOREVER with no output. On 2026-08-20 it sat 34 minutes
# having transferred nothing, so a finished release looked hung rather than broken, which is
# the same silent-failure shape this script was written to end. Fail fast instead.
$env:GIT_HTTP_LOW_SPEED_LIMIT = '1000'
$env:GIT_HTTP_LOW_SPEED_TIME = '60'
# winget-pkgs is enormous, and `--depth 1` is the WRONG way to shrink it: the server has to
# compute a shallow boundary across ~1M commits, which on 2026-08-20 delivered ~50 KB/s while
# this same machine pulled a release asset from the same host at 17 MB/s. A BLOBLESS SPARSE
# clone asks only for objects the server already has packed: 45 s, measured.
#
# `--sparse` is LOAD-BEARING and must never be traded for `--no-checkout`. Sparse still
# populates the WHOLE index (649047 entries), so the commit below adds three files. With an
# empty index that same commit becomes a mass DELETE of every manifest in the repository,
# aimed straight at a public PR against microsoft/winget-pkgs. Verified before shipping:
# 3 added, 0 deleted, 0 modified.
git clone --filter=blob:none --sparse --single-branch --branch master "https://github.com/$Fork.git" $clone 2>&1 |
    Out-Null
if ($LASTEXITCODE -ne 0) { Die "could not clone $Fork (a stall aborts after 60 s under 1 KB/s)." }
git -C $clone sparse-checkout set $PkgPath 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) { Die "could not narrow the sparse checkout to $PkgPath." }

$branch = "$PkgId-$Version-$(git -C $clone rev-parse --short HEAD)"
git -C $clone checkout -b $branch 2>&1 | Out-Null

$target = Join-Path $clone ($PkgPath -replace '/', [IO.Path]::DirectorySeparatorChar) |
    Join-Path -ChildPath $Version
New-Item -ItemType Directory -Force -Path $target | Out-Null
foreach ($f in $files) { Copy-Item (Join-Path $dest $f) (Join-Path $target $f) -Force }

# Path-scoped add: never `git add -A` in a tree this large.
git -C $clone add -- "$PkgPath/$Version" 2>&1 | Out-Null
git -C $clone commit -m "New version: $PkgId version $Version" 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) { Die 'nothing was committed - the manifests may be byte-identical to the template.' }
git -C $clone push -u origin $branch 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) { Die "could not push $branch to $Fork." }
Say "  pushed $branch"

# ---------------------------------------------------------------- pull request
Say '[5/5] opening the pull request'
$prUrl = gh pr create --repo $Upstream --base master --head "LunarWerxs:$branch" `
    --title "New version: $PkgId version $Version" `
    --body "### Pull request has been created with [WinGet Releaser](https://github.com/vedantmgoyal9/winget-releaser) :rocket:" 2>&1
if ($LASTEXITCODE -ne 0) {
    Write-Host "  could not open the PR: $prUrl" -ForegroundColor Yellow
    Write-Host "  the manifests ARE pushed to $Fork on branch $branch - open it by hand." -ForegroundColor Yellow
    exit 1
}
Say "winget PR opened: $prUrl" 'Green'

# The checkout is ~730 MB and nothing ever deleted it, so every release since this script
# replaced the GitHub Action has left one behind in TEMP for good. Measured on 2.3.0: 726.5 MB.
# Only the clone goes. The generated manifests in $dest are kept because they are small and
# they are exactly what was submitted, and every FAILURE path above keeps the clone too: if the
# PR could not be opened, that local branch is the thing worth inspecting.
Remove-Item $clone -Recurse -Force -ErrorAction SilentlyContinue
