<#
  Run the full test suite CORRECTLY.

  The integration tests (tests/*.rs) LoadLibrary the built DLL at
  target/<profile>/sagethumbs2k.dll. Plain `cargo test` builds the rlib used to
  link the tests but does NOT refresh that canonical cdylib, so the tests could
  load a STALE DLL. We `cargo build` first to force a fresh cdylib, in both
  profiles (release also exercises panic="abort").
#>
$ErrorActionPreference = 'Stop'
Write-Host "== debug =="
cargo build
cargo test
Write-Host "== release =="
cargo build --release
cargo test --release
Write-Host "All green."
