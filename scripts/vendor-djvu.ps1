<#
  vendor-djvu.ps1 - regenerate crates/vendor/djvu-rs from the pristine crates.io source plus
  crates/vendor/djvu-patches/djvu-rs.patch. Mirrors vendor-jxl.ps1's design (see its own header
  for the full rationale: the SOURCE OF TRUTH is the patch file, not the vendored copy, so a
  version bump or a lost hand-edit is always visible as "the patch no longer applies" rather
  than silently rotting).

      pwsh scripts\vendor-djvu.ps1                 # regenerate at the pinned version
      pwsh scripts\vendor-djvu.ps1 -Check          # verify the tree matches; changes nothing
      pwsh scripts\vendor-djvu.ps1 -Version 0.28.0 # try a new upstream release

  Sourced from cargo's own extracted registry cache (like vendor-jxl.ps1), not downloaded fresh
  (unlike vendor-exr.ps1): djvu-rs is, until this patch lands, an ordinary crates.io dependency,
  so a tree that has built once already carries its pristine source at
  `~/.cargo/registry/src/<index>/djvu-rs-<version>`. That is exactly what cargo compiled, so
  there is no chance of patching a different tarball than the one Cargo.lock resolved.

  DELETE ALL OF IT (this file, scripts/fetch-pristine-djvu.ps1, crates/vendor/djvu-patches,
  crates/vendor/djvu-rs, the `[patch.crates-io]` line and its `[workspace] exclude` entry) once
  an upstream djvu-rs release fixes `composite_rows_bilevel_one`'s fallback loop to bound the
  source column by the mask's own width - see crates/vendor/djvu-patches/README.md for exactly
  what the patch does and why.
#>
[CmdletBinding()]
param(
    [string]$Version = '0.27.0',
    # Verify only: regenerate into a temp directory and diff against the committed tree.
    [switch]$Check,
    # Refuse to skip. Without this, a `-Check` run that cannot find the pristine source prints
    # a loud SKIPPED and exits 0, which is right on a developer machine that has never built
    # (going red there says nothing about the tree) and WRONG in CI, where exit 0 is read as
    # "the vendored tree was compared and matches". See vendor-jxl.ps1's identical param for
    # the CI incident this guards against - the same shape bites here once the patch lands and
    # `[patch.crates-io]` stops cargo from ever downloading the plain crates.io tarball.
    [switch]$RequireReal
)
$ErrorActionPreference = 'Stop'

$root = Split-Path $PSScriptRoot -Parent
$patchPath = Join-Path $root 'crates\vendor\djvu-patches\djvu-rs.patch'
$name = 'djvu-rs'

if (-not (Get-Command git -ErrorAction SilentlyContinue)) { throw 'git is required (for git apply)' }
if (-not (Test-Path $patchPath)) { throw "patch not found: $patchPath" }

# Prefer the registry source directory that genuinely has this pinned crate/version on disk -
# a machine can carry more than one `...\registry\src\<dir>` (a stale one from an older cargo,
# a different index mirror, etc.), and vendor-jxl.ps1's own header documents picking the wrong
# one as a real, already-hit failure mode.
$registryCandidates = Get-ChildItem "$env:USERPROFILE\.cargo\registry\src" -Directory -ErrorAction SilentlyContinue
$registry = $registryCandidates | Where-Object { Test-Path (Join-Path $_.FullName "$name-$Version") } | Select-Object -First 1
if (-not $registry) { $registry = $registryCandidates | Select-Object -First 1 }
if (-not $registry -or -not (Test-Path (Join-Path $registry.FullName "$name-$Version"))) {
    if ($Check -and $RequireReal) {
        throw "$name $Version is not in this machine's cargo source cache, so the vendored tree was NOT compared. -RequireReal forbids reporting that as a pass. Run scripts\fetch-pristine-djvu.ps1 first."
    }
    if ($Check) {
        Write-Host "[vendor-djvu] SKIPPED - $name $Version is not in this machine's cargo source cache," -ForegroundColor Yellow
        Write-Host "              so the committed vendor tree was NOT compared against pristine + patch." -ForegroundColor Yellow
        Write-Host "              Run 'cargo fetch' or scripts\fetch-pristine-djvu.ps1 first." -ForegroundColor DarkGray
        exit 0
    }
    throw "pristine source not found for $name $Version - run 'cargo fetch' at the pinned version, or pass -Version <version>."
}
$src = Join-Path $registry.FullName "$name-$Version"

$dest = if ($Check) { Join-Path ([IO.Path]::GetTempPath()) ("djvu-vendor-check-" + [guid]::NewGuid().ToString('N')) } else { Join-Path $root 'crates\vendor' }
if ($Check) { New-Item -ItemType Directory -Force $dest | Out-Null }
$out = Join-Path $dest $name
Remove-Item -Recurse -Force $out -ErrorAction SilentlyContinue
Copy-Item -Recurse $src $out

Push-Location $out
try {
    # CAPTURE THE EXIT CODE BEFORE ANYTHING ELSE RUNS - see vendor-jxl.ps1's identical comment
    # for why (a pipelined git call loses $LASTEXITCODE to the pipeline instead of git, and
    # this script would then report "patch applied" for one that had not).
    $applyOut = & git apply --verbose -p2 $patchPath 2>&1
    $applyRc = $LASTEXITCODE
    $applyOut | ForEach-Object { Write-Verbose $_ }
    # `git apply` can exit 0 while having changed NOTHING: it prints "Skipped patch '<file>'"
    # when a patch carries git-style `diff --git a/... b/...` headers, because it then treats
    # the paths as repository-root-relative and, run from inside this subdirectory, finds
    # them outside it. A plain `diff -ruN pristine/... patched/...` header (the form the jxl
    # patches use, and the only form this script accepts) is taken relative to the current
    # directory and applies. Trusting the exit code alone would report success for a vendor
    # tree that was never patched, so "Skipped" is a hard failure here.
    if ($applyRc -ne 0 -or ($applyOut -join "`n") -match "Skipped patch") {
        Write-Host "[vendor-djvu] $name $Version - PATCH DID NOT APPLY" -ForegroundColor Red
        if (($applyOut -join "`n") -match "Skipped patch") {
            Write-Host "              git apply exited 0 but reported 'Skipped patch': the patch has git-style" -ForegroundColor Yellow
            Write-Host "              'diff --git a/ b/' headers, which git reads as repo-root paths. Rewrite the" -ForegroundColor Yellow
            Write-Host "              headers as 'diff -ruN pristine/djvu-rs/... patched/djvu-rs/...' (see jxl-patches)." -ForegroundColor Yellow
        } else {
            Write-Host "              The upstream source moved under the patch, or -Version is not 0.27.0." -ForegroundColor Yellow
            Write-Host "              Re-check the hunks: git apply -p2 --reject $patchPath" -ForegroundColor Yellow
            Write-Host "              ...then regenerate the patch from the fixed tree. Do NOT hand-edit the vendored copy." -ForegroundColor Yellow
        }
        if ($Check) { Remove-Item -Recurse -Force $dest -ErrorAction SilentlyContinue }
        exit 1
    }
} finally { Pop-Location }
Write-Host ("[vendor-djvu] {0}  {1}  patch applied" -f $name, $Version) -ForegroundColor Green

if ($Check) {
    $committed = Join-Path $root "crates\vendor\$name"
    $diff = & git diff --no-index --stat -- $committed $out 2>&1
    $drift = @()
    if ($LASTEXITCODE -ne 0 -and $diff) { $drift += $diff }

    # The diff above compares the WORKING TREE, and the working tree can hold files git will
    # never commit - vendor-jxl.ps1's own header names the exact incident this guards against
    # (a per-machine ignore rule hid an upstream `examples/` file from `git add`, so it sat on
    # disk satisfying that diff and never reached a commit; CI's checkout never had it). A file
    # under the vendored tree that is untracked OR ignored is drift CI will see and this
    # machine cannot, so fail here by name instead of passing by accident.
    $rel = "crates/vendor/$name"
    $untracked = @(& git -C $root ls-files --others --exclude-standard -- $rel 2>$null)
    $ignored = @(& git -C $root ls-files --others --ignored --exclude-standard -- $rel 2>$null)
    $uncommitted = @($untracked + $ignored | Where-Object { $_ } | Sort-Object -Unique)
    if ($uncommitted) {
        $drift += "$name`n  present on disk but NOT committed (CI's checkout will not have these):`n" +
            (($uncommitted | ForEach-Object { "    $_" }) -join "`n") +
            "`n  git add -f each one, or fix the ignore rule that is hiding it."
    }

    Remove-Item -Recurse -Force $dest -ErrorAction SilentlyContinue
    if ($drift) {
        Write-Host "[vendor-djvu] the committed vendor tree does NOT match pristine + patch:" -ForegroundColor Red
        $drift | ForEach-Object { Write-Host $_ }
        Write-Host "              Someone hand-edited the vendored copy, or the patch changed." -ForegroundColor Yellow
        Write-Host "              Re-run without -Check to regenerate, and fold real changes into the patch." -ForegroundColor Yellow
        exit 1
    }
    Write-Host "[vendor-djvu] OK - the committed vendor tree is exactly pristine + patch." -ForegroundColor Green
}
