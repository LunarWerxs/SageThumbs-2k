# Prints the 30 slowest passing tests from the log scripts/test-timed.ps1 appends to
# ($env:RT_TEST_TIMES). Never fails: it only reports.
if (-not $env:RT_TEST_TIMES -or -not (Test-Path -LiteralPath $env:RT_TEST_TIMES)) {
    Write-Host 'no per-test times were recorded'
    exit 0
}
Get-Content -LiteralPath $env:RT_TEST_TIMES |
    ForEach-Object {
        if ($_ -match '^test (.*) \.\.\. ok <([0-9.]+)s>$') {
            [pscustomobject]@{ Seconds = [double]$Matches[2]; Name = $Matches[1] }
        }
    } |
    Sort-Object Seconds -Descending |
    Select-Object -First 30 |
    ForEach-Object { '{0,8:N3}s  {1}' -f $_.Seconds, $_.Name }
exit 0
