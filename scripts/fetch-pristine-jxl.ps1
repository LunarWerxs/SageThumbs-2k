<#
  fetch-pristine-jxl.ps1 - put the PRISTINE crates.io sources for the two vendored JXL crates
  into cargo's registry cache, so `vendor-jxl.ps1 -Check` has something real to diff against.

      pwsh scripts\fetch-pristine-jxl.ps1                       # the pinned versions
      pwsh scripts\fetch-pristine-jxl.ps1 -Render 0.12.5 -Oxide 0.12.7

  WHY THIS EXISTS, AND WHY `cargo fetch` IN THE REPO CANNOT DO IT. The consistency job used to
  run `cargo fetch --locked` at the repo root and then `vendor-jxl.ps1 -Check`, with a comment
  saying the fetch was there to populate the registry cache the check reads. It never did.
  The workspace's own `[patch.crates-io]` redirects `jxl-render` and `jxl-oxide` to the
  vendored PATH copies, so cargo resolves them locally and never downloads the plain crates.io
  tarballs. The check therefore found no pristine source, printed a loud SKIPPED, and exited 0.
  Every green main run since the check was added reported a pass having compared nothing, which
  is the false-green shape: a guard that has never once fired looks exactly like a guard over a
  clean tree.

  The fix is to ask for the crates from OUTSIDE the workspace, where no patch table applies: a
  throwaway manifest in a temp directory that depends on the two crates at the pinned versions,
  and one `cargo fetch` against it. Cargo extracts them into the same shared
  `~/.cargo/registry/src/<index>/<name>-<version>` that `vendor-jxl.ps1` reads, so the check
  then does a genuine comparison.

  It deliberately does NOT vendor, build, or write anything into this repository. Its whole
  effect is on the user-level cargo cache, which is why it is safe to run in CI and locally.
#>
[CmdletBinding()]
param(
    # Keep these in step with vendor-jxl.ps1's own defaults. Passing a version this repo does
    # not pin would populate the cache with a tarball the check is not looking for, which
    # would leave the check skipping and looking like this script had failed silently.
    [string]$Render = '0.12.4',
    [string]$Oxide = '0.12.6'
)
$ErrorActionPreference = 'Stop'

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { throw 'cargo is required' }

$scratch = Join-Path ([IO.Path]::GetTempPath()) ("jxl-pristine-fetch-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force $scratch | Out-Null
try {
    # A leaf package with no workspace of its own. `[workspace]` is empty on purpose: without
    # it, cargo walks UP from the temp directory and can attach this manifest to some other
    # workspace whose patch table would defeat the entire point.
    $manifest = @"
[package]
name = "jxl-pristine-fetch"
version = "0.0.0"
edition = "2021"

[dependencies]
jxl-render = "=$Render"
jxl-oxide = "=$Oxide"

[workspace]
"@
    Set-Content -LiteralPath (Join-Path $scratch 'Cargo.toml') -Value $manifest -Encoding utf8
    New-Item -ItemType Directory -Force (Join-Path $scratch 'src') | Out-Null
    Set-Content -LiteralPath (Join-Path $scratch 'src\lib.rs') -Value '' -Encoding utf8

    Write-Host "[fetch-pristine-jxl] fetching jxl-render $Render and jxl-oxide $Oxide from crates.io" -ForegroundColor Cyan
    Push-Location $scratch
    try {
        # Download and extract only. No compilation, so this costs seconds and needs no
        # toolchain beyond cargo itself.
        & cargo fetch
        if ($LASTEXITCODE -ne 0) { throw "cargo fetch failed for the pristine JXL sources (exit $LASTEXITCODE)" }
    } finally {
        Pop-Location
    }
} finally {
    Remove-Item -Recurse -Force $scratch -ErrorAction SilentlyContinue
}

# Prove the point of the whole script rather than trusting that cargo did what was asked. If
# the extract landed somewhere this repo's checker does not read, saying so HERE is far better
# than letting the checker skip and report a pass.
$found = @()
$missing = @()
foreach ($pair in @(@('jxl-render', $Render), @('jxl-oxide', $Oxide))) {
    $name, $ver = $pair
    $hit = Get-ChildItem "$env:USERPROFILE\.cargo\registry\src" -Directory -ErrorAction SilentlyContinue |
        ForEach-Object { Join-Path $_.FullName "$name-$ver" } |
        Where-Object { Test-Path $_ } |
        Select-Object -First 1
    if ($hit) { $found += "$name $ver" } else { $missing += "$name $ver" }
}
if ($missing.Count -gt 0) {
    throw "pristine sources still absent after cargo fetch: $($missing -join ', '). vendor-jxl.ps1 -Check would skip, so this is a hard failure rather than a warning."
}
Write-Host "[fetch-pristine-jxl] OK - cached: $($found -join ', ')" -ForegroundColor Green
