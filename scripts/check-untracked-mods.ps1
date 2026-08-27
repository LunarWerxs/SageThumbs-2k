<#
  check-untracked-mods.ps1 - every `mod X;` must resolve to a file GIT IS TRACKING.

  This exists because the identical bug shipped twice in two days, from the identical shape of
  commit. In this repo it was `02d730a "chore: commit idle working-tree changes"`, which staged
  every MODIFIED tracked file and none of the NEW untracked ones, so `main` declared `mod nudge;`
  and `mod nudge_engine;` without shipping either file: three E0583s plus two knock-on E0277s.
  QuickDictate's `393c38c`, same title, did the same thing to its `main` on the same day.

  WHY NOTHING ELSE CATCHES IT, which is the whole point. Every other gate compiles the WORKING
  TREE, where the untracked files are sitting right there - so `cargo build`, `cargo test`, clippy
  and the entire verify ladder are green on the machine that made the commit. `git status` is
  clean too, because untracked is not modified. The tree is broken only for somebody who clones
  it, and the first honest signal is CI going red after the push. That is one round trip too late,
  and on a public repo it is a red X on the front page while you work out why.

  So the question this asks is NOT "does the file exist" - existence is exactly the thing that
  lies here. It asks "is the file in the index".

  A cross-repo version of the same rule lives at ~/.claude/tools/untracked-mods.py for sweeping
  the whole fleet (`--all`). This one is the GATE: PowerShell, so it adds no runtime to CI's
  consistency job, and it is the copy that must stay correct.

  Usage:
      pwsh -File scripts\check-untracked-mods.ps1
      pwsh -File scripts\check-untracked-mods.ps1 -ProveItFails   # self-test, changes nothing

  Exit 0 clean, 1 on findings.
#>
[CmdletBinding()]
param(
    # Verify the check can actually go red, by resolving against a synthetic tracked-file set
    # that is missing a module the tree really declares. A gate nobody has seen fail is a gate
    # nobody should trust; same convention as check-render-sanity.ps1.
    [switch] $ProveItFails
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent

# `mod foo;` with a SEMICOLON is a file module. `mod foo { ... }` is inline and owns no file -
# matching those would fire on every `#[cfg(test)] mod tests {` in the tree.
$modRe = [regex]'^\s*(?:pub\s*(?:\([^)]*\)\s*)?)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;'
$pathAttrRe = [regex]'^\s*#\[\s*path\s*=\s*"([^"]+)"\s*\]'

# Files whose child modules live in the SAME directory. `mod.rs` owns its directory; the rest are
# CRATE ROOTS, and a crate root's children sit beside it.
$rootBasenames = @('mod.rs', 'lib.rs', 'main.rs', 'build.rs')
# Cargo auto-discovers a crate root for every .rs directly inside these. `src/bin/cli.rs` is the
# reason this exists: the first cut of this check looked for its `mod vdec;` in `src/bin/cli/` and
# reported a FALSE RED, when the answer was `src/bin/vdec/mod.rs` sitting tracked in the same
# folder. A gate that cries wolf gets ignored, and then it misses the real thing.
$rootParents = @('bin', 'tests', 'benches', 'examples')

function Test-OwnsOwnDirectory([string]$rel) {
    $leaf = Split-Path $rel -Leaf
    if ($rootBasenames -contains $leaf) { return $true }
    $parent = Split-Path (Split-Path $rel -Parent) -Leaf
    return ($rootParents -contains $parent)
}

# Split-Path hands back Windows separators, and every path compared here is forward-slashed
# because that is what `git ls-files` emits. Mixing the two made the first run of this check
# report 158 FALSE findings - every module in the tree at once - because a half-Windows,
# half-Unix path matches nothing in the tracked set.
function Get-Dir([string]$rel) {
    return (Split-Path $rel -Parent).Replace([System.IO.Path]::DirectorySeparatorChar, '/')
}

Push-Location $root
try {
    $tracked = @(git ls-files) | ForEach-Object { $_.Replace([char]92, [char]47) }
    if (-not $tracked) { Write-Host '[untracked-mods] git ls-files returned nothing' -ForegroundColor Red; exit 1 }
    $trackedSet = [System.Collections.Generic.HashSet[string]]::new([string[]]$tracked, [System.StringComparer]::OrdinalIgnoreCase)

    # Vendored third-party trees are not ours to police and may use layouts we do not.
    $sources = @($tracked | Where-Object { $_ -like '*.rs' -and $_ -notlike '*/vendor/*' -and $_ -notlike 'vendor/*' })

    $dropped = $null
    if ($ProveItFails) {
        # Pick a module the tree really declares and pretend its file was never added.
        foreach ($rel in $sources) {
            foreach ($line in [System.IO.File]::ReadAllLines((Join-Path $root $rel))) {
                $m = $modRe.Match($line)
                if (-not $m.Success) { continue }
                $dir = Get-Dir $rel
                if (-not (Test-OwnsOwnDirectory $rel)) {
                    $stem = [System.IO.Path]::GetFileNameWithoutExtension($rel)
                    $dir = if ($dir) { "$dir/$stem" } else { $stem }
                }
                $cand = "$dir/$($m.Groups[1].Value).rs"
                if ($trackedSet.Contains($cand)) { $dropped = $cand; break }
            }
            if ($dropped) { break }
        }
        if (-not $dropped) { Write-Host '[untracked-mods] -ProveItFails found no module to drop' -ForegroundColor Red; exit 1 }
        [void]$trackedSet.Remove($dropped)
        Write-Host "[untracked-mods] self-test: pretending $dropped was never added" -ForegroundColor Yellow
    }

    $findings = New-Object System.Collections.Generic.List[string]
    foreach ($rel in $sources) {
        $lines = [System.IO.File]::ReadAllLines((Join-Path $root $rel))
        $pathAttr = $null
        for ($i = 0; $i -lt $lines.Count; $i++) {
            $line = $lines[$i]
            $attr = $pathAttrRe.Match($line)
            if ($attr.Success) { $pathAttr = $attr.Groups[1].Value; continue }
            if ($line -match '^\s*(//|/\*)') { continue }

            $m = $modRe.Match($line)
            if (-not $m.Success) {
                if ($line.Trim()) { $pathAttr = $null }
                continue
            }
            $name = $m.Groups[1].Value
            $dir = Get-Dir $rel
            $sameDir = Test-OwnsOwnDirectory $rel
            if (-not $sameDir) {
                $stem = [System.IO.Path]::GetFileNameWithoutExtension($rel)
                $dir = if ($dir) { "$dir/$stem" } else { $stem }
            }
            $prefix = if ($dir) { "$dir/" } else { '' }
            $candidates = if ($pathAttr) { @("$prefix$pathAttr") } else { @("$prefix$name.rs", "$prefix$name/mod.rs") }
            $pathAttr = $null

            $hit = $false
            foreach ($c in $candidates) { if ($trackedSet.Contains($c)) { $hit = $true; break } }
            if ($hit) { continue }

            $onDisk = @($candidates | Where-Object { Test-Path (Join-Path $root $_) })
            $state = if ($onDisk.Count) { 'PRESENT ON DISK BUT UNTRACKED' } else { 'MISSING ENTIRELY' }
            $findings.Add("$rel`:$($i + 1)  mod $name;  -> $state  (expected: $($candidates -join ' or '))")
        }
    }

    if ($findings.Count) {
        Write-Host "[untracked-mods] FAILED - $($findings.Count) module declaration(s) with no tracked file:" -ForegroundColor Red
        $findings | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
        Write-Host '  A fresh clone of this commit does not compile. These build for YOU only' -ForegroundColor Red
        Write-Host '  because they are sitting in your working tree. Fix: git add them.' -ForegroundColor Red
        if ($ProveItFails) { Write-Host '[untracked-mods] self-test PASSED - the check can go red.' -ForegroundColor Green; exit 0 }
        exit 1
    }

    if ($ProveItFails) {
        Write-Host '[untracked-mods] self-test FAILED - dropping a real module did NOT trip it.' -ForegroundColor Red
        exit 1
    }
    Write-Host "[untracked-mods] OK - every ``mod X;`` in $($sources.Count) tracked source file(s) resolves to a tracked file." -ForegroundColor Green
}
finally {
    Pop-Location
}
