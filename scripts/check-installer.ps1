<#
  check-installer.ps1 - static lint for scripts\packaging\installer.iss [Code].

  Catches uninstaller-only RUNTIME bugs that ISCC compiles happily (so a green build
  hides them) and that our dev loop never exercises - the dev uninstall (install.ps1
  -Uninstall) calls regsvr32 directly and NEVER runs Inno's unins000.exe, and every
  automated uninstall is silent (which skips the interactive-only survey). That blind
  spot shipped issue #3: the uninstall survey created its dialog with

      F := TSetupForm.Create(nil);

  TForm.Create loads a DFM form resource named after the class. Inno links that
  resource into Setup.exe (the wizard uses it) but NOT into the stripped-down
  uninstaller binary, so in unins000.exe it throws a FATAL

      Runtime error: Resource TSetupForm not found.

  and aborts the whole uninstall. Custom forms must instead be built with
  CreateCustomForm(w, h, keepX, keepY) - it uses CreateNew (no resource lookup) and
  works in Setup AND the uninstaller. This lint fails the release build if the banned
  constructor reappears in installer.iss.

  A full headless install/uninstall smoke test was considered and deliberately NOT used:
  the real uninstall [Code] has side effects on the machine it runs on (it clears the app's
  own HKCU settings), so running it as a build step on a dev box is undesirable. This static
  rule is deterministic, side-effect-free, and drift-free (it reads the real installer.iss
  every run).

  Run by build-release.ps1 before the ISCC compile; also runnable standalone:
      pwsh scripts\check-installer.ps1
#>
[CmdletBinding()]
param(
    [string]$IssPath = (Join-Path (Split-Path $PSScriptRoot -Parent) 'scripts\packaging\installer.iss'),

    # When build-release supplies the staged ImageMagick directory, prove that
    # every top-level managed payload entry is covered by the exact cleanup
    # allowlist. This catches a new pinned-runtime filename before an upgrade can
    # leave the previous copy behind.
    [string]$ManagedPayloadPath,

    # Always present in release staging, including engine-less CI builds. When a full
    # bundle also exists, the two policy copies must be byte-identical.
    [string]$CorePolicyPath
)
$ErrorActionPreference = 'Stop'
if (-not (Test-Path -LiteralPath $IssPath)) { throw "installer.iss not found at $IssPath" }

$lines = Get-Content -LiteralPath $IssPath
$violations = New-Object System.Collections.Generic.List[string]

for ($i = 0; $i -lt $lines.Count; $i++) {
    # Strip Pascal comments so our own explanatory notes (which legitimately name the
    # banned pattern) don't trip the check: // to end-of-line, and { ... } inline blocks.
    $code = $lines[$i] -replace '//.*$', '' -replace '\{[^}]*\}', ''
    if ($code -match 'TSetupForm\s*\.\s*Create\s*\(') {
        $violations.Add("  installer.iss:$($i + 1): $($lines[$i].Trim())")
    }
}

# --- Unescaped braces in a command a directive hands to a shell -------------------------
#
# Inno reads '{' as the start of one of ITS OWN constants, so a PowerShell block written into
# a Parameters: value - `catch{...}`, `if(...){...}` - is read as a constant named "catch" and
# ISCC ABORTS with "Unknown constant". '{{' is how Inno spells a literal brace; a closing '}'
# needs no escape.
#
# This is why the rule exists rather than being theoretical: the 2026-08-22 rewrite of the
# modern-menu registration (which made upgrades 2x faster) shipped with bare PowerShell braces
# and broke the release build outright. NOTHING caught it until a full ISCC compile four
# minutes into build-release.ps1, because every other check reads the file as text and the
# braces are perfectly valid text. This one reads it as Inno does.
#
# The allowlist is Inno's documented constant set as this installer uses it. A NEW legitimate
# constant will fail here; that is intended, and the message says to add it. Failing on an
# unknown name is the only version of this check that cannot be fooled by a PowerShell block
# whose first word happens to look like a constant.
$innoConstants = @(
    'app', 'autopf', 'autopf32', 'autopf64', 'code', 'commonappdata', 'commondesktop',
    'commondocs', 'commonpf', 'commonprograms', 'commonstartmenu', 'dao', 'dotnet40',
    'drive', 'fonts', 'group', 'hwnd', 'ini', 'language', 'localappdata', 'log', 'olddata',
    'param', 'reg', 'sd', 'src', 'srcexe', 'sys', 'sysnative', 'syswow64', 'tmp',
    'uninstallexe', 'userappdata', 'userdesktop', 'userdocs', 'userprograms', 'usersendto',
    'userstartmenu', 'win', 'wizardhwnd'
)
for ($i = 0; $i -lt $lines.Count; $i++) {
    $line = $lines[$i]
    # An .iss comment may legitimately DISCUSS a brace (the note beside the registration line
    # does exactly that), so comments are not code.
    if ($line.TrimStart().StartsWith(';')) { continue }
    # Only directives whose value is handed to a shell or the filesystem. Restricting the scan
    # keeps the rule away from [Code], [Messages] and everything else that uses braces for its
    # own reasons.
    if ($line -notmatch '(?i)(^|\s)(Filename|Parameters|WorkingDir|StatusMsg|IconFilename)\s*:') {
        continue
    }
    $idx = 0
    while ($idx -lt $line.Length) {
        $open = $line.IndexOf('{', $idx)
        if ($open -lt 0) { break }
        if ($open + 1 -lt $line.Length -and $line[$open + 1] -eq '{') {
            $idx = $open + 2   # an escaped literal brace, which is the correct spelling
            continue
        }
        $close = $line.IndexOf('}', $open + 1)
        $inner = if ($close -gt $open) {
            $line.Substring($open + 1, $close - $open - 1)
        } else {
            $line.Substring($open + 1)
        }
        # '#' is a preprocessor value, '%' an environment variable, '\' a path separator
        # constant - all legitimate opens that are not names.
        $isSpecial = $inner.StartsWith('#') -or $inner.StartsWith('%') -or $inner.StartsWith('\')
        $name = ($inner -split ':', 2)[0]
        if (-not $isSpecial -and $innoConstants -notcontains $name.ToLowerInvariant()) {
            $shown = if ($name.Length -gt 34) { $name.Substring(0, 34) + '...' } else { $name }
            $violations.Add(
                "  installer.iss:$($i + 1): '{$shown' is not an Inno constant. If it is a " +
                "literal brace (a PowerShell block, say), write '{{'; ISCC otherwise aborts " +
                "with `"Unknown constant`". If it IS a constant, add it to `$innoConstants " +
                "in check-installer.ps1."
            )
        }
        $idx = $open + 1
    }
}

$requiredUpgradeCleanup = @(
    'Type: filesandordirs; Name: "{app}\modules"',
    'Type: files; Name: "{app}\magick.exe"',
    'Type: files; Name: "{app}\CORE_RL_*.dll"',
    'Type: files; Name: "{app}\mfc140u.dll"',
    'Type: files; Name: "{app}\msvcp140*.dll"',
    'Type: files; Name: "{app}\vcomp140.dll"',
    'Type: files; Name: "{app}\vcruntime140*.dll"',
    'Type: files; Name: "{app}\colors.xml"',
    'Type: files; Name: "{app}\configure.xml"',
    'Type: files; Name: "{app}\delegates.xml"',
    'Type: files; Name: "{app}\english.xml"',
    'Type: files; Name: "{app}\locale.xml"',
    'Type: files; Name: "{app}\log.xml"',
    'Type: files; Name: "{app}\mime.xml"',
    'Type: files; Name: "{app}\policy.xml"',
    'Type: files; Name: "{app}\thresholds.xml"',
    'Type: files; Name: "{app}\type-ghostscript.xml"',
    'Type: files; Name: "{app}\type.xml"',
    'Type: files; Name: "{app}\License.txt"',
    'Type: files; Name: "{app}\NOTICE.txt"'
)

$installDeleteHeaders = @(
    for ($i = 0; $i -lt $lines.Count; $i++) {
        if ($lines[$i] -match '^\s*\[InstallDelete\]\s*$') { $i }
    }
)
$actualUpgradeCleanup = [System.Collections.Generic.List[string]]::new()
if ($installDeleteHeaders.Count -ne 1) {
    $violations.Add(
        "  installer.iss: expected exactly one [InstallDelete] section; found $($installDeleteHeaders.Count)"
    )
} else {
    for ($i = $installDeleteHeaders[0] + 1; $i -lt $lines.Count; $i++) {
        $line = $lines[$i].Trim()
        if ($line -match '^\s*\[[^\]]+\]\s*$') { break }
        if (-not $line -or $line.StartsWith(';', [StringComparison]::Ordinal)) { continue }
        $actualUpgradeCleanup.Add($line)
    }
}

foreach ($entry in $requiredUpgradeCleanup) {
    $count = @($actualUpgradeCleanup | Where-Object { $_ -ceq $entry }).Count
    if ($count -eq 0) {
        $violations.Add("  installer.iss: missing managed upgrade cleanup entry: $entry")
    } elseif ($count -gt 1) {
        $violations.Add("  installer.iss: duplicate managed upgrade cleanup entry: $entry")
    }
}
$requiredSet = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
foreach ($entry in $requiredUpgradeCleanup) {
    $null = $requiredSet.Add($entry)
}
foreach ($entry in $actualUpgradeCleanup) {
    if (-not $requiredSet.Contains($entry)) {
        $violations.Add(
            "  installer.iss: unmanaged or overly broad [InstallDelete] entry is forbidden: $entry"
        )
    }
}

$filesHeaders = @(
    for ($i = 0; $i -lt $lines.Count; $i++) {
        if ($lines[$i] -match '^\s*\[Files\]\s*$') { $i }
    }
)
$actualFileEntries = [System.Collections.Generic.List[string]]::new()
if ($filesHeaders.Count -ne 1) {
    $violations.Add(
        "  installer.iss: expected exactly one [Files] section; found $($filesHeaders.Count)"
    )
} else {
    for ($i = $filesHeaders[0] + 1; $i -lt $lines.Count; $i++) {
        $line = $lines[$i].Trim()
        if ($line -match '^\s*\[[^\]]+\]\s*$') { break }
        if (-not $line -or $line.StartsWith(';', [StringComparison]::Ordinal)) { continue }
        $actualFileEntries.Add($line)
    }
}

# policy.xml ships unconditionally, ahead of the engine it constrains: the decoder can fall
# back to a separately-installed system ImageMagick, which must be constrained too. The
# bundled Magick row must exclude its duplicate staged copy so Inno has one unambiguous
# source for the installed policy. (Both rows lost their `Components:` clause when the
# Full/Compact selection was removed — there are no components any more.)
$corePolicyEntry =
    'Source: "{#StageDir}\policy.xml"; DestDir: "{app}"; Flags: ignoreversion'
$magickPayloadEntry =
    'Source: "{#StageDir}\magick\*"; DestDir: "{app}"; Excludes: "policy.xml"; Flags: ignoreversion recursesubdirs createallsubdirs'
if (@($actualFileEntries | Where-Object { $_ -ceq $corePolicyEntry }).Count -ne 1) {
    $violations.Add(
        "  installer.iss: hardened policy must be installed exactly once as core: $corePolicyEntry"
    )
}
$magickRows = @(
    $actualFileEntries |
        Where-Object { $_.StartsWith('Source: "{#StageDir}\magick\*"', [StringComparison]::Ordinal) }
)
if ($magickRows.Count -ne 1 -or $magickRows[0] -cne $magickPayloadEntry) {
    $violations.Add(
        "  installer.iss: bundled Magick row must exclude duplicate policy.xml: $magickPayloadEntry"
    )
}

if ($ManagedPayloadPath) {
    if (-not (Test-Path -LiteralPath $ManagedPayloadPath -PathType Container)) {
        $violations.Add("  staged managed payload is missing: $ManagedPayloadPath")
    } else {
        $cleanupRules = @(
            foreach ($entry in $requiredUpgradeCleanup) {
                if ($entry -notmatch '^Type:\s*([^;]+);\s*Name:\s*"\{app\}\\([^"]+)"$') {
                    throw "internal cleanup rule could not be parsed: $entry"
                }
                [pscustomobject]@{
                    Type = $Matches[1].Trim()
                    Pattern = $Matches[2]
                }
            }
        )

        foreach ($item in Get-ChildItem -LiteralPath $ManagedPayloadPath) {
            $covered = if ($item.PSIsContainer) {
                @($cleanupRules | Where-Object {
                        $_.Type -ceq 'filesandordirs' -and $item.Name -ieq $_.Pattern
                    }).Count -eq 1
            } else {
                @($cleanupRules | Where-Object {
                        $_.Type -ceq 'files' -and $item.Name -like $_.Pattern
                    }).Count -ge 1
            }
            if (-not $covered) {
                $kind = if ($item.PSIsContainer) { 'directory' } else { 'file' }
                $violations.Add(
                    "  staged managed $kind has no exact [InstallDelete] coverage: $($item.Name)"
                )
            }
        }
    }
}

if ($CorePolicyPath) {
    if (-not (Test-Path -LiteralPath $CorePolicyPath -PathType Leaf)) {
        $violations.Add("  staged core policy is missing: $CorePolicyPath")
    } elseif ($ManagedPayloadPath) {
        $bundledPolicyPath = Join-Path $ManagedPayloadPath 'policy.xml'
        if (-not (Test-Path -LiteralPath $bundledPolicyPath -PathType Leaf)) {
            $violations.Add("  staged bundled policy is missing: $bundledPolicyPath")
        } else {
            $corePolicyHash = (Get-FileHash -LiteralPath $CorePolicyPath -Algorithm SHA256).Hash
            $bundledPolicyHash = (
                Get-FileHash -LiteralPath $bundledPolicyPath -Algorithm SHA256
            ).Hash
            if ($corePolicyHash -cne $bundledPolicyHash) {
                $violations.Add(
                    '  staged core and bundled policy.xml copies are not byte-identical'
                )
            }
        }
    }
}

if ($violations.Count -gt 0) {
    Write-Host "installer.iss lint FAILED" -ForegroundColor Red
    Write-Host "  Custom forms must be resource-safe, and upgrades must remove the exact" -ForegroundColor Red
    Write-Host "  managed ImageMagick payload before the engine is repopulated." -ForegroundColor Red
    Write-Host ""
    $violations | ForEach-Object { Write-Host $_ -ForegroundColor Yellow }
    exit 1
}

Write-Host "installer.iss lint OK (resource-safe forms + scoped upgrades + core policy)" -ForegroundColor Green
exit 0
