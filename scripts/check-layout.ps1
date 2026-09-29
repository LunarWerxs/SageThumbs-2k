<#
.SYNOPSIS
  Every window the headless --shot harness builds, in every shipped language and at several
  display scalings, checked for controls that are cut off, off the window's edge, or covering
  each other.

.DESCRIPTION
  One `SageThumbs2K.exe --audit-layout` process per language and scaling builds every dialog
  and every Settings page in turn and records what `win::audit_layout` finds (see
  `modes::run_audit_layout`). Settings go to a throwaway portable ini per language
  (ST2K_PORTABLE_INI), so nothing here touches the registry or your own settings.

  The full matrix (36 languages x 3 scalings, 24 windows each) is 108 processes. It used to be
  2,592 `--shot` captures and took most of an hour; see the wall time this prints. The test
  suite's `layout_audit` runs the three cases that caught the most on every push. Run this
  after changing any dialog's layout or adding a translation, and before a release. A finding
  names the window, the control, its text and the measurement; fix the layout (size it to its
  text), or shorten that one translation when the layout has no room left.

.EXAMPLE
  pwsh -NoProfile -File scripts\check-layout.ps1
  pwsh -NoProfile -File scripts\check-layout.ps1 -Langs de,fi -Dpis 96,192
#>
[CmdletBinding()]
param(
    [string[]]$Langs = @(),
    [int[]]$Dpis = @(96, 144, 192),
    # Each run mostly waits on window creation, so twice the core count would oversubscribe
    # nothing; half of it leaves the machine usable.
    [int]$Parallel = [Math]::Max(4, [Environment]::ProcessorCount / 2),
    # Default: the release build in this repo's cargo target dir (see _targetdir.ps1).
    [string]$Exe = '',
    [string]$Out = (Join-Path ([IO.Path]::GetTempPath()) 'st2k-layout')
)
$ErrorActionPreference = 'Stop'
$clock = [Diagnostics.Stopwatch]::StartNew()
if (-not $Exe) { $Exe = Join-Path (& "$PSScriptRoot\_targetdir.ps1") 'release\SageThumbs2K.exe' }
if (-not $Langs) { $Langs = (Get-ChildItem (Join-Path $PSScriptRoot '..\assets\locales') -Filter *.toml).BaseName }
# Accept `-Langs de,fi` from `pwsh -File`, which hands the list over as one string.
$Langs = @($Langs | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
$Dpis = @($Dpis | ForEach-Object { "$_" -split ',' } | Where-Object { $_ } | ForEach-Object { [int]$_ })

New-Item -ItemType Directory -Force -Path $Out | Out-Null
Get-ChildItem $Out -File -ErrorAction SilentlyContinue | Remove-Item -Force

$running = New-Object System.Collections.ArrayList
$failed = New-Object System.Collections.ArrayList
function Reap {
    foreach ($r in @($running | Where-Object { $_.Proc.HasExited })) {
        if ($r.Proc.ExitCode -ne 0) { [void]$failed.Add("$($r.Tag): exit $($r.Proc.ExitCode)") }
        [void]$running.Remove($r)
    }
}
foreach ($lang in $Langs) {
    $ini = Join-Path $Out "$lang.ini"
    Set-Content -Path $ini -Value "[Settings]`nLang=$lang`nPreviewEnabled=1`nNavDotsSeen=2047" -Encoding utf8
    foreach ($dpi in $Dpis) {
        while ($running.Count -ge $Parallel) { Reap; if ($running.Count -ge $Parallel) { Start-Sleep -Milliseconds 100 } }
        $tag = "$lang`_$dpi"
        $env:ST2K_PORTABLE_INI = $ini
        $p = Start-Process $Exe -ArgumentList "--audit-layout `"$(Join-Path $Out "$tag.jsonl")`" --dpi $dpi" -WindowStyle Hidden -PassThru
        $null = $p.Handle # keep the handle, or ExitCode reads back empty once it has exited
        [void]$running.Add([pscustomobject]@{ Tag = $tag; Proc = $p })
    }
}
while ($running.Count) { Reap; if ($running.Count) { Start-Sleep -Milliseconds 100 } }
Remove-Item Env:\ST2K_PORTABLE_INI -ErrorAction SilentlyContinue

$findings = New-Object System.Collections.ArrayList
$audited = 0
foreach ($f in Get-ChildItem $Out -Filter *.jsonl) {
    foreach ($line in Get-Content $f.FullName) {
        if ($line -match '"kind":') { [void]$findings.Add("$($f.BaseName): $line") }
        elseif ($line -match '"error":') { [void]$failed.Add("$($f.BaseName): $line") }
        elseif ($line -match '"audited":') { $audited++ }
    }
}
"layout: $audited shots audited ($($Langs.Count) languages x $($Dpis.Count) scalings), $($findings.Count) finding(s), $($failed.Count) failed run(s), $([int]$clock.Elapsed.TotalSeconds) s - details in $Out"
$failed | Select-Object -First 20
$findings | Select-Object -First 60
if ($findings.Count -or $failed.Count -or -not $audited) { exit 1 }
