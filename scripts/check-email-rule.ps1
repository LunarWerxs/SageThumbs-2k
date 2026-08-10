<#
  check-email-rule.ps1 - holds the THREE implementations of the reply-address rule to one
  shared contract.

  The uninstall survey's reply field is email-only (2026-08-05). Enforcing that takes three
  implementations, because it runs in three runtimes and none of them can call the others:

      scripts/packaging/installer.iss        LooksLikeEmail    Pascal Script, inside the uninstaller
      src/bin/app/feedback.rs        looks_like_email  Rust, the in-app Send feedback box
      scripts/packaging/analytics/worker.js  looksLikeEmail    JS, the Cloudflare Worker intake gate

  Three copies of a rule is three chances to drift, and the drift is INVISIBLE: a stricter
  client silently drops addresses the server would have kept, a stricter server silently
  discards ones the client promised to deliver. So all three are run against ONE table of
  cases, tests\fixtures\email-rule-cases.txt.

  Rust is covered by `feedback::tests::email_rule_matches_shared_table` under cargo test.
  This script covers the other two:

    * JS     - the shipped `looksLikeEmail` is EXTRACTED from worker.js and executed, so this
               tests the deployed function, not a transcription of it. Requires node, which
               CI's windows-latest image and the dev box both have. A missing node FAILS
               rather than skipping - a silent skip is how a check quietly stops checking.

    * Pascal - the shipped `LooksLikeEmail` is extracted from installer.iss verbatim into a
               throwaway setup whose InitializeSetup writes verdicts and returns False (so it
               can never install anything), compiled and run. This needs ISCC, which is NOT on
               a stock CI runner. When ISCC is absent the Pascal leg is REPORTED AS SKIPPED and
               the script still succeeds - the release pipeline has ISCC, so it is covered
               there, and this way the check is still useful on a plain runner. The skip is
               always printed; it is never silent.

  Run standalone:  pwsh scripts\check-email-rule.ps1
#>
[CmdletBinding()]
param(
    [string]$Root = (Split-Path $PSScriptRoot -Parent)
)

$ErrorActionPreference = 'Stop'

$casesPath = Join-Path $Root 'tests\fixtures\email-rule-cases.txt'
$workerPath = Join-Path $Root 'scripts\packaging\analytics\worker.js'
$issPath = Join-Path $Root 'scripts\packaging\installer.iss'

# The fixture and installer.iss are TRACKED, so their absence is a real breakage.
foreach ($p in @($casesPath, $issPath)) {
    if (-not (Test-Path $p)) { Write-Error "missing required file: $p"; exit 1 }
}
# worker.js is NOT: `/scripts/packaging/analytics/` is gitignored, so a clean checkout never has it and
# CI never will. Treating that as a failure would make the CI step red on every single run,
# which is why this is a reported SKIP like the Pascal leg rather than a hard error. The dev box
# and the release pipeline both have the file, so the JS leg still actually runs where it can.
$haveWorker = Test-Path $workerPath

# ---- the shared table -------------------------------------------------------------------
# Everything after the FIRST '|' is the value verbatim, so a case may itself contain '|'.
$cases = [System.Collections.Generic.List[object]]::new()
foreach ($line in [IO.File]::ReadAllLines($casesPath)) {
    $l = $line.TrimEnd()
    if (-not $l -or $l.StartsWith('#')) { continue }
    $i = $l.IndexOf('|')
    if ($i -lt 0) { Write-Error "case line has no '|': $l"; exit 1 }
    $cases.Add([pscustomobject]@{
            Want  = $l.Substring(0, $i)
            Value = $l.Substring($i + 1)
        })
}
if ($cases.Count -lt 25) {
    Write-Error "shared fixture looks truncated: $($cases.Count) cases parsed"
    exit 1
}
Write-Host "[email-rule] $($cases.Count) shared cases" -ForegroundColor DarkGray

$failures = 0

function Compare-Verdicts {
    param([string]$Runtime, [string[]]$Got)

    if ($Got.Count -ne $script:cases.Count) {
        Write-Host "  FAIL $Runtime produced $($Got.Count) verdicts for $($script:cases.Count) cases" -ForegroundColor Red
        return 1
    }
    $bad = 0
    for ($i = 0; $i -lt $script:cases.Count; $i++) {
        if ($Got[$i] -ne $script:cases[$i].Want) {
            $bad++
            $shown = $script:cases[$i].Value
            if (-not $shown) { $shown = '(empty)' }
            Write-Host ("  FAIL {0}: [{1}] want={2} got={3}" -f
                $Runtime, $shown, $script:cases[$i].Want, $Got[$i]) -ForegroundColor Red
        }
    }
    if ($bad -eq 0) { Write-Host "  ok   $Runtime agrees on all $($script:cases.Count) cases" -ForegroundColor Green }
    return $bad
}

# ---- JS: extract the shipped function and run it -----------------------------------------
$node = Get-Command node -ErrorAction SilentlyContinue
if (-not $haveWorker) {
    Write-Host "  SKIP js     worker.js is gitignored and absent here (covered on the dev box + release pipeline)" -ForegroundColor Yellow
}
elseif (-not $node) {
    # node absence IS a hard failure: unlike worker.js it is tooling, present on every runner
    # we use, and skipping on it is how a check quietly stops checking.
    Write-Error "node not found - required to verify the Worker's looksLikeEmail. Install Node, or run this on a runner that has it."
    exit 1
}

$jsRunner = @'
const fs = require("fs");
const [workerPath, casesPath] = process.argv.slice(2);
const src = fs.readFileSync(workerPath, "utf8");
// Pull the SHIPPED function out of worker.js so this tests the real code, not a copy of it.
const m = src.match(/function looksLikeEmail\(s\) \{[\s\S]*?\n\}/);
if (!m) { console.error("looksLikeEmail not found in worker.js"); process.exit(2); }
const fn = new Function(m[0] + "; return looksLikeEmail;")();
const out = [];
for (const line of fs.readFileSync(casesPath, "utf8").split(/\r?\n/)) {
  const l = line.replace(/\s+$/, "");
  if (!l || l.startsWith("#")) continue;
  const i = l.indexOf("|");
  if (i < 0) continue;
  out.push(fn(l.slice(i + 1)) ? "1" : "0");
}
process.stdout.write(out.join("\n"));
'@
if ($haveWorker) {
    $jsFile = Join-Path ([IO.Path]::GetTempPath()) ("st2k-emailrule-" + [guid]::NewGuid().ToString('N') + ".js")
    try {
        [IO.File]::WriteAllText($jsFile, $jsRunner)
        $jsOut = & node $jsFile $workerPath $casesPath
        if ($LASTEXITCODE -ne 0) { Write-Host "  FAIL js runner exited $LASTEXITCODE" -ForegroundColor Red; $failures++ }
        else { $failures += Compare-Verdicts -Runtime 'js    ' -Got @($jsOut -split "`r?`n") }
    }
    finally {
        Remove-Item $jsFile -ErrorAction SilentlyContinue
    }
}

# ---- Pascal: extract the shipped function, compile it, run it ----------------------------
$iscc = @(
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1

if (-not $iscc) {
    # Loud, never silent: this line is the record that one leg did not run.
    Write-Host "  SKIP pascal - ISCC.exe not found (covered by the release pipeline, which has it)" -ForegroundColor Yellow
}
else {
    $work = Join-Path ([IO.Path]::GetTempPath()) ("st2k-emailrule-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $work -Force | Out-Null
    try {
        # Verbatim slice of the shipped function: from its signature to the first line that is
        # exactly "end;" at column 0, which is how every top-level routine in the file closes.
        $iss = [IO.File]::ReadAllLines($issPath)
        $fn = [System.Collections.Generic.List[string]]::new()
        $collecting = $false
        foreach ($line in $iss) {
            if (-not $collecting -and $line -match '^function LooksLikeEmail') { $collecting = $true }
            if ($collecting) {
                $fn.Add($line)
                if ($line -eq 'end;') { break }
            }
        }
        if ($fn.Count -lt 10 -or $fn[-1] -ne 'end;') {
            Write-Host "  FAIL could not extract LooksLikeEmail from installer.iss" -ForegroundColor Red
            $failures++
        }
        else {
            $verdictPath = Join-Path $work 'verdicts.txt'
            $probe = @()
            $probe += '[Setup]'
            $probe += 'AppName=EmailRuleProbe'
            $probe += 'AppVersion=1'
            $probe += 'DefaultDirName={autopf}\EmailRuleProbe'
            $probe += "OutputDir=$work"
            $probe += 'OutputBaseFilename=emailruleprobe'
            $probe += '[Code]'
            $probe += $fn
            $probe += ''
            # Returns False so setup ALWAYS aborts before installing anything. There is no
            # [Files] or [Run] section either - this binary can only write the verdict file.
            $probe += 'function InitializeSetup(): Boolean;'
            $probe += 'var'
            $probe += '  Lines, Verdicts: TArrayOfString;'
            $probe += '  i, p, n: Integer;'
            $probe += '  Line, Val: String;'
            $probe += 'begin'
            $probe += '  Result := False;'
            $probe += "  if not LoadStringsFromFile('$casesPath', Lines) then Exit;"
            $probe += '  SetArrayLength(Verdicts, GetArrayLength(Lines));'
            $probe += '  n := 0;'
            $probe += '  for i := 0 to GetArrayLength(Lines) - 1 do begin'
            $probe += '    Line := Trim(Lines[i]);'
            $probe += "    if (Line = '') or (Copy(Line, 1, 1) = '#') then Continue;"
            $probe += "    p := Pos('|', Line);"
            $probe += '    if p = 0 then Continue;'
            $probe += '    Val := Copy(Line, p + 1, Length(Line) - p);'
            $probe += "    if LooksLikeEmail(Val) then Verdicts[n] := '1' else Verdicts[n] := '0';"
            $probe += '    n := n + 1;'
            $probe += '  end;'
            $probe += '  SetArrayLength(Verdicts, n);'
            $probe += "  SaveStringsToFile('$verdictPath', Verdicts, False);"
            $probe += 'end;'

            $probePath = Join-Path $work 'probe.iss'
            [IO.File]::WriteAllLines($probePath, $probe)
            & $iscc $probePath /Q *> $null
            if ($LASTEXITCODE -ne 0) {
                Write-Host "  FAIL pascal probe did not compile (exit $LASTEXITCODE) - run ISCC on $probePath to see why" -ForegroundColor Red
                $failures++
            }
            else {
                $exe = Join-Path $work 'emailruleprobe.exe'
                $p = Start-Process $exe -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES' -Wait -PassThru
                if (-not (Test-Path $verdictPath)) {
                    Write-Host "  FAIL pascal probe wrote no verdicts (exit $($p.ExitCode))" -ForegroundColor Red
                    $failures++
                }
                else {
                    $failures += Compare-Verdicts -Runtime 'pascal' -Got @([IO.File]::ReadAllLines($verdictPath))
                }
            }
        }
    }
    finally {
        Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# Rust runs under cargo test; assert the wiring is present so deleting the test is noticed.
$rs = Get-Content (Join-Path $Root 'src\bin\app\feedback.rs') -Raw
if ($rs -notmatch 'email-rule-cases\.txt' -or $rs -notmatch 'fn email_rule_matches_shared_table') {
    Write-Host "  FAIL src\bin\app\feedback.rs no longer reads the shared fixture in email_rule_matches_shared_table" -ForegroundColor Red
    $failures++
}
else {
    Write-Host "  ok   rust  reads the same fixture (verdicts asserted by cargo test)" -ForegroundColor Green
}

if ($failures -gt 0) {
    Write-Host "[email-rule] FAILED - $failures problem(s). The three implementations must agree; see the header of this script." -ForegroundColor Red
    exit 1
}
Write-Host "[email-rule] OK" -ForegroundColor Green
exit 0
