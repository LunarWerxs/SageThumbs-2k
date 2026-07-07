<#
  release.ps1 - a GATED release: it never tags or publishes a commit until CI is GREEN
  on that exact commit. This is the fix for "tagged/released a broken commit" (the 0.7.0
  missing-asset incident): the tag is created by `gh release create` at the very end, only
  after `gh run watch` confirms CI passed.

  Prereqs: the version is already bumped in Cargo.toml and the release commit is on `main`
  (committed, not pushed). Run from anywhere:  pwsh scripts\release.ps1

  Flow:  consistency check  ->  clean-main guard  ->  push  ->  WAIT for CI green
         ->  build signed installer  ->  gh release create (creates the tag)  ->  winget auto-publishes.
#>
[CmdletBinding()]
param([switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Push-Location $root
try {
    $ver = ([regex]::Match((Get-Content "$root\Cargo.toml" -Raw), '(?m)^\s*version\s*=\s*"([^"]+)"')).Groups[1].Value
    if (-not $ver) { throw "could not read version from Cargo.toml" }
    $tag = "v$ver"
    Write-Host "== Releasing $tag ==" -ForegroundColor Cyan

    # 0) consistency: referenced assets tracked, format count + version aligned.
    Write-Host "[1/6] consistency check" -ForegroundColor Green
    pwsh "$root\scripts\check-consistency.ps1"; if ($LASTEXITCODE) { throw "consistency check failed - fix before releasing" }

    # 1) must be on main with a clean tree (so we release exactly what's committed).
    Write-Host "[2/6] clean-tree + branch guard" -ForegroundColor Green
    $branch = (git rev-parse --abbrev-ref HEAD).Trim()
    if ($branch -ne 'main') { throw "not on main (on '$branch') - release from main" }
    if (git status --porcelain) { throw "working tree is dirty - commit or stash before releasing" }

    # 2) refuse to clobber an existing tag (bump the version instead).
    if (git ls-remote --tags origin "refs/tags/$tag") { throw "$tag already exists on origin - bump the version in Cargo.toml" }

    # 3) push, then WAIT for CI to go GREEN on this exact commit before doing anything irreversible.
    $sha = (git rev-parse HEAD).Trim()
    Write-Host "[3/6] push main + wait for CI on $($sha.Substring(0,7))" -ForegroundColor Green
    git push origin main; if ($LASTEXITCODE) { throw "git push failed" }
    # Find the CI run for THIS exact commit. It usually registers in seconds, but under
    # Actions load (e.g. a prior push's run still queued) it can lag minutes — so poll for up
    # to 12 min (the old 6-min window aborted the 0.8.0 release when a prior run was busy).
    # `--limit 30` guards against the target being pushed past the default page of 20.
    $runId = $null
    for ($i = 0; $i -lt 120 -and -not $runId; $i++) {
        Start-Sleep -Seconds 6
        $runId = (gh run list --branch main --workflow CI --limit 30 --json headSha, databaseId `
                --jq "[.[] | select(.headSha==`"$sha`")][0].databaseId" 2>$null)
    }
    if (-not $runId) { throw "no CI run found for $sha after 12 min - check Actions" }
    # POLL the run to completion via `gh run view` (JSON). We deliberately do NOT use
    # `gh run watch`: it needs a live TTY and exits non-zero when run headless (from a
    # background / non-interactive shell), which aborts the release even though CI is fine
    # (this is exactly what broke the 0.7.1 release run).
    Write-Host "      run $runId found - waiting for it to finish..." -ForegroundColor Green
    $status = ''
    for ($i = 0; $i -lt 160 -and ($status -eq '' -or $status -eq 'queued' -or $status -eq 'in_progress'); $i++) {
        Start-Sleep -Seconds 15
        $status = (gh run view $runId --json status --jq .status 2>$null)
    }
    $concl = (gh run view $runId --json conclusion --jq .conclusion 2>$null)
    if ($concl -ne 'success') { throw "CI on $($sha.Substring(0,7)) finished '$concl' (not success) - NOT releasing. Fix + re-run." }
    Write-Host "      CI green." -ForegroundColor Green

    # 4) build the shippable signed installer (CI validates code; it doesn't build the installer).
    if (-not $SkipBuild) {
        Write-Host "[4/6] build installer" -ForegroundColor Green
        pwsh "$root\scripts\build-release.ps1"; if ($LASTEXITCODE) { throw "installer build failed" }
    }
    $setup = Get-ChildItem "$root\dist\SageThumbs2K-Setup-$ver.exe" -EA Stop

    # 5) create the GitHub release - this creates + pushes the tag, ONLY now that CI is green.
    Write-Host "[5/6] gh release create $tag" -ForegroundColor Green
    $notes = "$root\dist\RELEASE-NOTES-$tag.md"
    $notesArg = if (Test-Path $notes) { @('--notes-file', $notes) } else { @('--generate-notes') }
    gh release create $tag $setup.FullName --title "SageThumbs 2K $ver" --target main @notesArg
    if ($LASTEXITCODE) { throw "gh release create failed" }

    Write-Host "[6/6] DONE - $tag released. winget auto-publishes if the package is onboarded." -ForegroundColor Cyan
}
finally { Pop-Location }
