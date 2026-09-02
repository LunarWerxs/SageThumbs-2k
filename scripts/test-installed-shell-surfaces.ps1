<#
  Prove the shell surfaces of a BUILT SageThumbs 2K DLL actually work, headlessly.

  This exists because the ARM64 release gate used to read "install it on a real ARM64
  device and click around in Explorer", which is not a gate anyone can run on demand:
  an x64 host cannot run an ARM64 Windows guest, so it blocked the release on borrowed
  hardware. Almost all of that gate does NOT need a desktop. Explorer asks the shell for
  a thumbnail through IShellItemImageFactory and hosts the preview through
  IPreviewHandler; both are COM calls that work in a non-interactive session, which is
  exactly what a GitHub-hosted runner gives us. So CI can answer the real question -
  "does the registered handler load on this architecture and return a correct image?" -
  on native hardware, every push.

  What it checks, against the DLL you point it at:
    1. regsvr32 registers it (waiting on the process and reading its REAL exit code -
       PowerShell does not populate $LASTEXITCODE for a GUI-subsystem exe).
    2. `st2k doctor` runs and reports the CLSIDs registered and its decode self-test OK.
    3. IShellItemImageFactory returns a non-blank thumbnail for a format only WE handle
       (a TGA the CLI writes itself, so this needs no corpus checkout).
    4. The registered IPreviewHandler CLSID activates, renders into an owned off-screen
       window, and produces a non-blank capture.
  Then it unregisters, always, even on failure.

  What it deliberately does NOT check: the pixels of Explorer's own right-click flyout.
  That needs an interactive desktop. It is also the one part that is pure Windows shell
  chrome driving architecture-independent Rust, so it is the cheapest thing to leave to
  a human spot-check rather than the thing to block a release on.

      pwsh scripts\test-installed-shell-surfaces.ps1 -TargetDir <dir with the built DLL>
      pwsh scripts\test-installed-shell-surfaces.ps1 -UseInstalled   # probe the installed
                                                                     # copy, register nothing

  -ExtraSamples pushes corpus files through the Explorer thumbnail surface too, with an
  optional expected colour. Run this after touching anything the SHELL path depends on
  that the CLI does not - the stream name lookup in particular, which the CLI gets from
  its own argv and the shell has to be asked for:

      pwsh scripts\test-installed-shell-surfaces.ps1 -TargetDir path\to\your\st2k-target\release `
          -ExtraSamples ..\..\test-corpus\sample.rla, ..\..\test-corpus\sample.tim, `
                        ..\..\test-corpus\sample.scr=255,0,0

  Needs elevation (regsvr32), so drive it with Start-Process -Verb RunAs.
#>
[CmdletBinding()]
param(
    # Directory holding the freshly built sagethumbs2k.dll + st2k.exe to register and probe.
    [string]$TargetDir,
    # Probe the already-installed machine-wide copy instead. Registers and unregisters
    # NOTHING, so it is safe to run on a working desktop.
    [switch]$UseInstalled,
    # Where to drop the rendered proofs. Handy as a CI artifact when something fails.
    [string]$ArtifactDir,
    # Extra files to push through the Explorer thumbnail surface as well.
    #
    # The built-in TGA proves the handler LOADS and returns an image. It cannot prove
    # anything about a format whose decode depends on the shell telling us the file's
    # NAME - and that is a real, separate failure mode: the formats ImageMagick can only
    # identify by extension (RLA, TIM, MacPaint, SCREEN$, ...) are decoded by naming the
    # coder from the stream's reported name, which the CLI gets for free and the shell
    # does not have to provide at all. Verified by CLI alone, that path looks fine while
    # being dead in Explorer, which is the only surface users see.
    #
    # Empty by default so CI (which has no corpus) is unchanged.
    [string[]]$ExtraSamples = @()
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$PreviewClsid = '2C8F1A3D-6B4E-4D9C-A1F2-7E3B5C8D0A46'

if ($UseInstalled) {
    $TargetDir = Join-Path ${env:ProgramFiles} 'SageThumbs2K'
} elseif (-not $TargetDir) {
    throw 'Pass -TargetDir <dir with the built DLL>, or -UseInstalled.'
}
$dll  = Join-Path $TargetDir 'sagethumbs2k.dll'
$st2k = Join-Path $TargetDir 'st2k.exe'
foreach ($required in $dll, $st2k) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) { throw "not found: $required" }
}
if (-not $ArtifactDir) { $ArtifactDir = Join-Path ([IO.Path]::GetTempPath()) "st2k-shell-$PID" }
New-Item -ItemType Directory -Force -Path $ArtifactDir | Out-Null

$script:passed = 0
function Check([string]$name, [scriptblock]$body) {
    try { & $body; Write-Host "  PASS  $name" -ForegroundColor Green; $script:passed++ }
    catch { Write-Host "  FAIL  $name" -ForegroundColor Red; Write-Host "        $($_.Exception.Message)" -ForegroundColor Red; throw }
}

# regsvr32 is a GUI-subsystem executable, so PowerShell does not reliably populate
# $LASTEXITCODE for it. Start it explicitly and read the process's own exit code.
function Invoke-Regsvr32([string]$path, [switch]$Unregister) {
    $regsvrArgs = @('/s')
    if ($Unregister) { $regsvrArgs += '/u' }
    $regsvrArgs += "`"$path`""
    $proc = Start-Process -FilePath "$env:SystemRoot\System32\regsvr32.exe" `
        -ArgumentList $regsvrArgs -PassThru -Wait -WindowStyle Hidden
    if ($proc.ExitCode -ne 0) {
        throw "regsvr32 $(if ($Unregister) {'/u '} else {''})failed with exit code $($proc.ExitCode) for $path"
    }
}

# The COM work runs out-of-process in Windows PowerShell with -STA. See the header of
# _shell-surface-probe.ps1 for why it cannot live in this (PowerShell 7) process.
function Invoke-ShellSurfaceProbe {
    param([string]$Sample, [string]$ThumbnailOut, [string]$PreviewOut)
    $winps = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    if (-not (Test-Path -LiteralPath $winps -PathType Leaf)) {
        throw "Windows PowerShell not found at $winps (needed to host the COM probe)"
    }
    $probe = Join-Path $PSScriptRoot '_shell-surface-probe.ps1'
    $output = & $winps -NoProfile -NonInteractive -STA -ExecutionPolicy Bypass -File $probe `
        -InputPath $Sample -ThumbnailOut $ThumbnailOut -PreviewOut $PreviewOut `
        -PreviewClsid $PreviewClsid 2>&1
    $output | ForEach-Object { Write-Host "        $_" -ForegroundColor DarkGray }
    if ($LASTEXITCODE -ne 0) { throw "shell COM probe failed (exit $LASTEXITCODE)" }
}

# "Not blank" has to mean something stricter than "a file exists": a handler that fails
# quietly still hands back a uniform grey bitmap. Require real colour variety.
function Assert-NonBlankImage([string]$path, [string]$what) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "$what produced no file" }
    Add-Type -AssemblyName System.Drawing
    $bitmap = [System.Drawing.Bitmap]::FromFile($path)
    try {
        $seen = [System.Collections.Generic.HashSet[int]]::new()
        for ($y = 0; $y -lt $bitmap.Height; $y += [Math]::Max(1, [int]($bitmap.Height / 48))) {
            for ($x = 0; $x -lt $bitmap.Width; $x += [Math]::Max(1, [int]($bitmap.Width / 48))) {
                [void]$seen.Add($bitmap.GetPixel($x, $y).ToArgb())
            }
        }
        if ($seen.Count -lt 4) { throw "$what is effectively blank ($($seen.Count) distinct sampled colours)" }
        Write-Host "        $what -> $($bitmap.Width)x$($bitmap.Height), $($seen.Count) distinct sampled colours" -ForegroundColor DarkGray
    } finally { $bitmap.Dispose() }
}

# The colour-variety test above is the right check for a PHOTOGRAPH and the wrong one for a
# sample that is legitimately one flat colour: it called a correct, pure-red ZX Spectrum
# screen "effectively blank" on its first run. A gate that cries wolf gets ignored, so a
# sample with a KNOWN answer is asserted against that answer instead of against variety.
# This is also the stronger check of the two - it fails a handler that returns the wrong
# picture, which colour variety cannot see at all.
function Assert-ImageColour([string]$path, [string]$what, [int[]]$rgb) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "$what produced no file" }
    Add-Type -AssemblyName System.Drawing
    $bitmap = [System.Drawing.Bitmap]::FromFile($path)
    try {
        # Mean of the whole tile, so a resample or a border cannot swing the verdict.
        [long]$r = 0; [long]$g = 0; [long]$b = 0; [int]$n = 0
        for ($y = 0; $y -lt $bitmap.Height; $y += [Math]::Max(1, [int]($bitmap.Height / 32))) {
            for ($x = 0; $x -lt $bitmap.Width; $x += [Math]::Max(1, [int]($bitmap.Width / 32))) {
                $px = $bitmap.GetPixel($x, $y)
                $r += $px.R; $g += $px.G; $b += $px.B; $n++
            }
        }
        $got = @([int]($r / $n), [int]($g / $n), [int]($b / $n))
        $off = 0..2 | ForEach-Object { [Math]::Abs($got[$_] - $rgb[$_]) } | Measure-Object -Maximum
        if ($off.Maximum -gt 24) {
            throw ("$what is the WRONG picture: mean rgb={0} expected {1}" -f ($got -join ','), ($rgb -join ','))
        }
        Write-Host ("        $what -> {0}x{1}, mean rgb={2} (expected {3})" -f $bitmap.Width, $bitmap.Height, ($got -join ','), ($rgb -join ',')) -ForegroundColor DarkGray
    } finally { $bitmap.Dispose() }
}

Write-Host "[shell-surfaces] target: $TargetDir" -ForegroundColor Cyan
Write-Host "[shell-surfaces] artifacts: $ArtifactDir" -ForegroundColor DarkGray
$registered = $false
try {
    if (-not $UseInstalled) {
        Check 'DLL registers through regsvr32' { Invoke-Regsvr32 $dll }
        $registered = $true
    }

    Check 'st2k doctor runs and reports a healthy chain' {
        $report = & $st2k doctor 2>&1 | Out-String
        Write-Host $report -ForegroundColor DarkGray
        # Assert on POSITIVE signals. A negative regex like "not registered" would also
        # match doctor's per-format lines on a machine where formats are simply disabled,
        # which is a legitimate state on a fresh CI runner and not a failure.
        if ($report -notmatch '(?i)registered,\s*loads OK') {
            throw 'doctor reported no coclass as "registered, loads OK" (the LoadLibrary probe is the point of this check)'
        }
        if ($report -notmatch '(?i)decode self-test\s+passed') {
            throw 'doctor decode self-test did not pass'
        }
    }

    # Build the sample with our own CLI, so this needs no test corpus on the runner and
    # simultaneously smoke-tests convert. TGA is chosen because Windows has no codec for
    # it: if a thumbnail comes back, it came back through OUR handler and nothing else.
    $sampleDir = Join-Path $ArtifactDir 'sample'
    New-Item -ItemType Directory -Force -Path $sampleDir | Out-Null
    $seed = Join-Path $sampleDir 'seed.png'
    $sample = Join-Path $sampleDir 'probe.tga'
    Check 'CLI writes a format Windows itself cannot decode' {
        Add-Type -AssemblyName System.Drawing
        $bitmap = New-Object System.Drawing.Bitmap 256, 192
        try {
            for ($y = 0; $y -lt 192; $y++) { for ($x = 0; $x -lt 256; $x++) {
                $bitmap.SetPixel($x, $y, [System.Drawing.Color]::FromArgb(255, $x, $y * 255 / 191, (($x + $y) % 256)))
            } }
            $bitmap.Save($seed, [System.Drawing.Imaging.ImageFormat]::Png)
        } finally { $bitmap.Dispose() }
        & $st2k convert $seed $sample
        if ($LASTEXITCODE -ne 0) { throw "st2k convert failed with exit code $LASTEXITCODE" }
        if (-not (Test-Path -LiteralPath $sample -PathType Leaf)) { throw 'st2k convert wrote no TGA' }
    }

    $thumb = Join-Path $ArtifactDir 'thumbnail.png'
    $prev  = Join-Path $ArtifactDir 'preview.png'
    Check 'shell COM probe runs both surfaces' {
        Invoke-ShellSurfaceProbe -Sample $sample -ThumbnailOut $thumb -PreviewOut $prev
    }
    Check 'Explorer thumbnail path (IShellItemImageFactory) renders it' {
        Assert-NonBlankImage $thumb 'thumbnail'
    }
    Check 'Preview-pane path (registered IPreviewHandler CLSID) renders it' {
        Assert-NonBlankImage $prev 'preview'
    }

    foreach ($entry in $ExtraSamples) {
        # `path` or `path=R,G,B`. With a colour, the tile is checked against THAT rather
        # than against colour variety - see Assert-ImageColour for why that matters.
        $expect = $null
        $spec = $entry
        if ($entry -match '^(.*)=(\d+),(\d+),(\d+)$') {
            $spec = $Matches[1]
            $expect = @([int]$Matches[2], [int]$Matches[3], [int]$Matches[4])
        }
        $extra = (Resolve-Path -LiteralPath $spec).Path
        $leaf = Split-Path $extra -Leaf
        $extraThumb = Join-Path $ArtifactDir "extra-$leaf.png"
        $extraPrev  = Join-Path $ArtifactDir "extra-$leaf.preview.png"
        Check "Explorer thumbnail path renders $leaf" {
            Invoke-ShellSurfaceProbe -Sample $extra -ThumbnailOut $extraThumb -PreviewOut $extraPrev
            if ($expect) { Assert-ImageColour $extraThumb "thumbnail for $leaf" $expect }
            else { Assert-NonBlankImage $extraThumb "thumbnail for $leaf" }
        }
    }
}
finally {
    if ($registered) {
        try { Invoke-Regsvr32 $dll -Unregister; Write-Host '  (unregistered)' -ForegroundColor DarkGray }
        catch { Write-Host "  WARNING: unregister failed: $($_.Exception.Message)" -ForegroundColor Yellow }
    }
}

Write-Host "[shell-surfaces] ALL GREEN ($script:passed checks)" -ForegroundColor Green
