<#
  vendor-jxl.ps1 - regenerate crates/vendor/jxl-render and crates/vendor/jxl-oxide from the
  pristine crates.io sources plus our patches.

      pwsh scripts\vendor-jxl.ps1                 # regenerate at the pinned versions
      pwsh scripts\vendor-jxl.ps1 -Check          # verify the tree matches; changes nothing
      pwsh scripts\vendor-jxl.ps1 -Render 0.12.5 -Oxide 0.12.7   # try a new upstream release

  WHY THIS EXISTS, AND WHY IT IS NOT A PILE OF HAND EDITS. A 12 MP JPEG XL took ~2 s to
  thumbnail because a thumbnail cost a FULL-RESOLUTION decode: the cost is in pixels, not
  bytes, so no file-size gate can ever catch it, and JPEG XL exists precisely to make
  small-file-huge-image cheap. The fix is upstream-shaped - a VarDCT frame's LF image is a
  complete 8x-downsampled picture that the decoder already builds, dequantized and smoothed,
  before any HF coefficient parsing or inverse DCT - so we run it patched here and submitted
  it as tirr-c/jxl-oxide#505.

  Vendoring a dependency is a standing maintenance cost, and the way that cost usually turns
  into a disaster is one-off edits: someone bumps the version, re-extracts, and quietly loses
  changes nobody wrote down. So the SOURCE OF TRUTH IS THE PATCH FILES, not the vendored
  copies. Bumping a version is: change the default below (or pass -Render/-Oxide), run this,
  and either it applies cleanly or `git apply` names the exact hunks that no longer fit.

  The vendored copies are still COMMITTED, deliberately, because cargo needs the path
  dependencies present at build time and CI checks out the repo without running this script.
  They are build inputs; this script is how they are produced.

  DELETE ALL OF IT once an upstream release carries the feature: remove the two
  `[patch.crates-io]` lines, `crates/vendor/jxl-render`, `crates/vendor/jxl-oxide`,
  `crates/vendor/jxl-patches`, and this file.
#>
[CmdletBinding()]
param(
    # Pinned versions. These must match what Cargo.lock resolves for the UNPATCHED crates,
    # or the patch is being applied to a different codebase than the one that was tested.
    [string]$Render = '0.12.4',
    [string]$Oxide = '0.12.6',
    # Verify only: regenerate into a temp directory and diff against the committed tree.
    [switch]$Check
)
$ErrorActionPreference = 'Stop'

$root = Split-Path $PSScriptRoot -Parent
$patchDir = Join-Path $root 'crates\vendor\jxl-patches'
$crates = [ordered]@{ 'jxl-render' = $Render; 'jxl-oxide' = $Oxide }

if (-not (Get-Command git -ErrorAction SilentlyContinue)) { throw 'git is required (for git apply)' }

# The pristine sources: cargo's own extracted registry cache, which is already on disk for
# any tree that has built once. Preferred over downloading because it is exactly what cargo
# compiled, so there is no chance of patching a different tarball than the one in Cargo.lock.
$registry = Get-ChildItem "$env:USERPROFILE\.cargo\registry\src" -Directory -ErrorAction SilentlyContinue |
    Select-Object -First 1
if (-not $registry) {
    # -Check also runs in CI's consistency job, which does not build, so the extracted registry
    # cache may not exist there. Skip LOUDLY rather than going red for a reason that says
    # nothing about the tree; the guard that matters is the local one, before a push.
    if ($Check) {
        Write-Host "[vendor-jxl] SKIPPED - no cargo registry source cache on this machine, so the" -ForegroundColor Yellow
        Write-Host "             committed vendor tree was NOT compared against pristine + patches." -ForegroundColor Yellow
        Write-Host "             Run 'cargo fetch' first to make this check real." -ForegroundColor DarkGray
        exit 0
    }
    throw "no cargo registry source cache found - run 'cargo fetch' first"
}

$dest = if ($Check) { Join-Path ([IO.Path]::GetTempPath()) ("jxl-vendor-check-" + [guid]::NewGuid().ToString('N')) } else { Join-Path $root 'crates\vendor' }
if ($Check) { New-Item -ItemType Directory -Force $dest | Out-Null }

$failed = $false
foreach ($name in $crates.Keys) {
    $ver = $crates[$name]
    $src = Join-Path $registry.FullName "$name-$ver"
    if (-not (Test-Path $src)) {
        throw "pristine source not found: $src`n  Run 'cargo fetch' at the pinned version, or pass -$($name.Split('-')[1]) <version>."
    }
    $out = Join-Path $dest $name
    Remove-Item -Recurse -Force $out -ErrorAction SilentlyContinue
    Copy-Item -Recurse $src $out

    $patch = Join-Path $patchDir "$name.patch"
    if (-not (Test-Path $patch)) { throw "patch not found: $patch" }

    # `-p2` because the patches were generated as pristine/<crate>/... vs patched/<crate>/...,
    # so the first two path components are the diff's own scaffolding.
    Push-Location $out
    try {
        & git apply --verbose -p2 $patch 2>&1 | ForEach-Object { Write-Verbose $_ }
        if ($LASTEXITCODE -ne 0) {
            Write-Host "[vendor-jxl] $name $ver - PATCH DID NOT APPLY" -ForegroundColor Red
            Write-Host "             The upstream source moved under the patch. Re-check the hunks:" -ForegroundColor Yellow
            Write-Host "               git apply -p2 --reject $patch" -ForegroundColor Yellow
            Write-Host "             ...then regenerate the patch from the fixed tree. Do NOT hand-edit the vendored copy." -ForegroundColor Yellow
            $failed = $true
            continue
        }
    } finally { Pop-Location }
    Write-Host ("[vendor-jxl] {0,-11} {1}  patch applied" -f $name, $ver) -ForegroundColor Green
}
if ($failed) { exit 1 }

if ($Check) {
    $drift = @()
    foreach ($name in $crates.Keys) {
        $a = Join-Path $root "crates\vendor\$name"
        $b = Join-Path $dest $name
        $diff = & git diff --no-index --stat -- $a $b 2>&1
        if ($LASTEXITCODE -ne 0 -and $diff) { $drift += "$name`n$diff" }
    }
    Remove-Item -Recurse -Force $dest -ErrorAction SilentlyContinue
    if ($drift) {
        Write-Host "[vendor-jxl] the committed vendor tree does NOT match pristine + patches:" -ForegroundColor Red
        $drift | ForEach-Object { Write-Host $_ }
        Write-Host "             Someone hand-edited the vendored copy, or the patches changed." -ForegroundColor Yellow
        Write-Host "             Re-run without -Check to regenerate, and fold real changes into the patch." -ForegroundColor Yellow
        exit 1
    }
    Write-Host "[vendor-jxl] OK - the committed vendor tree is exactly pristine + patches." -ForegroundColor Green
}
