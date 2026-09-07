<#
  qualify.ps1 - runs the storage-mode / multi-user qualification suite (audit item E04).

  Reads docs/QUALIFICATION-MATRIX.md and runs exactly its AUTOMATED rows: the derivation
  is FROM the table (parsed the same way tests/qualification_matrix.rs parses it), not a
  second hand-maintained list that could drift from it. It then prints the MANUAL rows as
  a checklist, with the procedure number a tester needs to open in that doc.

  Coverage handled per row:
    - AUTOMATED, evidence naming a `tests/x.rs::fn` or `src/x.rs::...::fn` reference: run
      that one test by name through `cargo test`, picking the right invocation shape for
      an integration test vs. a `src/bin/app/*` bin-crate test vs. a `src/*` lib test.
    - AUTOMATED, evidence naming a `scripts/x.ps1::Function-Name` reference: run that
      script once (it exercises the named function end to end against the real
      installer.iss); the same script is de-duplicated across every row that cites it.
    - AUTOMATED, evidence naming the ARM64 CI job (`aarch64-pc-windows-msvc`): this
      machine cannot run that natively. SKIPPED LOUDLY with the reason, never silently
      folded into a pass - see docs/DEVELOPMENT_GOTCHAS.md and CLAUDE.md on why a skip
      that reads as a pass is worse than no check at all.
    - MANUAL: printed as a checklist item naming its procedure number, run nothing.
    - UNSUPPORTED: not run, not printed (nothing to check - it is documented behaviour,
      not a test gap).

  Honours CARGO_TARGET_DIR if the caller has set it (cargo reads that env var itself; this
  script does not need to pass it through explicitly, but it prints what it resolved to so
  a run's timing is comparable to another).

  Exit code: non-zero if any AUTOMATED row's evidence failed to run, resolve, or pass.
#>

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$matrixPath = Join-Path $root 'docs/QUALIFICATION-MATRIX.md'

$targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { '(unset - .cargo/config.toml default)' }
Write-Host "[qualify] CARGO_TARGET_DIR = $targetDir" -ForegroundColor Cyan

# ---- parse the table (mirrors tests/qualification_matrix.rs's parsing) -----------------

$expectedHeader = '| # | Scenario | Storage | Process | User | Sessions | Coverage | Evidence |'

function Split-Row([string]$line) {
    $trimmed = $line.Trim()
    $inner = $trimmed
    if ($inner.StartsWith('|')) { $inner = $inner.Substring(1) }
    if ($inner.EndsWith('|')) { $inner = $inner.Substring(0, $inner.Length - 1) }
    return $inner -split '\|' | ForEach-Object { $_.Trim() }
}

function Get-MatrixRows {
    $lines = [System.IO.File]::ReadAllLines($matrixPath)
    $headerIdx = -1
    for ($i = 0; $i -lt $lines.Length; $i++) {
        if ($lines[$i].Trim() -eq $expectedHeader) { $headerIdx = $i; break }
    }
    if ($headerIdx -lt 0) {
        throw "could not find the format-contract header row verbatim in $matrixPath"
    }
    $rows = @()
    for ($i = $headerIdx + 2; $i -lt $lines.Length; $i++) {
        $trimmed = $lines[$i].Trim()
        if (-not $trimmed.StartsWith('|')) { break }
        $cells = Split-Row $trimmed
        if ($cells.Count -ne 8) {
            throw "expected 8 columns, got $($cells.Count) in row: $trimmed"
        }
        for ($col = 0; $col -lt $cells.Count; $col++) {
            if ([string]::IsNullOrEmpty($cells[$col])) {
                throw "blank cell in column $col of row $($cells[0]): $trimmed"
            }
        }
        $rows += [pscustomobject]@{
            Number   = $cells[0]
            Scenario = $cells[1]
            Coverage = $cells[6]
            Evidence = $cells[7]
        }
    }
    return $rows
}

# Pulls every backtick-delimited `tests/...::...` / `src/...::...` / `scripts/...::...`
# reference out of an evidence cell.
function Get-References([string]$evidence) {
    $spans = $evidence -split '`' | Where-Object { $_ -like '*::*' }
    $refs = @()
    foreach ($span in $spans) {
        $parts = $span -split '::', 2
        $path = $parts[0]
        $tail = $parts[1]
        $funcParts = $tail -split '::'
        $func = $funcParts[$funcParts.Count - 1]
        if ($path.EndsWith('.rs') -and ($path.StartsWith('tests/') -or $path.StartsWith('src/'))) {
            $refs += [pscustomobject]@{ Kind = 'rust'; File = $path; Func = $func }
        } elseif ($path.EndsWith('.ps1') -and $path.StartsWith('scripts/')) {
            $refs += [pscustomobject]@{ Kind = 'ps1'; File = $path; Func = $func }
        }
    }
    return $refs
}

$rows = Get-MatrixRows
Write-Host "[qualify] parsed $($rows.Count) scenario rows from $matrixPath" -ForegroundColor Cyan

$automated = $rows | Where-Object { $_.Coverage -eq 'AUTOMATED' }
$manual    = $rows | Where-Object { $_.Coverage -eq 'MANUAL' }

# ---- separate the ARM64-CI-only rows (cannot run natively on this host) ----------------

$isArm64Host = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq
    [System.Runtime.InteropServices.Architecture]::Arm64

$ciOnlyRows   = @()
$runnableRows = @()
foreach ($row in $automated) {
    if ($row.Evidence -match 'aarch64-pc-windows-msvc' -and -not $isArm64Host) {
        $ciOnlyRows += $row
    } else {
        $runnableRows += $row
    }
}

# ---- collect the distinct rust-test and script invocations across all runnable rows ----

$rustRuns = @()   # {Row, File, Func}
$scriptFiles = New-Object System.Collections.Generic.HashSet[string]
$scriptToRows = @{}

foreach ($row in $runnableRows) {
    $refs = Get-References $row.Evidence
    if ($refs.Count -eq 0) {
        throw "AUTOMATED row $($row.Number) has no resolvable tests/src/scripts evidence reference: $($row.Evidence)"
    }
    foreach ($ref in $refs) {
        if ($ref.Kind -eq 'rust') {
            $rustRuns += [pscustomobject]@{ Row = $row.Number; File = $ref.File; Func = $ref.Func }
        } else {
            [void]$scriptFiles.Add($ref.File)
            if (-not $scriptToRows.ContainsKey($ref.File)) { $scriptToRows[$ref.File] = @() }
            $scriptToRows[$ref.File] += $row.Number
        }
    }
}

$failures = @()
$passCount = 0

# ---- run each named Rust test ----------------------------------------------------------

foreach ($run in $rustRuns) {
    $cargoArgs = @('test')
    if ($run.File.StartsWith('tests/')) {
        $testBin = [System.IO.Path]::GetFileNameWithoutExtension($run.File)
        $cargoArgs += @('--test', $testBin)
    } elseif ($run.File.StartsWith('src/bin/app/')) {
        $cargoArgs += @('--bin', 'SageThumbs2K')
    } else {
        $cargoArgs += @('-p', 'sagethumbs2k', '--lib')
    }
    # No `--exact`: the bin/lib crate tests live inside a `mod tests` whose full path
    # (module-prefixed) this script does not re-derive, and a substring filter on these
    # long, distinctive function names cannot collide with an unrelated test.
    $cargoArgs += @($run.Func)

    Write-Host "[qualify] row $($run.Row): cargo $($cargoArgs -join ' ')" -ForegroundColor Yellow
    Push-Location $root
    try {
        & cargo @cargoArgs 2>&1 | Tee-Object -Variable cargoOut | Out-Null
        $exit = $LASTEXITCODE
    } finally {
        Pop-Location
    }
    $resultLine = ($cargoOut | Select-String -Pattern 'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed' | Select-Object -Last 1)
    $ranAtLeastOne = $false
    $reportedOk = $false
    if ($resultLine) {
        $m = $resultLine.Matches[0]
        $reportedOk = ($m.Groups[1].Value -eq 'ok') -and ($m.Groups[3].Value -eq '0')
        $ranAtLeastOne = [int]$m.Groups[2].Value -gt 0
    }
    if ($exit -ne 0 -or -not $reportedOk -or -not $ranAtLeastOne) {
        $why = if (-not $ranAtLeastOne) { "no test matched the filter '$($run.Func)' - the evidence is stale" } else { 'the test failed' }
        Write-Host "[qualify] FAILED: row $($run.Row) ($($run.File)::$($run.Func)) - $why" -ForegroundColor Red
        $cargoOut | Write-Host
        $failures += "row $($run.Row): $($run.File)::$($run.Func) - $why"
    } else {
        Write-Host "[qualify] passed: row $($run.Row) ($($run.File)::$($run.Func))" -ForegroundColor Green
        $passCount++
    }
}

# ---- run each distinct installer-lint (or other) script once --------------------------

foreach ($scriptFile in $scriptFiles) {
    $rowsCiting = ($scriptToRows[$scriptFile] | Select-Object -Unique) -join ', '
    Write-Host "[qualify] rows $rowsCiting -> pwsh -File $scriptFile" -ForegroundColor Yellow
    $scriptPath = Join-Path $root $scriptFile
    Push-Location $root
    try {
        & pwsh -NoProfile -File $scriptPath 2>&1 | Tee-Object -Variable scriptOut | Out-Null
        $exit = $LASTEXITCODE
    } finally {
        Pop-Location
    }
    if ($exit -ne 0) {
        Write-Host "[qualify] FAILED: $scriptFile (rows $rowsCiting)" -ForegroundColor Red
        $scriptOut | Write-Host
        $failures += "rows ${rowsCiting}: $scriptFile"
    } else {
        Write-Host "[qualify] passed: $scriptFile (rows $rowsCiting)" -ForegroundColor Green
        $passCount++
    }
}

# ---- report the CI-only rows loudly, never as a silent pass or a silent drop ----------

foreach ($row in $ciOnlyRows) {
    Write-Host "[qualify] SKIP (row $($row.Number)): ARM64-only, verified by the arm64-native CI job in .github/workflows/ci.yml. This host is $([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture), not Arm64, so it cannot run this natively." -ForegroundColor DarkYellow
}

# ---- print the MANUAL checklist ---------------------------------------------------------

if ($manual.Count -gt 0) {
    Write-Host ''
    Write-Host '[qualify] MANUAL scenarios - run these on a VM, then check them off by hand:' -ForegroundColor Cyan
    foreach ($row in $manual) {
        $procMatch = [regex]::Match($row.Evidence, 'Manual procedure (\d+)')
        $proc = if ($procMatch.Success) { $procMatch.Groups[1].Value } else { '?' }
        Write-Host "  [ ] row $($row.Number): $($row.Scenario)" -ForegroundColor White
        Write-Host "      -> docs/QUALIFICATION-MATRIX.md, ### Procedure $proc" -ForegroundColor DarkGray
    }
}

Write-Host ''
Write-Host "[qualify] $passCount automated check(s) passed, $($failures.Count) failed, $($ciOnlyRows.Count) skipped (ARM64-only), $($manual.Count) manual scenario(s) listed above." -ForegroundColor Cyan

if ($failures.Count -gt 0) {
    Write-Host '[qualify] FAILURES:' -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}

exit 0
