<#
  check-installer.ps1 - static lint for packaging\installer.iss [Code].

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
    [string]$IssPath = (Join-Path (Split-Path $PSScriptRoot -Parent) 'packaging\installer.iss')
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

if ($violations.Count -gt 0) {
    Write-Host "installer.iss [Code] lint FAILED" -ForegroundColor Red
    Write-Host "  Custom forms must use CreateCustomForm(w, h, keepX, keepY), never" -ForegroundColor Red
    Write-Host "  TSetupForm.Create(nil): the latter needs a form resource the uninstaller" -ForegroundColor Red
    Write-Host "  doesn't ship and dies with 'Resource TSetupForm not found' (issue #3)." -ForegroundColor Red
    Write-Host ""
    $violations | ForEach-Object { Write-Host $_ -ForegroundColor Yellow }
    exit 1
}

Write-Host "installer.iss [Code] lint OK (no TSetupForm.Create; custom forms use CreateCustomForm)" -ForegroundColor Green
exit 0
