<#
  check-locale-keys.ps1 - standalone locale KEY PARITY gate.

  build.rs already treats a DUPLICATE key inside one locale file as a hard build failure
  (toml::from_str panics on it), but a locale that is simply INCOMPLETE - missing keys vs.
  en.toml, or carrying orphaned keys en.toml doesn't have - compiles fine and just falls
  back to English at runtime (i18n::t). Nothing in the build catches that, so a half-
  translated locale ships silently. This script is the dedicated, no-build gate for
  exactly that: read locales/en.toml as the canonical key set, then every other
  locales/*.toml, and report per-locale MISSING (present in en.toml, absent here) and
  EXTRA (present here, absent from en.toml - a dead/typo'd string) keys.

  Parsed with a plain line reader, no TOML crate/module dependency: every locale file is
  flat `key = "value"` pairs, UTF-8 no-BOM, CRLF line endings (see locales/en.toml's own
  header comment) - a full parser would be overkill for that shape.

  Exit 1 (with the offending locales + keys) on any mismatch; exit 0 with a clean summary
  when every locale matches en.toml's key set exactly.
#>
$ErrorActionPreference = 'Stop'
$root      = Split-Path $PSScriptRoot -Parent
$localeDir = Join-Path $root 'assets/locales'

function Read-LocaleKeys([string]$path) {
    # Only flat `key = "value"` lines count; `#`-comments and blanks are skipped, which
    # matches the shape build.rs's toml::from_str expects for these files.
    $keys = New-Object System.Collections.Generic.HashSet[string]
    foreach ($line in [System.IO.File]::ReadAllLines($path)) {
        $m = [regex]::Match($line, '^([A-Za-z0-9_]+)\s*=\s*"')
        if ($m.Success) { [void]$keys.Add($m.Groups[1].Value) }
    }
    return $keys
}

$enPath = Join-Path $localeDir 'en.toml'
if (-not (Test-Path $enPath)) {
    Write-Host '[locale-keys] locales/en.toml not found' -ForegroundColor Red
    exit 1
}
$enKeys = Read-LocaleKeys $enPath
if ($enKeys.Count -eq 0) {
    Write-Host '[locale-keys] parsed 0 keys from en.toml - the regex in this script needs fixing' -ForegroundColor Red
    exit 1
}

$fail    = New-Object System.Collections.Generic.List[string]
$checked = 0
foreach ($f in (Get-ChildItem $localeDir -Filter *.toml | Sort-Object Name)) {
    if ($f.Name -eq 'en.toml') { continue }
    $checked++
    $keys    = Read-LocaleKeys $f.FullName
    $missing = @($enKeys.GetEnumerator() | Where-Object { -not $keys.Contains($_) }) | Sort-Object
    $extra   = @($keys.GetEnumerator()   | Where-Object { -not $enKeys.Contains($_) }) | Sort-Object
    if ($missing.Count) {
        $fail.Add("$($f.Name): missing $($missing.Count) key(s) vs en.toml: $($missing -join ', ')")
    }
    if ($extra.Count) {
        $fail.Add("$($f.Name): $($extra.Count) extra key(s) not in en.toml: $($extra -join ', ')")
    }
}

if ($fail.Count) {
    Write-Host "[locale-keys] FAILED - $($fail.Count) locale(s) diverge from en.toml's $($enKeys.Count)-key set:" -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host "[locale-keys] OK - $checked locale(s) match en.toml's $($enKeys.Count)-key set exactly." -ForegroundColor Green
