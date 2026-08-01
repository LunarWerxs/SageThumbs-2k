<#
.SYNOPSIS
  Captures an opt-in Explorer small-icons regression image.

.DESCRIPTION
  Copies a small, known set of corpus images to a unique temporary directory,
  opens a dedicated Explorer window for that directory, invokes *that window's*
  Small icons UI-Automation command, and captures only its HWND with PrintWindow.
  It deliberately does not send mouse/keyboard input, alter Explorer defaults,
  or terminate Explorer (or any other owner process).

  The result is intended for a human small-icon regression check after an
  installed shell-extension build.  It does not prove Explorer has loaded a
  particular DLL; use the normal install/hash checks before treating it as a
  release gate.

.EXAMPLE
  pwsh scripts\capture-explorer-small-icons.ps1

.EXAMPLE
  pwsh scripts\capture-explorer-small-icons.ps1 -Keep
  # Keeps the isolated sample directory and PNG for inspection.
#>
[CmdletBinding()]
param(
    # Test corpus is a sibling of the project checkout.
    [string]$CorpusPath = (Join-Path (Split-Path $PSScriptRoot -Parent) '..\test-corpus'),

    # A compact, stable group of ordinary and extension-owned image formats.
    [string[]]$SampleNames = @('sample.png', 'sample.jpg', 'sample.webp', 'sample-epsi.eps', 'sample-avif-alpha.avif'),

    # Explorer exposes this command through UI Automation.  Override only on a
    # non-English Windows installation where its accessible name is localized.
    [string]$SmallIconsLabel = 'Small icons',

    # Explicit destination makes an artifact easy to attach to an issue.  By
    # default it stays alongside the GUID temp directory, so use -Keep when
    # inspecting the default capture after a successful run.
    [string]$OutputPath,

    [switch]$Keep,

    [ValidateRange(5, 60)]
    [int]$TimeoutSeconds = 20
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not ('St2kExplorerSmallIconNative' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class St2kExplorerSmallIconNative {
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }

    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdcBlt, uint flags);
    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool PostMessage(IntPtr hWnd, uint msg, IntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool IsWindow(IntPtr hWnd);
    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool SetWindowPos(IntPtr hWnd, IntPtr hWndInsertAfter, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);

}
'@
}

Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

$root = Split-Path $PSScriptRoot -Parent
$resolvedCorpus = (Resolve-Path -LiteralPath $CorpusPath -ErrorAction Stop).Path
$tempDir = Join-Path ([IO.Path]::GetTempPath()) ("st2k-explorer-small-icons-" + [Guid]::NewGuid().ToString('N'))
$createdHwnd = [IntPtr]::Zero
$capturePath = $null

function Get-ExplorerWindows {
    param([Parameter(Mandatory)] $Shell)

    # Shell.Windows includes browser windows too.  Only Explorer windows expose
    # a ShellFolderView document and are candidates for the location match below.
    @($Shell.Windows()) | Where-Object {
        $_.FullName -and ([IO.Path]::GetFileName($_.FullName) -ieq 'explorer.exe')
    }
}

function Get-ExplorerHwnd {
    param([Parameter(Mandatory)] $Window)
    try { return [IntPtr][int64]$Window.HWND } catch { return [IntPtr]::Zero }
}

function Get-ExplorerLocationPath {
    param([Parameter(Mandatory)] $Window)
    try {
        $url = [string]$Window.LocationURL
        if ([string]::IsNullOrWhiteSpace($url)) { return $null }
        if ($url -notmatch '^file:') { return $null }
        return [Uri]::UnescapeDataString(([Uri]$url).LocalPath).TrimEnd('\')
    } catch {
        return $null
    }
}

function Wait-ForDedicatedExplorerWindow {
    param(
        [Parameter(Mandatory)] $Shell,
        [Parameter(Mandatory)] [string]$ExpectedPath,
        [Parameter(Mandatory)] [datetime]$Deadline
    )

    do {
        foreach ($window in Get-ExplorerWindows $Shell) {
            $actualPath = Get-ExplorerLocationPath $window
            if ($actualPath -and [string]::Equals($actualPath, $ExpectedPath, [StringComparison]::OrdinalIgnoreCase)) {
                $hwnd = Get-ExplorerHwnd $window
                if ($hwnd -ne [IntPtr]::Zero -and [St2kExplorerSmallIconNative]::IsWindow($hwnd)) {
                    return [pscustomobject]@{ Window = $window; Hwnd = $hwnd }
                }
            }
        }
        Start-Sleep -Milliseconds 150
    } while ([datetime]::UtcNow -lt $Deadline)

    throw "Explorer did not open the isolated directory within $TimeoutSeconds seconds."
}

function Set-OnlyThisWindowToSmallIcons {
    param(
        [Parameter(Mandatory)] $ExplorerWindow,
        [Parameter(Mandatory)] [IntPtr]$Hwnd,
        [Parameter(Mandatory)] [string]$Label
    )

    # Win11 accepts IShellFolderViewDual2.CurrentViewMode = FVM_SMALLICON but
    # leaves the actual view at FVM_ICON.  Its command-bar View popup exposes
    # the real per-window command as an accessibility TogglePattern.  Invoking
    # that pattern is semantic UI Automation -- no mouse/keyboard injection,
    # no registry/default-view edit, and no interaction with another window.
    $window = [System.Windows.Automation.AutomationElement]::FromHandle($Hwnd)
    if (-not $window) { throw 'UI Automation could not bind the dedicated Explorer HWND.' }
    $frame = New-Object St2kExplorerSmallIconNative+RECT
    if (-not [St2kExplorerSmallIconNative]::GetWindowRect($Hwnd, [ref]$frame)) {
        throw 'Could not resolve the dedicated Explorer bounds before opening its View menu.'
    }
    $buttonCondition = [System.Windows.Automation.AndCondition]::new(@(
        [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::ControlTypeProperty, [System.Windows.Automation.ControlType]::Button),
        [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::NameProperty, 'View')
    ))
    $viewMatches = [Collections.Generic.List[object]]::new()
    foreach ($candidate in $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, $buttonCondition)) {
        if ($candidate.Current.IsOffscreen) { continue }
        $bounds = $candidate.Current.BoundingRectangle
        $centreX = $bounds.Left + ($bounds.Width / 2)
        $centreY = $bounds.Top + ($bounds.Height / 2)
        if ($centreX -ge $frame.Left -and $centreX -le $frame.Right -and
            $centreY -ge $frame.Top -and $centreY -le $frame.Bottom) {
            $viewMatches.Add($candidate)
        }
    }
    if ($viewMatches.Count -ne 1) {
        throw "Expected exactly one visible View button inside the dedicated Explorer frame; found $($viewMatches.Count)."
    }
    $viewButton = $viewMatches[0]
    try {
        ([System.Windows.Automation.InvokePattern]$viewButton.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)).Invoke()
    } catch {
        throw "Explorer View command-bar button could not be invoked: $($_.Exception.Message)"
    }

    $menuItemCondition = [System.Windows.Automation.PropertyCondition]::new(
        [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
        [System.Windows.Automation.ControlType]::MenuItem
    )
    $deadline = [datetime]::UtcNow.AddSeconds(5)
    $smallIcons = $null
    do {
        # The popup is a separate top-level HWND, so search the automation root
        # only after THIS window opened it.  The exact label avoids touching any
        # unrelated control in the Explorer frame.
        $matches = [Collections.Generic.List[object]]::new()
        foreach ($item in [System.Windows.Automation.AutomationElement]::RootElement.FindAll([System.Windows.Automation.TreeScope]::Descendants, $menuItemCondition)) {
            if ($item.Current.Name -eq $Label -and -not $item.Current.IsOffscreen) {
                $bounds = $item.Current.BoundingRectangle
                $centreX = $bounds.Left + ($bounds.Width / 2)
                $centreY = $bounds.Top + ($bounds.Height / 2)
                if ($centreX -ge $frame.Left -and $centreX -le $frame.Right -and
                    $centreY -ge $frame.Top -and $centreY -le $frame.Bottom) {
                    $matches.Add($item)
                }
            }
        }
        if ($matches.Count -gt 1) {
            throw "More than one visible '$Label' command appeared inside the dedicated Explorer frame."
        }
        $smallIcons = if ($matches.Count -eq 1) { $matches[0] } else { $null }
        if ($smallIcons) { break }
        Start-Sleep -Milliseconds 100
    } while ([datetime]::UtcNow -lt $deadline)
    if (-not $smallIcons) {
        throw "Explorer did not expose the '$Label' View command. On a localized Windows build, pass -SmallIconsLabel with its accessible name."
    }

    try {
        $toggle = [System.Windows.Automation.TogglePattern]$smallIcons.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern)
        if ($toggle.Current.ToggleState -ne [System.Windows.Automation.ToggleState]::On) { $toggle.Toggle() }
    } catch {
        throw "Explorer '$Label' command is not an automation toggle: $($_.Exception.Message)"
    }

    $deadline = [datetime]::UtcNow.AddSeconds(5)
    do {
        $on = $toggle.Current.ToggleState -eq [System.Windows.Automation.ToggleState]::On
        # The command is per-window, but the Shell automation mode provides an
        # independent confirmation that FVM_SMALLICON (2) is now active.
        $mode = [int]$ExplorerWindow.Document.CurrentViewMode
        if ($on -and $mode -eq 2) { return }
        Start-Sleep -Milliseconds 100
    } while ([datetime]::UtcNow -lt $deadline)
    throw "Explorer did not confirm '$Label' as FVM_SMALLICON (toggle=$($toggle.Current.ToggleState), mode=$mode)."
}

function Save-WindowPng {
    param(
        [Parameter(Mandatory)] [IntPtr]$Hwnd,
        [Parameter(Mandatory)] [string]$Path
    )

    $rect = New-Object St2kExplorerSmallIconNative+RECT
    if (-not [St2kExplorerSmallIconNative]::GetWindowRect($Hwnd, [ref]$rect)) {
        throw "GetWindowRect failed (Win32=$([Runtime.InteropServices.Marshal]::GetLastWin32Error()))."
    }
    $width = $rect.Right - $rect.Left
    $height = $rect.Bottom - $rect.Top
    if ($width -lt 640 -or $height -lt 480) {
        throw "Dedicated Explorer window is unexpectedly small (${width}x${height}); refusing an unhelpful capture."
    }

    $bitmap = New-Object System.Drawing.Bitmap($width, $height, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    try {
        $hdc = $graphics.GetHdc()
        try {
            # PW_RENDERFULLCONTENT asks DWM for the complete client content.
            if (-not [St2kExplorerSmallIconNative]::PrintWindow($Hwnd, $hdc, 2)) {
                throw "PrintWindow failed (Win32=$([Runtime.InteropServices.Marshal]::GetLastWin32Error()))."
            }
        } finally {
            $graphics.ReleaseHdc($hdc)
        }
        $bitmap.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
    } finally {
        $graphics.Dispose()
        $bitmap.Dispose()
    }
}

function Test-CapturePng {
    param([Parameter(Mandatory)] [string]$Path)

    if (-not (Test-Path -LiteralPath $Path)) { throw "Capture was not written: $Path" }
    if ((Get-Item -LiteralPath $Path).Length -lt 4096) { throw "Capture is implausibly small: $Path" }

    $bitmap = [System.Drawing.Bitmap]::FromFile($Path)
    try {
        if ($bitmap.Width -lt 640 -or $bitmap.Height -lt 480) {
            throw "Capture dimensions are unexpectedly small: $($bitmap.Width)x$($bitmap.Height)."
        }
        # A blank PrintWindow result is usually solid black/white.  Sample a
        # regular grid and require more than one RGB value; this avoids reading
        # pixels from any other desktop/window surface.
        $colours = [Collections.Generic.HashSet[int]]::new()
        for ($y = 0; $y -lt $bitmap.Height; $y += [Math]::Max(1, [int]($bitmap.Height / 30))) {
            for ($x = 0; $x -lt $bitmap.Width; $x += [Math]::Max(1, [int]($bitmap.Width / 40))) {
                $pixel = $bitmap.GetPixel($x, $y)
                [void]$colours.Add(($pixel.R -shl 16) -bor ($pixel.G -shl 8) -bor $pixel.B)
            }
        }
        if ($colours.Count -lt 2) { throw 'Capture is blank (sampled as one solid colour).' }
    } finally {
        $bitmap.Dispose()
    }
}

try {
    New-Item -ItemType Directory -Path $tempDir -Force | Out-Null
    $copied = 0
    foreach ($name in $SampleNames) {
        if ([IO.Path]::GetFileName($name) -ne $name) {
            throw "SampleNames must be corpus file names, not paths: '$name'"
        }
        $source = Join-Path $resolvedCorpus $name
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "Known corpus sample is missing: $source"
        }
        Copy-Item -LiteralPath $source -Destination (Join-Path $tempDir $name) -Force
        $copied++
    }
    if ($copied -eq 0) { throw 'No corpus samples were selected.' }

    if ($OutputPath) {
        $capturePath = [IO.Path]::GetFullPath($OutputPath)
        $parent = Split-Path -Parent $capturePath
        if (-not (Test-Path -LiteralPath $parent)) { throw "Output directory does not exist: $parent" }
    } else {
        $capturePath = Join-Path $tempDir 'explorer-small-icons.png'
    }

    $shell = New-Object -ComObject Shell.Application
    try {
        # Explore creates an independent file-manager window.  We locate it by
        # its unique path rather than process id, since existing Explorer windows
        # commonly share explorer.exe's process.
        $shell.Explore($tempDir)
        $target = Wait-ForDedicatedExplorerWindow $shell $tempDir ([datetime]::UtcNow.AddSeconds($TimeoutSeconds))
        $createdHwnd = $target.Hwnd

        # A predictable capture rectangle, without activation, z-order changes,
        # desktop input, or a change to any global Explorer preference.
        [void][St2kExplorerSmallIconNative]::SetWindowPos($createdHwnd, [IntPtr]::Zero, 80, 80, 1100, 760, 0x0014) # SWP_NOZORDER | SWP_NOACTIVATE
        [void][St2kExplorerSmallIconNative]::ShowWindow($createdHwnd, 4) # SW_SHOWNOACTIVATE
        Set-OnlyThisWindowToSmallIcons $target.Window $createdHwnd $SmallIconsLabel
        Start-Sleep -Milliseconds 750
        Save-WindowPng $createdHwnd $capturePath
        Test-CapturePng $capturePath
    } finally {
        # Only the HWND proven to be the GUID directory window is asked to close.
        # Do not call taskkill, Stop-Process, or Shell.Quit(): Explorer owns other
        # user windows and may host unrelated owner applications/extensions.
        if ($createdHwnd -ne [IntPtr]::Zero -and [St2kExplorerSmallIconNative]::IsWindow($createdHwnd)) {
            [void][St2kExplorerSmallIconNative]::PostMessage($createdHwnd, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) # WM_CLOSE
        }
        [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell)
    }

    Write-Host "[explorer-small-icons] PASS  $capturePath" -ForegroundColor Green
    if ($Keep) { Write-Host "[explorer-small-icons] kept sample directory  $tempDir" }
} finally {
    if (-not $Keep -and (Test-Path -LiteralPath $tempDir)) {
        Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
