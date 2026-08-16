<#
.SYNOPSIS
    Every shared registry slot we CLAIM must be claimed non-destructively.

.DESCRIPTION
    Windows keeps exactly one handler CLSID per (file type, handler category) in a `shellex`
    slot. Those slots are SHARED with every other product on the machine, so writing into one
    silently evicts whoever held it — and unless we record what was there, the eviction is
    permanent: uninstall removes our value and leaves the slot empty rather than restoring it.

    That is not hypothetical. `hook_ext` (thumbnails) did exactly this for 300+ file types
    while its two siblings `hook_ext_preview` and `hook_ext_propstore` both guarded correctly.
    Nothing flagged the asymmetry, because nothing was looking: it is invisible in a diff, the
    build is green, the tests pass, and the damage only shows up on a stranger's machine that
    happened to have Icaros or a codec pack installed.

    So this checks the invariant directly instead of trusting review. For every function in
    register.rs that writes a CLSID into a shared slot, require ONE of:

      * a call to `remember_displaced` — record the incumbent, restore it on unhook; or
      * an ownership guard — read the slot first and return without writing if it is foreign.

    A fourth handler category added tomorrow that forgets both fails here, not in the wild.

.NOTES
    Deterministic source analysis, no build required. Exit 1 = a slot is claimed destructively.
#>
[CmdletBinding()]
param(
    [string]$RegisterPath = (Join-Path $PSScriptRoot '..\src\register.rs')
)

$ErrorActionPreference = 'Stop'
$fail = 0

if (-not (Test-Path $RegisterPath)) {
    Write-Host "[reg-symmetry] FAIL: cannot find $RegisterPath" -ForegroundColor Red
    exit 1
}
$lines = Get-Content $RegisterPath

# --- 0. Analyse PRODUCTION code only; stop at the test module. ---------------------------
# The invariant this enforces is about the user's real registry: claiming a slot another
# product holds evicts it permanently. A `#[cfg(test)]` function writing into a `Scratch`
# root under a throwaway key, with invented `zzz…` extensions, claims nothing from anyone
# and has no incumbent to record.
#
# Without this cut the check fires on exactly the test you most want to exist. On
# 2026-08-15 it failed `remove_user_if_ours_clears_our_stale_hook_but_leaves_a_foreign_one`,
# a test whose entire purpose is proving we DON'T clobber a foreign hook, and whose only
# writes are seeding the two fixtures it then asserts on. The suggested remedy would have
# been to call `remember_displaced()` inside a test that is not displacing anything, i.e.
# to distort the test until the linter was happy. A false red on a correct test is worse
# than no check at all: it teaches the next person that this one cries wolf.
$testMod = ($lines | Select-String -Pattern '^\s*#\[cfg\(test\)\]' | Select-Object -First 1)
if ($testMod) { $lines = $lines[0..($testMod.LineNumber - 2)] }

# --- 1. Find the helpers that BUILD shared-slot key paths. ------------------------------
# A shared slot is one keyed by a handler-category GUID: `…\shellex\{CATEGORY}` for the
# thumbnail/preview handlers, and the machine-wide PropertyHandlers list for the property
# handler. The helpers that format those paths are the definition of "shared" for this check,
# so a new category is picked up automatically as soon as its helper exists.
$slotHelpers = @()
for ($i = 0; $i -lt $lines.Count; $i++) {
    if ($lines[$i] -match '^\s*fn\s+(\w*keys)\s*\(') {
        $name = $Matches[1]
        # Look at the helper body (until the next top-level item) for a category-GUID path.
        $body = ''
        for ($j = $i; $j -lt $lines.Count -and -not ($j -gt $i -and $lines[$j] -match '^\}'); $j++) {
            $body += $lines[$j] + "`n"
        }
        if ($body -match 'shellex\\\\\{|PROPERTY_HANDLERS') {
            $slotHelpers += $name
        }
    }
}

if ($slotHelpers.Count -eq 0) {
    Write-Host '[reg-symmetry] FAIL: found no shared-slot key helpers — has register.rs been restructured?' -ForegroundColor Red
    Write-Host '                     This check cannot silently pass by matching nothing.' -ForegroundColor Red
    exit 1
}
Write-Host "[reg-symmetry] shared-slot key helpers: $($slotHelpers -join ', ')"

# --- 2. Split the file into top-level functions. ----------------------------------------
$funcs = @{}
$cur = $null
foreach ($line in $lines) {
    if ($line -match '^\s*(pub(\([\w:]+\))?\s+)?fn\s+(\w+)') {
        $cur = $Matches[3]
        $funcs[$cur] = New-Object System.Text.StringBuilder
    }
    if ($cur) { [void]$funcs[$cur].Append($line + "`n") }
    if ($line -match '^\}') { $cur = $null }
}

# --- 3. Every function that WRITES through a shared-slot helper must guard or record. ----
$writers = @()
foreach ($name in $funcs.Keys) {
    $body = $funcs[$name].ToString()
    if ($slotHelpers | Where-Object { $body -match "\b$_\s*\(" }) {
        if ($body -match 'set_string\s*\(') { $writers += $name }
    }
}

if ($writers.Count -eq 0) {
    Write-Host '[reg-symmetry] FAIL: found no functions writing through those helpers.' -ForegroundColor Red
    exit 1
}

foreach ($name in ($writers | Sort-Object)) {
    $body = $funcs[$name].ToString()
    $records = $body -match '\bremember_displaced(_in)?\s*\('
    # An ownership guard reads the slot's CURRENT value and then SKIPS the write when it
    # belongs to someone else. `return` is the shape used by the single-slot helpers;
    # `continue` is the shape used by the ones that loop over both key paths — both are a
    # genuine bail, and only accepting `return` would have failed a correctly-guarded
    # function (it did, on the first run of this check).
    $guards = ($body -match 'get_string\s*\(') -and ($body -match '\b(return|continue)\b')
    if ($records -or $guards) {
        $how = if ($records) { 'records the incumbent' } else { 'guards on the incumbent' }
        Write-Host ("  ok   {0,-24} {1}" -f $name, $how)
    }
    else {
        Write-Host ("  FAIL {0,-24} claims a shared slot with neither a guard nor a displacement record" -f $name) -ForegroundColor Red
        Write-Host "       -> call remember_displaced() before the write, or read the slot and bail if it is foreign." -ForegroundColor Red
        $fail = 1
    }
}

# --- 4. Anything we record must have a restore path. ------------------------------------
$all = ($lines -join "`n")
if ($all -match '\bremember_displaced\s*\(' -and $all -notmatch '\brestore_displaced\s*\(') {
    Write-Host '  FAIL displacements are recorded but never restored — unhook must put the slot back.' -ForegroundColor Red
    $fail = 1
}

if ($fail) {
    Write-Host '[reg-symmetry] FAILED' -ForegroundColor Red
    exit 1
}
Write-Host '[reg-symmetry] OK' -ForegroundColor Green
exit 0
