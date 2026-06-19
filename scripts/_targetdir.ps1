# Resolves the Cargo build target directory, in priority order:
#   1. $env:CARGO_TARGET_DIR (if set)
#   2. a `target-dir = "..."` redirect in .cargo/config.toml (if present)
#   3. the default ./target next to the workspace
# Callers append `release\` or `debug\` to the returned path. Resolution is
# always relative to this script's own location, so it works from any caller.
$cfg = Join-Path $PSScriptRoot '..\.cargo\config.toml'
if ($env:CARGO_TARGET_DIR) {
    $env:CARGO_TARGET_DIR
} elseif (Test-Path $cfg) {
    $m = [regex]::Match((Get-Content $cfg -Raw), 'target-dir\s*=\s*"([^"]+)"')
    if ($m.Success) { $m.Groups[1].Value } else { Join-Path $PSScriptRoot '..\target' }
} else {
    Join-Path $PSScriptRoot '..\target'
}
