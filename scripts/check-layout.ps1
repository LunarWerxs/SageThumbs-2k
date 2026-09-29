<#
.SYNOPSIS
  Every window the headless --shot harness builds, in every shipped language and at several
  display scalings, checked for controls that are cut off, off the window's edge, or covering
  each other.

.DESCRIPTION
  Each capture runs with ST2K_LAYOUT_AUDIT pointing at a file, so the capture step
  (`win::capture_and_destroy`) records what `win::audit_layout` finds in the window it shows.
  Settings go to a throwaway portable ini per language (ST2K_PORTABLE_INI), so nothing here
  touches the registry or your own settings.

  The full matrix (36 languages x 3 scalings x 24 windows) takes ~40 minutes; the test suite's
  `layout_audit` runs the three cases that caught the most on every push. Run this after
  changing any dialog's layout or adding a translation, and before a release. A finding names
  the window, the control, its text and the measurement; fix the layout (size it to its text),
  or shorten that one translation when the layout has no room left.

.EXAMPLE
  pwsh -NoProfile -File scripts\check-layout.ps1
  pwsh -NoProfile -File scripts\check-layout.ps1 -Langs de,fi -Dpis 96,192
#>
[CmdletBinding()]
param(
    [string[]]$Langs = @(),
    [int[]]$Dpis = @(96, 144, 192),
    [int]$Parallel = 6,
    # Default: the release build in this repo's cargo target dir (see _targetdir.ps1).
    [string]$Exe = '',
    [string]$Out = (Join-Path ([IO.Path]::GetTempPath()) 'st2k-layout')
)
$ErrorActionPreference = 'Stop'
if (-not $Exe) { $Exe = Join-Path (& "$PSScriptRoot\_targetdir.ps1") 'release\SageThumbs2K.exe' }
if (-not $Langs) { $Langs = (Get-ChildItem (Join-Path $PSScriptRoot '..\assets\locales') -Filter *.toml).BaseName }
# Accept `-Langs de,fi` from `pwsh -File`, which hands the list over as one string.
$Langs = @($Langs | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
$Dpis = @($Dpis | ForEach-Object { "$_" -split ',' } | Where-Object { $_ } | ForEach-Object { [int]$_ })

New-Item -ItemType Directory -Force -Path $Out | Out-Null
Get-ChildItem $Out -File -ErrorAction SilentlyContinue | Remove-Item -Force
$windows = @('rename', 'files-to-folder', 'tags-to-folders', 'convert', 'convert-report', 'feedback', 'about',
    'doctor', 'upload', 'uploads', 'firstrun', 'firstrun2', 'ocr') + (0..10 | ForEach-Object { "settings:$_" })

$running = New-Object System.Collections.ArrayList
foreach ($lang in $Langs) {
    $ini = Join-Path $Out "$lang.ini"
    Set-Content -Path $ini -Value "[Settings]`nLang=$lang`nPreviewEnabled=1`nNavDotsSeen=2047" -Encoding utf8
    foreach ($dpi in $Dpis) {
        foreach ($w in $windows) {
            while ($running.Count -ge $Parallel) {
                @($running | Where-Object { $_.HasExited }) | ForEach-Object { [void]$running.Remove($_) }
                if ($running.Count -ge $Parallel) { Start-Sleep -Milliseconds 100 }
            }
            $tag = "$lang`_$dpi`_$($w -replace ':', '-')"
            $png = Join-Path $Out "$tag.png"
            $shotArgs = if ($w -like 'settings:*') { "--shot `"$png`" --tab $($w.Split(':')[1]) --dpi $dpi" }
            else { "--shot `"$png`" --window $w --dpi $dpi" }
            $env:ST2K_PORTABLE_INI = $ini
            $env:ST2K_LAYOUT_AUDIT = Join-Path $Out "$tag.jsonl"
            [void]$running.Add((Start-Process $Exe -ArgumentList $shotArgs -WindowStyle Hidden -PassThru))
        }
    }
}
$running | ForEach-Object { $_.WaitForExit() }
Remove-Item Env:\ST2K_PORTABLE_INI, Env:\ST2K_LAYOUT_AUDIT -ErrorAction SilentlyContinue
Get-ChildItem $Out -Filter *.png | Remove-Item -Force

$findings = foreach ($f in Get-ChildItem $Out -Filter *.jsonl) {
    Get-Content $f.FullName | ForEach-Object { "{0}: {1}" -f $f.BaseName, $_ }
}
$runs = $Langs.Count * $Dpis.Count * $windows.Count
"layout: $runs captures, $(@($findings).Count) finding(s) - details in $Out"
$findings | Select-Object -First 60
if ($findings) { exit 1 }
