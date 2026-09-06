<#
  Contract test for scripts\verify.ps1's -Lint script-invocation loops (2026-09-05 audit,
  finding F25). Dependency-free, does not build, does not touch crates/vendor.

  What broke: verify.ps1 ran a generic "invoke every script in this list with NO arguments"
  loop that included vendor-jxl.ps1. That script has a MUTATING bare/default mode (regenerate
  the vendored JXL trees from pristine sources, deleting each vendored tree first) alongside a
  separate, read-only `-Check` mode - see vendor-jxl.ps1's own header. Folding it into the
  generic loop meant "verify.ps1 -Lint" could discard uncommitted vendor edits, silently repair
  drift before the ladder ever validated it, and leave the tree half-regenerated on a failed
  patch. CI's consistency job only ever calls `vendor-jxl.ps1 -Check`.

  This test pins two things, generically rather than by special-casing one script name:
    (a) verify.ps1 invokes vendor-jxl.ps1 explicitly in -Check mode.
    (b) NONE of verify.ps1's generic "run these scripts with no arguments" loops name a
        script that declares its own `[switch]$Check` parameter - that parameter IS the
        signal a script uses to distinguish a mutating default from a validation-only mode
        (vendor-jxl.ps1, vendor-exr.ps1, set-sourceforge-default.ps1 all use this shape), so
        finding one bare inside a no-argument loop is exactly the bug class that shipped.
  A third case proves the detector itself would have caught the original bug, independent of
  git history: it feeds a synthetic loop shaped like the pre-fix code straight to the same
  detection function and asserts a violation is reported.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$scriptsRoot = $PSScriptRoot
$script:passed = 0
. (Join-Path $PSScriptRoot 'test-assert-lib.ps1')

# Matches this repo's "run these scripts with no arguments" idiom in verify.ps1:
#
#   foreach ($scriptName in @(
#       'a.ps1',
#       'b.ps1'
#   )) {
#       & pwsh -NoProfile -File (Join-Path $PSScriptRoot $scriptName)
#       ...
#   }
#
# Returns one array of quoted script names per loop of this exact shape found in $Text.
function Get-GenericLintLoopEntries {
    param([Parameter(Mandatory)][string]$Text)
    $pattern = [regex]::new(
        'foreach\s*\(\s*\$scriptName\s+in\s+@\(\s*(?<list>.*?)\)\)\s*\{\s*&\s*pwsh\s+-NoProfile\s+-File\s+\(Join-Path\s+\$PSScriptRoot\s+\$scriptName\)',
        [Text.RegularExpressions.RegexOptions]::Singleline
    )
    $loops = @()
    foreach ($m in $pattern.Matches($Text)) {
        $names = @([regex]::Matches($m.Groups['list'].Value, "'([^']+)'") | ForEach-Object { $_.Groups[1].Value })
        $loops += , $names
    }
    return $loops
}

# A script that declares its own `[switch]$Check` PARAMETER is, by this repo's convention
# (vendor-jxl.ps1, vendor-exr.ps1, set-sourceforge-default.ps1), one whose bare/default
# invocation MUTATES and whose `-Check` invocation only validates. Such a script has no
# business inside a loop that calls every entry with no arguments.
#
# Parsed via the real PowerShell AST rather than a text/regex search of the file, so this
# looks only at an actual parameter declaration - not at a comment, a doc-header example, or
# (as first written) this very test file's own prose about `[switch]$Check`, which a plain
# text search flagged as a false positive against itself the first time this ran.
function Test-ScriptDeclaresCheckSwitch {
    param([Parameter(Mandatory)][string]$Path)
    $tokens = $null
    $parseErrors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($Path, [ref]$tokens, [ref]$parseErrors)
    if ($parseErrors.Count) { return $false }
    $paramBlock = $ast.ParamBlock
    if (-not $paramBlock) { return $false }
    foreach ($p in $paramBlock.Parameters) {
        if ($p.Name.VariablePath.UserPath -ne 'Check') { continue }
        foreach ($attr in $p.Attributes) {
            if ($attr -is [System.Management.Automation.Language.TypeConstraintAst] -and
                $attr.TypeName.Name -eq 'switch') {
                return $true
            }
        }
    }
    return $false
}

# Scans every generic no-argument loop in $VerifyText and returns the names of any listed
# script that declares a `[switch]$Check` parameter - i.e. a mutating-by-default script being
# invoked without its validation switch.
function Get-MutatingScriptsInGenericLoops {
    param(
        [Parameter(Mandatory)][string]$VerifyText,
        [Parameter(Mandatory)][string]$ScriptsRoot
    )
    $violations = @()
    foreach ($names in (Get-GenericLintLoopEntries -Text $VerifyText)) {
        foreach ($name in $names) {
            $path = Join-Path $ScriptsRoot $name
            if (-not (Test-Path -LiteralPath $path)) { continue }
            if (Test-ScriptDeclaresCheckSwitch -Path $path) {
                $violations += $name
            }
        }
    }
    return $violations
}

$verifyPath = Join-Path $root 'scripts\verify.ps1'
$verifyText = Get-Content -LiteralPath $verifyPath -Raw

Assert-Passes 'verify.ps1 -Lint script loops never bare-invoke a script with its own -Check mode' {
    $violations = @(Get-MutatingScriptsInGenericLoops -VerifyText $verifyText -ScriptsRoot $scriptsRoot)
    if ($violations.Count -gt 0) {
        throw "generic no-argument loop invokes mutating-by-default script(s) without -Check: $($violations -join ', ')"
    }
}

Assert-Passes 'verify.ps1 invokes vendor-jxl.ps1 explicitly in -Check (validation-only) mode' {
    if ($verifyText -notmatch "vendor-jxl\.ps1'\)\s+-Check\b") {
        throw 'verify.ps1 no longer calls vendor-jxl.ps1 -Check explicitly'
    }
}

Assert-Passes 'the detector itself flags the exact pre-fix shape (2026-09-05 audit, F25)' {
    # Reproduces the shape of the pre-fix "release/installer/MSIX consistency contracts"
    # loop, which folded vendor-jxl.ps1 into the bare no-argument list. This does not depend
    # on git history staying a particular shape over time - it proves the detector would
    # catch the regression if verify.ps1 were ever reverted to it.
    $buggyFixture = @'
    Stage 'release/installer/MSIX consistency contracts' {
        foreach ($scriptName in @(
            'test-release-size.ps1',
            'check-vendored-exr.ps1',
            'vendor-jxl.ps1'
        )) {
            & pwsh -NoProfile -File (Join-Path $PSScriptRoot $scriptName)
            if ($LASTEXITCODE -ne 0) { throw "$scriptName failed" }
        }
    }
'@
    $violations = @(Get-MutatingScriptsInGenericLoops -VerifyText $buggyFixture -ScriptsRoot $scriptsRoot)
    if ($violations -notcontains 'vendor-jxl.ps1') {
        throw 'detector did not flag vendor-jxl.ps1 in a synthetic bare generic loop - it would have missed the real regression'
    }
}

Assert-Passes 'the detector does not false-positive on an ordinary validation-only loop' {
    # A loop of plain test-*.ps1 / check-*.ps1 scripts (none declaring [switch]$Check) must
    # report zero violations - otherwise this test would cry wolf on every ordinary entry.
    $cleanFixture = @'
    Stage 'architecture + freshness contracts' {
        foreach ($scriptName in @(
            'test-architecture-release-contract.ps1',
            'test-dev-architecture.ps1',
            'test-magick-dependency-freshness.ps1'
        )) {
            & pwsh -NoProfile -File (Join-Path $PSScriptRoot $scriptName)
            if ($LASTEXITCODE -ne 0) { throw "$scriptName failed" }
        }
    }
'@
    $violations = @(Get-MutatingScriptsInGenericLoops -VerifyText $cleanFixture -ScriptsRoot $scriptsRoot)
    if ($violations.Count -gt 0) {
        throw "false positive: clean fixture flagged $($violations -join ', ')"
    }
}

Write-Host "verify.ps1 lint-loop contract tests passed: $script:passed" -ForegroundColor Green
