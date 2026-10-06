# Lists the tests that passed WITHOUT measuring anything, from the log scripts/test-timed.ps1
# keeps ($env:RT_NOT_MEASURED): a corpus sample absent, no Media Foundation, no second volume.
# A runner has no test corpus, so a green run here is not the same proof as a green run on a
# box with one; this says how far apart they are, as one warning on the run and a table in its
# summary. Never fails: it only reports.
if (-not $env:RT_NOT_MEASURED -or -not (Test-Path -LiteralPath $env:RT_NOT_MEASURED)) {
    Write-Host 'every test that ran measured what it tests'
    exit 0
}
$rows = Get-Content -LiteralPath $env:RT_NOT_MEASURED -Encoding utf8 |
    Where-Object { $_ } |
    ForEach-Object {
        $test, $why = $_ -split "`t", 2
        [pscustomobject]@{ Test = $test; Why = ($why -replace '^.*?NOT MEASURED[:\s]*', '').Trim() }
    } |
    Sort-Object Test, Why -Unique
$tests = @($rows | Select-Object -ExpandProperty Test -Unique)
Write-Host "::warning title=NOT MEASURED::$($tests.Count) passing tests measured nothing on this runner (no test corpus, no Media Foundation, ...); they are listed in the run summary"
$rows | ForEach-Object { Write-Host ('{0}  {1}' -f $_.Test, $_.Why) }
if ($env:GITHUB_STEP_SUMMARY) {
    $md = @("### $($tests.Count) tests passed without measuring", '', '| Test | Why |', '| --- | --- |')
    $md += $rows | ForEach-Object { '| `{0}` | {1} |' -f $_.Test, ($_.Why -replace '\|', '\|') }
    $md | Out-File -LiteralPath $env:GITHUB_STEP_SUMMARY -Append -Encoding utf8
}
exit 0
