<#
.SYNOPSIS
  Local pre-push gate. Mirrors the GitHub CI `build-test` + `deny` jobs so a broken
  push never has to round-trip through CI to be caught.

  Wired up as a git pre-push hook (.git/hooks/pre-push), but also runnable by hand:
      pwsh scripts/preflight.ps1

  These checks are RELEASE builds on purpose: the tree ships `panic="abort"` + `lto`,
  and some failures (e.g. link-time "unresolved external symbol") only surface in a
  release link — a debug `cargo test` would NOT catch them.
#>
$ErrorActionPreference = 'Stop'
$failed = $false

function Step([string]$name, [scriptblock]$block) {
    Write-Host ""
    Write-Host "==> $name" -ForegroundColor Cyan
    & $block
    if ($LASTEXITCODE -ne 0) {
        Write-Host "FAILED: $name (exit $LASTEXITCODE)" -ForegroundColor Red
        $script:failed = $true
    }
}

# Mirror .github/workflows/ci.yml -> build-test job, in order.
Step 'build (release)'              { cargo build --release }
if (-not $failed) { Step 'clippy (-D warnings)' { cargo clippy --release --all-targets -- -D warnings } }
if (-not $failed) { Step 'unit tests (--lib)'   { cargo test  --release --lib } }
if (-not $failed) { Step 'integration tests'    { cargo test  --release --tests } }

# Mirror the `deny` job — only if cargo-deny is installed locally (config lives in .github/).
if (-not $failed -and (Get-Command cargo-deny -ErrorAction SilentlyContinue)) {
    Step 'cargo-deny' { cargo deny check --config .github/deny.toml }
}

Write-Host ""
if ($failed) {
    Write-Host "PREFLIGHT FAILED — push blocked. (bypass once with: git push --no-verify)" -ForegroundColor Red
    exit 1
}
Write-Host "PREFLIGHT PASSED — safe to push." -ForegroundColor Green
exit 0
