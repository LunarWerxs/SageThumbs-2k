# Cargo test runner (CARGO_TARGET_<TRIPLE>_RUNNER="pwsh -NoProfile -File <this script>").
# Adds libtest's --report-time to TEST binaries only and appends the output to $env:RT_TEST_TIMES,
# so scripts/slowest-tests.ps1 can list the slowest tests. Every other binary runs untouched.
# $args, not param(): cargo's own flags (-Z..., --nocapture, --ignored) must reach the test
# binary, not bind to this script. Exits with the binary's exit code.
#
# With $env:RT_NOT_MEASURED set it also keeps every NOT MEASURED line a PASSING test printed
# (a corpus sample absent, no Media Foundation), as "<test>`t<line>", for
# scripts/not-measured.ps1. libtest throws a passing test's output away, so without this a
# runner with no corpus passed those tests green and nobody could see they measured nothing.
# The rest of the passing tests' output stays out of the log, as it always did.
$exe = $args[0]
$rest = @($args | Select-Object -Skip 1)
if ($exe -notmatch '[\\/]deps[\\/]') { & $exe @rest; exit $LASTEXITCODE }

$env:RUSTC_BOOTSTRAP = '1'   # unlocks -Zunstable-options for this test binary only, never the build
$rest += '-Zunstable-options', '--report-time'
$keep = [bool]$env:RT_NOT_MEASURED
if ($keep) { $rest += '--show-output' }
$test, $passed = '', $false
$sieve = {
    if ($keep) {
        # libtest prints the passing tests' output in a "successes:" block, then any failures.
        if ($_ -eq 'successes:') { $passed = $true }
        elseif ($_ -eq 'failures:' -or $_ -match '^test result:') { $passed = $false }
        if ($_ -match '^---- (.+) stdout ----$') { $test = $Matches[1] }
        if ($passed) {
            if ($_ -match 'NOT MEASURED') {
                Add-Content -LiteralPath $env:RT_NOT_MEASURED -Value "$test`t$($_.Trim())" -Encoding utf8
            }
            return
        }
    }
    $_
}
if ($env:RT_TEST_TIMES) { & $exe @rest | ForEach-Object $sieve | Tee-Object -FilePath $env:RT_TEST_TIMES -Append }
else { & $exe @rest | ForEach-Object $sieve }
exit $LASTEXITCODE
