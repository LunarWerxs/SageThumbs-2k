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

  It ALSO checks PLACEHOLDER parity (added 2026-08-27). A value can carry `{who}` / `{n}` /
  `{app}` substitution slots that are filled by NAME at runtime, and a translation that drops
  one ships a sentence with the account name, the file count or the error text simply missing -
  while a key check sees a present key and a build sees a valid file. Nothing caught that class
  before. Only keys that HAVE slots in en.toml are compared, and only in that direction, so a
  legitimate literal brace elsewhere cannot make this gate cry wolf.

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

function Read-LocalePlaceholders([string]$path) {
    # key -> the sorted, de-duplicated set of `{token}` substitution slots in its value,
    # joined into one comparable string. i18n::t hands the string to a formatter that fills
    # these by NAME, so a locale that drops one silently ships a sentence with the account
    # name / file count / error text simply missing, and one that invents a new one leaves
    # the literal braces on screen. KEY parity cannot see either: the key is present and the
    # file compiles.
    $map = @{}
    foreach ($line in [System.IO.File]::ReadAllLines($path)) {
        $m = [regex]::Match($line, '^([A-Za-z0-9_]+)\s*=\s*"(.*)"\s*$')
        if (-not $m.Success) { continue }
        $slots = [regex]::Matches($m.Groups[2].Value, '\{[A-Za-z0-9_]+\}') |
            ForEach-Object { $_.Value }
        $map[$m.Groups[1].Value] = (($slots | Sort-Object -Unique) -join ' ')
    }
    return $map
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

$enSlots = Read-LocalePlaceholders $enPath
$slotKeys = @($enSlots.Keys | Where-Object { $enSlots[$_] })

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

    # Placeholder parity, over the keys that HAVE placeholders in en.toml. A key en.toml has
    # no slots in is not checked in reverse on purpose: a translator adding braces to a
    # slot-free string is a typo the key check cannot see either, but flagging it here would
    # fire on any legitimate literal brace, and this gate must never cry wolf.
    $locSlots = Read-LocalePlaceholders $f.FullName
    foreach ($k in ($slotKeys | Sort-Object)) {
        if (-not $keys.Contains($k)) { continue }   # already reported as MISSING above
        $want = $enSlots[$k]
        $got  = $locSlots[$k]
        if ($want -ne $got) {
            $shown = if ($got) { $got } else { '(none)' }
            $fail.Add("$($f.Name): $k placeholder mismatch - en.toml has '$want', this has '$shown'")
        }
    }
}

if ($fail.Count) {
    Write-Host "[locale-keys] FAILED - $($fail.Count) problem(s) across the locales vs en.toml's $($enKeys.Count)-key set:" -ForegroundColor Red
    $fail | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host "[locale-keys] OK - $checked locale(s) match en.toml's $($enKeys.Count)-key set exactly, and all $($slotKeys.Count) placeholder-bearing key(s) agree." -ForegroundColor Green
