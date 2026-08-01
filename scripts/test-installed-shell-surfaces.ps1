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
#>
[CmdletBinding()]
param(
    # Directory holding the freshly built sagethumbs2k.dll + st2k.exe to register and probe.
    [string]$TargetDir,
    # Probe the already-installed machine-wide copy instead. Registers and unregisters
    # NOTHING, so it is safe to run on a working desktop.
    [switch]$UseInstalled,
    # Where to drop the rendered proofs. Handy as a CI artifact when something fails.
    [string]$ArtifactDir
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
}
finally {
    if ($registered) {
        try { Invoke-Regsvr32 $dll -Unregister; Write-Host '  (unregistered)' -ForegroundColor DarkGray }
        catch { Write-Host "  WARNING: unregister failed: $($_.Exception.Message)" -ForegroundColor Yellow }
    }
}

Write-Host "[shell-surfaces] ALL GREEN ($script:passed checks)" -ForegroundColor Green
