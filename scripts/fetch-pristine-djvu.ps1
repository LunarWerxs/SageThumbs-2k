<#
  fetch-pristine-djvu.ps1 - put the PRISTINE crates.io source for djvu-rs into cargo's registry
  cache, so `vendor-djvu.ps1 -Check` has something real to diff against.

      pwsh scripts\fetch-pristine-djvu.ps1                 # the pinned version
      pwsh scripts\fetch-pristine-djvu.ps1 -Version 0.28.0

  WHY THIS EXISTS. See fetch-pristine-jxl.ps1's header for the CI incident this mirrors: once
  the workspace's `[patch.crates-io]` redirects `djvu-rs` to the vendored PATH copy, cargo
  resolves it locally and never downloads the plain crates.io tarball, so a plain `cargo fetch`
  at the repo root cannot populate the registry cache `vendor-djvu.ps1 -Check` reads. Ask for
  the crate from OUTSIDE the workspace instead - a throwaway manifest in a temp directory - and
  cargo extracts it into the same shared `~/.cargo/registry/src/<index>/djvu-rs-<version>` that
  `vendor-djvu.ps1` reads, so the check then does a genuine comparison.

  It deliberately does NOT vendor, build, or write anything into this repository. Its whole
  effect is on the user-level cargo cache, which is why it is safe to run in CI and locally.
#>
[CmdletBinding()]
param(
    # Keep this in step with vendor-djvu.ps1's own default. Passing a version this repo does
    # not pin would populate the cache with a tarball the check is not looking for, which would
    # leave the check skipping and looking like this script had failed silently.
    [string]$Version = '0.27.0'
)
$ErrorActionPreference = 'Stop'

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { throw 'cargo is required' }

$scratch = Join-Path ([IO.Path]::GetTempPath()) ("djvu-pristine-fetch-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force $scratch | Out-Null
try {
    # A leaf package with no workspace of its own. `[workspace]` is empty on purpose: without
    # it, cargo walks UP from the temp directory and can attach this manifest to some other
    # workspace whose patch table would defeat the entire point.
    $manifest = @"
[package]
name = "djvu-pristine-fetch"
version = "0.0.0"
edition = "2021"

[dependencies]
djvu-rs = { version = "=$Version", default-features = false, features = ["std"] }

[workspace]
"@
    Set-Content -LiteralPath (Join-Path $scratch 'Cargo.toml') -Value $manifest -Encoding utf8
    New-Item -ItemType Directory -Force (Join-Path $scratch 'src') | Out-Null
    Set-Content -LiteralPath (Join-Path $scratch 'src\lib.rs') -Value '' -Encoding utf8

    Write-Host "[fetch-pristine-djvu] fetching djvu-rs $Version from crates.io" -ForegroundColor Cyan
    Push-Location $scratch
    try {
        # Download and extract only. No compilation, so this costs seconds and needs no
        # toolchain beyond cargo itself.
        & cargo fetch
        if ($LASTEXITCODE -ne 0) { throw "cargo fetch failed for the pristine djvu-rs source (exit $LASTEXITCODE)" }
    } finally {
        Pop-Location
    }
} finally {
    Remove-Item -Recurse -Force $scratch -ErrorAction SilentlyContinue
}

# Prove the point of the whole script rather than trusting that cargo did what was asked. If
# the extract landed somewhere this repo's checker does not read, saying so HERE is far better
# than letting the checker skip and report a pass.
$hit = Get-ChildItem "$env:USERPROFILE\.cargo\registry\src" -Directory -ErrorAction SilentlyContinue |
    ForEach-Object { Join-Path $_.FullName "djvu-rs-$Version" } |
    Where-Object { Test-Path $_ } |
    Select-Object -First 1
if (-not $hit) {
    throw "pristine source still absent after cargo fetch: djvu-rs $Version. vendor-djvu.ps1 -Check would skip, so this is a hard failure rather than a warning."
}
Write-Host "[fetch-pristine-djvu] OK - cached: djvu-rs $Version" -ForegroundColor Green
