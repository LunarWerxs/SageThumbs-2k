<#
  check-native-deps.ps1 - fails on any *-sys crate in the dependency graph that is not on the
  explicit allow-list below.

  WHY THIS EXISTS. Cargo.toml's own comments assert C-freedom as a load-bearing property in
  half a dozen places (sevenz-rust2, zip/flate2, djvu-rs, rars replacing UnRAR, and
  "webp-lossy... the ONLY C dependency in the build"). None of that was ever machine-checked:
  deny.toml's `[bans] deny = []` and its permissive-license allowlist both happily pass a
  newly-introduced C dependency, since a native -sys crate is almost always MIT/Apache
  licensed too. cargo-deny cannot express "ban anything matching *-sys" - it bans specific
  named crates, not a wildcard shape - so this is a small script instead of a deny.toml entry.

  A -sys crate is the near-universal Rust convention for "this links a native (usually C/C++)
  library" (the `sys` suffix is even documented Cargo convention: build.rs `links` key). Not
  every -sys crate compiles or bundles C - js-sys is pure-Rust wasm-bindgen glue, and
  webview2-com-sys is COM/windows-rs binding glue with no C toolchain requirement - which is
  exactly why this is an ALLOW-list of specific reviewed crates and their reason, not a bare
  "-sys is fine" rule: a future *-sys dependency (transitive or direct) that is NOT already on
  this list fails the build until someone reviews it and adds it here, deliberately.

  Reads Cargo.lock directly rather than shelling out to `cargo tree` - Cargo.lock already lists
  every resolved package (direct and transitive) with no need to invoke cargo, and this script
  is meant to run in the same places check-consistency.ps1 does (no build, sub-second).

.EXAMPLE
    pwsh scripts\check-native-deps.ps1
#>
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$lockPath = Join-Path $root 'Cargo.lock'

# name -> why it is safe despite the -sys suffix. Reviewed once; add a new entry only after
# confirming what it actually links (or that, like js-sys, it links nothing at all).
$allowed = @{
    'js-sys'             = 'pure-Rust wasm-bindgen glue (no native code, no C toolchain) - only reachable transitively, never compiled into a shipping target on Windows'
    'webview2-com-sys'   = 'WebView2 COM interface bindings via windows-rs, the same style as every other windows-rs crate already in this graph - no C/C++ compilation'
    'libwebp-sys'        = 'Cargo.toml documents this as the ONE deliberate, reviewed C dependency in the build (behind the webp-lossy feature) - see its own Cargo.toml comment'
}

if (-not (Test-Path -LiteralPath $lockPath)) {
    throw "Cargo.lock not found at $lockPath - run a cargo command once to generate it, then re-run this check"
}

$lock = Get-Content -LiteralPath $lockPath -Raw
$names = [regex]::Matches($lock, '(?m)^name = "([^"]+)"$') | ForEach-Object { $_.Groups[1].Value }
$sysCrates = $names | Where-Object { $_ -like '*-sys' } | Sort-Object -Unique

$unreviewed = @($sysCrates | Where-Object { -not $allowed.ContainsKey($_) })

if ($unreviewed) {
    Write-Host "[native-deps] FAILED - new native (*-sys) crate(s) not on the allow-list:" -ForegroundColor Red
    $unreviewed | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "`nA -sys crate almost always means a native C/C++ toolchain requirement and a larger" -ForegroundColor Red
    Write-Host "DLL surface. Confirm what it links (its build.rs, or its own README), then add it to" -ForegroundColor Red
    Write-Host "`$allowed in scripts\check-native-deps.ps1 with the reason, or remove the dependency" -ForegroundColor Red
    Write-Host "that pulled it in." -ForegroundColor Red
    exit 1
}

Write-Host "[native-deps] OK - $($sysCrates.Count) *-sys crate(s), all reviewed: $($sysCrates -join ', ')" -ForegroundColor Green
