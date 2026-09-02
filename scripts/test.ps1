<#
  Run the full test suite CORRECTLY.

  The integration tests (tests/*.rs) LoadLibrary the built DLL at
  target/<profile>/sagethumbs2k.dll. Plain `cargo test` builds the rlib used to
  link the tests but does NOT refresh that canonical cdylib, so the tests could
  load a STALE DLL. We `cargo build` first to force a fresh cdylib, in both
  profiles (release also exercises panic="abort").
#>
$ErrorActionPreference = 'Stop'

# $ErrorActionPreference only governs terminating PowerShell errors; a native
# process like cargo signals failure through $LASTEXITCODE, which PowerShell
# does not turn into a terminating error on its own. Without an explicit check
# after every call, a failing `cargo test` still let this script fall through
# to "All green." - the four calls below used to do exactly that.
function Invoke-Cargo {
    param([string[]]$CargoArgs)
    Write-Host "  > cargo $($CargoArgs -join ' ')" -ForegroundColor DarkGray
    & cargo @CargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo $($CargoArgs -join ' ') failed with exit code $LASTEXITCODE"
    }
}

Write-Host "== debug =="
Invoke-Cargo @('build')
Invoke-Cargo @('test')
Write-Host "== release =="
Invoke-Cargo @('build', '--release')
Invoke-Cargo @('test', '--release')
Write-Host "All green."
