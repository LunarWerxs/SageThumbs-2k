# Renders every test-corpus sample through st2k and reports which formats
# thumbnail successfully — a one-shot regression check. Builds a labelled
# contact sheet (contact.png) so all thumbnails can be eyeballed at once.
#
#   pwsh scripts\regression.ps1
param(
    [string]$Corpus = "$PSScriptRoot\..\..\test-corpus",
    [int]$Size = 96
)
$ErrorActionPreference = 'Continue'
$st2k = @("D:\st2k-target\release\st2k.exe", "$PSScriptRoot\..\target\release\st2k.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $st2k) { throw "st2k.exe not built (cargo build --release --bin st2k)" }
$magick = (Get-ChildItem 'C:\Program Files\ImageMagick*\magick.exe' -EA SilentlyContinue | Select-Object -First 1).FullName

$render = "$Corpus\_render"
if (Test-Path $render) { Remove-Item $render -Recurse -Force }
New-Item -ItemType Directory -Force $render | Out-Null

$skipExt = '.md', '.txt'
$files = Get-ChildItem $Corpus -File | Where-Object { $_.Name -notlike '_*' -and $skipExt -notcontains $_.Extension.ToLower() } | Sort-Object Name
$pass = @(); $fail = @()
foreach ($f in $files) {
    $ext = $f.Extension.TrimStart('.').ToLower()
    $out = "$render\$ext.png"
    & $st2k thumbnail $f.FullName $out --size $Size 2>$null | Out-Null
    if ((Test-Path $out) -and (Get-Item $out).Length -gt 0) { $pass += $ext } else { $fail += $ext }
}

Write-Host ("[regression] PASS {0}/{1}" -f $pass.Count, $files.Count) -ForegroundColor Green
if ($fail.Count) { Write-Host ("[regression] no-thumbnail ({0}): {1}" -f $fail.Count, (($fail | Sort-Object) -join ' ')) -ForegroundColor Yellow }

# Labelled contact sheet of everything that rendered.
if ($magick -and $pass.Count) {
    $contact = "$Corpus\contact.png"
    & $magick montage "$render\*.png" -label '%t' -tile 13x -geometry 92x92+3+3 -background '#202020' -fill '#dddddd' -pointsize 11 $contact 2>$null
    if (Test-Path $contact) { Write-Host "[regression] contact sheet: $contact" -ForegroundColor Cyan }
}
