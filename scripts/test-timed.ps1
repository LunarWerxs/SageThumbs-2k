# Cargo test runner (CARGO_TARGET_<TRIPLE>_RUNNER="pwsh -NoProfile -File <this script>").
# Adds libtest's --report-time to TEST binaries only and appends the output to $env:RT_TEST_TIMES,
# so scripts/slowest-tests.ps1 can list the slowest tests. Every other binary runs untouched.
# $args, not param(): cargo's own flags (-Z..., --nocapture, --ignored) must reach the test
# binary, not bind to this script. Exits with the binary's exit code.
$exe = $args[0]
$rest = @($args | Select-Object -Skip 1)
if ($exe -notmatch '[\\/]deps[\\/]') { & $exe @rest; exit $LASTEXITCODE }

$env:RUSTC_BOOTSTRAP = '1'   # unlocks -Zunstable-options for this test binary only, never the build
$rest += '-Zunstable-options', '--report-time'
if ($env:RT_TEST_TIMES) { & $exe @rest | Tee-Object -FilePath $env:RT_TEST_TIMES -Append }
else { & $exe @rest }
exit $LASTEXITCODE
