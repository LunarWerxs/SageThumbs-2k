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

# --- report -------------------------------------------------------------------
if ($fail.Count) {
  Write-Host "[consistency] FAILED ($($fail.Count)):" -ForegroundColor Red
  $fail | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
  exit 1
}
Write-Host "[consistency] OK - assets tracked, format count = $count, version $ver consistent." -ForegroundColor Green
