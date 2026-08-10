<#
  check-consistency.ps1 - guards the drift classes that have bitten releases:

   1. ASSET TRACKING - every file referenced by `include_bytes!`/`include_str!` (in src/)
      or an <img src=...> / ](...) in README/docs that points at assets/ MUST be git-tracked.
      (The 0.7.0 CI break + the preview4.png hero risk were both "referenced but not committed":
      it builds/renders locally because the file is on disk, then breaks on a clean checkout.)

   2. FORMAT COUNT - the count in the README shields badge and docs/FEATURES.md must match the
      number of entries in src/formats.rs `FORMATS`.

    3. VERSION - packaging/AppxManifest.xml must carry the Cargo.toml version (the MSIX version
      is a hand-written literal that has silently drifted before).
    4. LOCALES - every translation must have the same keys and placeholders as en.toml, with
      no duplicate keys or UTF-8 BOM.
    5. LIVE RELABEL - every Settings control built with translated text in settings_dlg/build.rs
      must have a matching (ID, key) row in settings_dlg/localize.rs, or it keeps its build-time
      language when the user switches language in the dialog (apply_labels only walks that table).

    6. TEXT FIT - the nav-rail labels and the page-header blurbs must MEASURE within the space
      they are drawn in. Nothing crashes when they don't: the blurb gets a DT_END_ELLIPSIS and
      the rail label is hard-clipped mid-word, in one language, on one page, which is precisely
      the class nobody notices until a user does. 1.9.0 introduced the settings-wide search box,
      which reserves the header's right 192px - that silently truncated 161 blurbs across 36
      languages, INCLUDING English's own Right-click menu line. Measured, not guessed.

  Exit 1 (with the offending items) on any mismatch; exit 0 when clean. Runs fast (no build) -
  wired into CI and called by release.ps1 before tagging. Run it locally before you push.
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$fail = New-Object System.Collections.Generic.List[string]

# Set of git-tracked paths (forward-slash, as git emits them).
$tracked = @{}
& git -C $root ls-files | ForEach-Object { $tracked[$_] = $true }

# --- 1) referenced assets must be git-tracked ---------------------------------
# include_bytes!/include_str! - path is relative to the .rs file that references it.
Get-ChildItem (Join-Path $root 'src') -Recurse -Filter *.rs | ForEach-Object {
  $dir = $_.DirectoryName; $name = $_.Name
  foreach ($m in [regex]::Matches((Get-Content $_.FullName -Raw), 'include_(?:bytes|str)!\(\s*"([^"]+)"')) {
    $abs = [System.IO.Path]::GetFullPath((Join-Path $dir $m.Groups[1].Value))
    $rel = $abs.Substring($root.Length + 1).Replace('\', '/')
    if (-not $tracked.ContainsKey($rel)) { $fail.Add("untracked asset (include_ in $name): $rel") }
  }
}
# README + docs/*.md image references that point at assets/.
$docs = @(Join-Path $root 'README.md')
$docs += (Get-ChildItem (Join-Path $root 'docs') -Filter *.md -EA SilentlyContinue).FullName
foreach ($doc in $docs) {
  if (-not (Test-Path $doc)) { continue }
  $leaf = Split-Path $doc -Leaf
  foreach ($m in [regex]::Matches((Get-Content $doc -Raw), '(?:src=|\]\()\s*"?(assets/[^")\s]+)')) {
    $rel = $m.Groups[1].Value.Replace('\', '/')
    if (-not $tracked.ContainsKey($rel)) { $fail.Add("untracked asset (img in ${leaf}): $rel") }
  }
}

# --- 2) format count: src/formats.rs FORMATS vs README badge + FEATURES --------
# FORMATS entries are ("ext", "Friendly name") tuples; the category sub-lists are bare
# &[&str] (no tuple) so this pattern counts FORMATS only. (Cross-checked == `st2k formats`.)
$count = ([regex]::Matches((Get-Content (Join-Path $root 'src\formats.rs') -Raw), '\(\s*"[A-Za-z0-9]+"\s*,\s*"')).Count
if ($count -lt 250) {
  $fail.Add("FORMATS count parse looks wrong ($count) - the regex in this script needs fixing")
}
else {
  $readme = Get-Content (Join-Path $root 'README.md') -Raw
  # The badge may spell out the exact count ("formats-316-") or use the non-numeric
  # "hundreds" convention (intentionally vague so the badge doesn't need to be bumped
  # every release) - either is accepted as long as FEATURES.md still has the real number.
  if ($readme -notmatch "formats-(?:$count-|hundreds-)") { $fail.Add("README shields badge count != FORMATS ($count) and isn't the 'hundreds' convention") }
  $featPath = Join-Path $root 'docs\FEATURES.md'
  if (Test-Path $featPath) {
    $feat = Get-Content $featPath -Raw
    if ($feat -notmatch "\*\*$count registered extensions") { $fail.Add("docs/FEATURES.md 'NNN registered extensions' != FORMATS ($count)") }
    if ($feat -notmatch "sum to \*\*$count\*\*") { $fail.Add("docs/FEATURES.md 'Counts sum to NNN' != FORMATS ($count)") }

    # PER-CATEGORY counts, e.g. "| **Image** (193) |". Only the TOTAL used to be checked,
    # so the per-category row drifted silently: it read 186 when the real Image count was
    # 187, and a +6 change propagated that off-by-one to 192 instead of 193. Nothing caught
    # it because 322 (the total) was right. The categories must ALSO sum to the total.
    $catNames = 'Image', 'Camera RAW', 'Ebook & comics', 'Document', 'Audio', 'Video', 'Archive'
    $catSum = 0
    $missing = @()
    foreach ($c in $catNames) {
      $m = [regex]::Match($feat, "\*\*$([regex]::Escape($c))\*\*\s*\((\d+)\)")
      if ($m.Success) { $catSum += [int]$m.Groups[1].Value } else { $missing += $c }
    }
    if ($missing.Count) {
      $fail.Add("docs/FEATURES.md missing a per-category count for: $($missing -join ', ')")
    }
    elseif ($catSum -ne $count) {
      $fail.Add("docs/FEATURES.md per-category counts sum to $catSum, but FORMATS has $count " +
                "(one of the category numbers is stale - `st2k formats` prints the live breakdown)")
    }
  }
}

# --- 3) version: Cargo.toml vs AppxManifest ------------------------------------
$ver = ([regex]::Match((Get-Content (Join-Path $root 'Cargo.toml') -Raw), '(?m)^\s*version\s*=\s*"([^"]+)"')).Groups[1].Value
$appx = Get-Content (Join-Path $root 'packaging\AppxManifest.xml') -Raw
if ($appx -notmatch [regex]::Escape("Version=`"$ver")) { $fail.Add("packaging/AppxManifest.xml Version != Cargo.toml ($ver)") }

# --- 4) locale parity: every locales/*.toml matches en.toml's key set -----------
# Two drift classes, both of which shipped:
#   * MISSING keys fall back to English at runtime (i18n::t), so nothing crashes and
#     nothing complains - v1.3.1 went out with 82 keys absent from ALL 35 non-English
#     locales, and a user reported a half-English Settings dialog (nav rail, page
#     header and "Reset order" in English inside a Portuguese UI).
#   * A DUPLICATE key is worse: build.rs's toml::from_str PANICS on one, so it breaks
#     the build outright (and stays latent until any locale edit forces a re-run).
# Placeholders are checked too: a dropped {dir}/{n} would break the runtime format.
$localeDir = Join-Path $root 'locales'
function Read-Locale([string]$path) {
  $map = @{}
  $dups = New-Object System.Collections.Generic.List[string]
  foreach ($line in [System.IO.File]::ReadAllLines($path)) {
    $m = [regex]::Match($line, '^([A-Za-z0-9_]+)\s*=\s*"(.*)"\s*$')
    if ($m.Success) {
      $k = $m.Groups[1].Value
      if ($map.ContainsKey($k)) { $dups.Add($k) } else { $map[$k] = $m.Groups[2].Value }
    }
  }
  return @{ Map = $map; Dups = $dups }
}
$phRe = '\{[a-z_]+\}'
$en = Read-Locale (Join-Path $localeDir 'en.toml')
if ($en.Dups.Count) { $fail.Add("locales/en.toml duplicate key(s): $(($en.Dups | Select-Object -First 5) -join ', ')") }
$localeCount = 0
foreach ($f in (Get-ChildItem $localeDir -Filter *.toml | Sort-Object Name)) {
  if ($f.Name -eq 'en.toml') { continue }
  $localeCount++
  $loc = Read-Locale $f.FullName
  if ($loc.Dups.Count) {
    $fail.Add("locales/$($f.Name) duplicate key(s) - build.rs will PANIC: $(($loc.Dups | Select-Object -First 5) -join ', ')")
  }
  $missing = @($en.Map.Keys | Where-Object { -not $loc.Map.ContainsKey($_) })
  if ($missing.Count) {
    $fail.Add("locales/$($f.Name) missing $($missing.Count) key(s) vs en.toml (silent English fallback): $(($missing | Sort-Object | Select-Object -First 5) -join ', ')")
  }
  $orphan = @($loc.Map.Keys | Where-Object { -not $en.Map.ContainsKey($_) })
  if ($orphan.Count) {
    $fail.Add("locales/$($f.Name) has $($orphan.Count) key(s) not in en.toml (dead strings): $(($orphan | Sort-Object | Select-Object -First 5) -join ', ')")
  }
  $phBad = @()
  foreach ($k in $en.Map.Keys) {
    if (-not $loc.Map.ContainsKey($k)) { continue }
    $a = (([regex]::Matches($en.Map[$k], $phRe)  | ForEach-Object { $_.Value }) | Sort-Object) -join ','
    $b = (([regex]::Matches($loc.Map[$k], $phRe) | ForEach-Object { $_.Value }) | Sort-Object) -join ','
    if ($a -ne $b) { $phBad += $k }
  }
  if ($phBad.Count) {
    $fail.Add("locales/$($f.Name) placeholder mismatch in $($phBad.Count) key(s): $(($phBad | Sort-Object | Select-Object -First 5) -join ', ')")
  }
  $head = [byte[]]::new(3)
  $fs = [System.IO.File]::OpenRead($f.FullName)
  try { $null = $fs.Read($head, 0, 3) } finally { $fs.Dispose() }
  if ($head[0] -eq 0xEF -and $head[1] -eq 0xBB -and $head[2] -eq 0xBF) {
    $fail.Add("locales/$($f.Name) has a UTF-8 BOM (the first key won't parse)")
  }
}

# --- 5) Settings dialog: translated control -> localize table row ---------------
# Sibling of check 4, and the same failure shape: nothing crashes, nothing warns. A control
# built with t("...") but absent from localize.rs's `pairs` table keeps its BUILD-TIME text
# forever - apply_labels (which on_lang_change calls) only re-texts ids listed in that table,
# so a live language switch leaves the control in the old language until Settings is closed
# and reopened. Seven of them had drifted this way (ID_PDF_MARGIN, ID_KEEP_METADATA,
# ID_PREVIEW_MD_REMOTE, ID_EDIT_UPLOAD_HOSTS, the two html-preview toggles) before this check.
$dlgDir   = Join-Path $root 'src\bin\app\settings_dlg'
$buildSrc = Get-Content (Join-Path $dlgDir 'build.rs') -Raw
$locSrc   = Get-Content (Join-Path $dlgDir 'localize.rs') -Raw

# Which id owns the visible translated string:
#   checkbox/button/header - the control's own id (their text IS the translation)
#   combo/edit             - the FIRST id, i.e. the separate ID_LBL_* static; the control
#                            itself holds user data, not a label, so it must NOT be relabeled.
$built = @{}
foreach ($m in [regex]::Matches($buildSrc, 'lc\.(checkbox|button|header|combo|edit|radio|link)\(\s*t\("([a-z0-9_]+)"\)(.*?)\)\s*;', 'Singleline')) {
  $kind = $m.Groups[1].Value
  $ids  = @([regex]::Matches($m.Groups[3].Value, '\b(ID_[A-Z0-9_]+|IDOK|IDCANCEL)\b') | ForEach-Object { $_.Groups[1].Value })
  if (-not $ids.Count) { continue }
  $built[$(if ($kind -in 'combo', 'edit') { $ids[0] } else { $ids[-1] })] = $m.Groups[2].Value
}
# button_row(&[(t("k"), ID), ...]) - the shared footer row.
foreach ($row in [regex]::Matches($buildSrc, 'lc\.button_row\(&\[(.*?)\]\s*\)', 'Singleline')) {
  foreach ($p in [regex]::Matches($row.Groups[1].Value, 't\("([a-z0-9_]+)"\)\s*,\s*(ID_[A-Z0-9_]+|IDOK|IDCANCEL)')) {
    $built[$p.Groups[2].Value] = $p.Groups[1].Value
  }
}
# Both the main `pairs` table and the #[cfg(feature = "html-preview")] slice beside it.
$locTable = @{}
foreach ($m in [regex]::Matches($locSrc, '\(\s*(ID_[A-Z0-9_]+|IDOK|IDCANCEL)\s*,\s*"([a-z0-9_]+)"\s*\)')) {
  $locTable[$m.Groups[1].Value] = $m.Groups[2].Value
}
if ($built.Count -lt 45 -or $locTable.Count -lt 45) {
  $fail.Add("settings_dlg parse looks wrong (built=$($built.Count), table=$($locTable.Count)) - the regex in this script needs fixing")
}
else {
  foreach ($id in ($built.Keys | Sort-Object)) {
    if (-not $locTable.ContainsKey($id)) {
      $fail.Add("settings_dlg: $id is built with t(`"$($built[$id])`") but has no row in localize.rs - it won't retranslate on a live language switch")
    }
    elseif ($locTable[$id] -ne $built[$id]) {
      $fail.Add("settings_dlg: $id key mismatch - build.rs uses '$($built[$id])', localize.rs uses '$($locTable[$id])'")
    }
  }
}

# --- 6) text fit: nav-rail labels + page-header blurbs ------------------------
# Budgets, both in 96-dpi design px, both read off navrail.rs rather than eyeballed:
#   blurb : the header is PANE_W + 4 = 532 wide, its text starts at +46, and it reserves the
#           right 192 for the floating search box -> 532 - 192 - 46 = 294. DT_END_ELLIPSIS.
#   rail  : NAV_W 188, text at +44, and the "you changed something here" dot sits at
#           right - 14 -> 188 - 44 - 18 = 126. No ellipsis flag at all, so it CLIPS mid-word.
# 9px of slack on the blurb so a font-metric wobble between machines cannot re-clip it.
# Measured in the fonts those two actually draw with: the system message font for the rail,
# and the same family at a 22px cell / weight 600 for the header title.
$fitSkipped = $false
try {
  Add-Type -AssemblyName System.Drawing -ErrorAction Stop
  Add-Type -AssemblyName System.Windows.Forms -ErrorAction Stop
} catch { $fitSkipped = $true }

if ($fitSkipped) {
  Write-Host "[consistency] note: text-fit check skipped (System.Drawing unavailable on this host)" -ForegroundColor Yellow
} else {
  $fitFont = [System.Drawing.SystemFonts]::MessageBoxFont
  $fitFlags = [System.Windows.Forms.TextFormatFlags]::NoPadding -bor [System.Windows.Forms.TextFormatFlags]::SingleLine
  $BLURB_MAX = 285
  $RAIL_MAX  = 126
  $SYNC_BTN_MAX    = 140
  $SYNC_STATUS_MAX = 348
  $navKeys = @('nav_general','nav_appearance','nav_filetypes','nav_ebook','nav_menu',
               'nav_screenshots','nav_quickaction','nav_advanced','nav_quickpreview','nav_databackup')
  $blurbKeys = $navKeys | ForEach-Object { $_ -replace '^nav_', 'blurb_' }
  $overflow = 0
  foreach ($f in (Get-ChildItem $localeDir -Filter *.toml | Sort-Object Name)) {
    $loc = Read-Locale $f.FullName
    foreach ($k in $blurbKeys) {
      if (-not $loc.Map.ContainsKey($k)) { continue }
      $w = [System.Windows.Forms.TextRenderer]::MeasureText($loc.Map[$k], $fitFont, [System.Drawing.Size]::new(10000, 200), $fitFlags).Width
      if ($w -gt $BLURB_MAX) {
        $overflow++
        $fail.Add("locales/$($f.Name) $k is ${w}px, over the ${BLURB_MAX}px header budget - it will render truncated with an ellipsis")
      }
    }
    foreach ($k in $navKeys) {
      if (-not $loc.Map.ContainsKey($k)) { continue }
      $w = [System.Windows.Forms.TextRenderer]::MeasureText($loc.Map[$k], $fitFont, [System.Drawing.Size]::new(10000, 200), $fitFlags).Width
      if ($w -gt $RAIL_MAX) {
        $overflow++
        $fail.Add("locales/$($f.Name) $k is ${w}px, over the ${RAIL_MAX}px nav-rail budget - it will be clipped mid-word")
      }
    }
    # The Data & Backup sync row: StatusBtn(status, button, 160) on a PANE_W of 528, so the
    # button gets 160 (less ~16 of padding) and the status fills PANE_W - 160 - 12 = 356.
    # A long translation runs the status line straight under the button, which is exactly how
    # the German one read before this was measured. The two placeholder-carrying status lines
    # are measured as templates: what {who} / {error} expand to is not ours to bound.
    foreach ($k in @('sync_btn_start','sync_btn_stop','sync_btn_disconnecting','sync_btn_signing_in')) {
      if (-not $loc.Map.ContainsKey($k)) { continue }
      $w = [System.Windows.Forms.TextRenderer]::MeasureText($loc.Map[$k], $fitFont, [System.Drawing.Size]::new(10000, 200), $fitFlags).Width
      if ($w -gt $SYNC_BTN_MAX) {
        $overflow++
        $fail.Add("locales/$($f.Name) $k is ${w}px, over the ${SYNC_BTN_MAX}px sync-button budget - the label will be cut off")
      }
    }
    foreach ($k in @('sync_state_off','sync_state_connecting','sync_state_synced','sync_state_synced_as','sync_state_updated','sync_state_pending','sync_state_pending_err')) {
      if (-not $loc.Map.ContainsKey($k)) { continue }
      $w = [System.Windows.Forms.TextRenderer]::MeasureText($loc.Map[$k], $fitFont, [System.Drawing.Size]::new(10000, 200), $fitFlags).Width
      if ($w -gt $SYNC_STATUS_MAX) {
        $overflow++
        $fail.Add("locales/$($f.Name) $k is ${w}px, over the ${SYNC_STATUS_MAX}px sync-status budget - it will run under the button")
      }
    }
  }
}

# --- report -------------------------------------------------------------------
if ($fail.Count) {
  Write-Host "[consistency] FAILED ($($fail.Count)):" -ForegroundColor Red
  $fail | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
  exit 1
}
$fitNote = if ($fitSkipped) { 'text fit NOT measured' } else { 'every nav label and blurb measures inside its box' }
Write-Host "[consistency] OK - assets tracked, format count = $count, version $ver consistent, $localeCount locales at $($en.Map.Count)-key parity, $($built.Count) Settings controls relabel live, $fitNote." -ForegroundColor Green
