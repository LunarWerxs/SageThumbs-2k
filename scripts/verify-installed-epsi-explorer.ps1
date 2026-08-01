<#
.SYNOPSIS
  Verifies SageThumbs 2K's installed EPS thumbnail and Preview-handler surfaces.

.DESCRIPTION
  Uses a GUID-named copy of test-corpus\sample-epsi.eps so Windows cannot reuse
  a thumbnail-cache entry.  It asks the Windows shell image factory for the
  thumbnail, then separately proves the registered PreviewHandler CLSID in an
  owned off-screen host.  Finally, it opens a GUID-only Explorer window,
  enables Preview pane through that window's UI Automation view menu, selects
  the EPS, captures Explorer, and verifies the actual prevhost module.  It
  restores the window's initial Preview-pane state and closes only that window.

.EXAMPLE
  powershell.exe -NoProfile -Sta -File scripts\verify-installed-epsi-explorer.ps1 -Keep
#>
[CmdletBinding()]
param(
    [string]$CorpusPath,
    # Some applications (for example Adobe Illustrator) own the higher-priority
    # bare .eps PreviewHandler slot.  This opt-in creates a current-user overlay
    # for the duration of the Explorer proof, simulating a clean machine without
    # changing or deleting the foreign machine-wide registration.
    [switch]$TemporarilyShadowForeignPreviewHandler,
    [switch]$Keep
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not $CorpusPath) {
    $CorpusPath = Join-Path (Split-Path $PSScriptRoot -Parent) '..\test-corpus'
}

if ([Threading.Thread]::CurrentThread.GetApartmentState() -ne [Threading.ApartmentState]::STA) {
    throw 'Run under Windows PowerShell with -Sta so the preview handler is hosted like Explorer.'
}

$source = Join-Path (Resolve-Path -LiteralPath $CorpusPath).Path 'sample-epsi.eps'
if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "Missing corpus sample: $source" }
$tempDir = Join-Path ([IO.Path]::GetTempPath()) ('st2k-epsi-installed-' + [Guid]::NewGuid().ToString('N'))
$thumbnailClsid = '{7B2E6A14-9C3D-4F8A-B1E7-2A5D9F0C6E31}'
$previewClsid = '{2C8F1A3D-6B4E-4D9C-A1F2-7E3B5C8D0A46}'
$previewCategory = '{8895b1c6-b41f-4c1c-a562-0d564250836f}'
$expectedDll = Join-Path $env:ProgramFiles 'SageThumbs2K\sagethumbs2k.dll'
$userExtensionPath = 'Registry::HKEY_CURRENT_USER\Software\Classes\.eps'
$userShellexPath = "$userExtensionPath\shellex"
$userPreviewPath = "$userShellexPath\$previewCategory"
$script:shadowState = $null

function Get-RegistryDefaultValue([string]$Path) {
    $key = Get-Item -LiteralPath $Path -ErrorAction Stop
    return [string]$key.GetValue('')
}

function Get-UserExtensionRegistrySnapshot {
    $lines = @(& reg.exe query 'HKCU\Software\Classes\.eps' /s 2>&1 | ForEach-Object { $_.ToString() })
    [pscustomobject]@{
        ExitCode = $LASTEXITCODE
        Text = $lines -join "`n"
    }
}

foreach ($registration in @(
    @{ Name = 'EPS thumbnail SystemFileAssociations slot'; Path = 'Registry::HKEY_CLASSES_ROOT\SystemFileAssociations\.eps\shellex\{E357FCCD-A995-4576-B01F-234630154E96}'; Expected = $thumbnailClsid },
    @{ Name = 'EPS preview SystemFileAssociations slot'; Path = 'Registry::HKEY_CLASSES_ROOT\SystemFileAssociations\.eps\shellex\{8895b1c6-b41f-4c1c-a562-0d564250836f}'; Expected = $previewClsid },
    @{ Name = 'thumbnail COM server'; Path = "Registry::HKEY_CLASSES_ROOT\CLSID\$thumbnailClsid\InprocServer32"; Expected = $expectedDll },
    @{ Name = 'preview COM server'; Path = "Registry::HKEY_CLASSES_ROOT\CLSID\$previewClsid\InprocServer32"; Expected = $expectedDll }
)) {
    $actual = Get-RegistryDefaultValue $registration.Path
    if (-not [string]::Equals($actual, $registration.Expected, [StringComparison]::OrdinalIgnoreCase)) {
        throw "$($registration.Name) is not the installed SageThumbs 2K handler: '$actual'"
    }
}
$previewList = Get-Item -LiteralPath 'Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\PreviewHandlers' -ErrorAction Stop
if ([string]::IsNullOrWhiteSpace([string]$previewList.GetValue($previewClsid))) {
    throw "PreviewHandlers does not list $previewClsid"
}

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
if (-not ('St2kInstalledEpsiProbe' -as [type])) {
    Add-Type -ReferencedAssemblies System.Windows.Forms,System.Drawing @'
using System;
using System.Collections.Generic;
using System.Drawing;
using System.Drawing.Imaging;
using System.Linq;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;
using System.Threading;
using System.Windows.Forms;

public static class St2kInstalledEpsiProbe {
    static Form host; static Thread hostThread; static AutoResetEvent hostReady;
    delegate bool EnumWindowsProc(IntPtr hwnd, IntPtr lParam);
    [StructLayout(LayoutKind.Sequential)] public struct SIZE { public int cx, cy; public SIZE(int x, int y) { cx=x; cy=y; } }
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int left, top, right, bottom; }
    [ComImport, Guid("bcc18b79-ba16-442f-80c4-8a59c30c463b"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IShellItemImageFactory { void GetImage(SIZE size, uint flags, out IntPtr bitmap); }
    [ComImport, Guid("B824B49D-22AC-4161-AC8A-9916E8FA3F7F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IInitializeWithStream { void Initialize(IStream stream, uint mode); }
    [ComImport, Guid("8895b1c6-b41f-4c1c-a562-0d564250836f"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IPreviewHandler { void SetWindow(IntPtr hwnd, ref RECT rect); void SetRect(ref RECT rect); void DoPreview(); void Unload(); void SetFocus(); void QueryFocus(out IntPtr hwnd); void TranslateAccelerator(IntPtr msg); }
    [DllImport("shell32.dll", CharSet=CharSet.Unicode, PreserveSig=false)] static extern void SHCreateItemFromParsingName(string path, IntPtr bindCtx, ref Guid iid, [MarshalAs(UnmanagedType.Interface)] out IShellItemImageFactory item);
    [DllImport("ole32.dll", PreserveSig=false)] static extern void CreateStreamOnHGlobal(IntPtr memory, bool deleteOnRelease, out IStream stream);
    [DllImport("gdi32.dll")] static extern bool DeleteObject(IntPtr obj);
    [DllImport("user32.dll", SetLastError=true)] static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);
    [DllImport("user32.dll", SetLastError=true)] static extern bool PostMessage(IntPtr hwnd, uint message, IntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll")] static extern bool EnumChildWindows(IntPtr parent, EnumWindowsProc callback, IntPtr lParam);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);
    [DllImport("shell32.dll")] static extern void SHChangeNotify(uint eventId, uint flags, IntPtr item1, IntPtr item2);
    static void Capture(IntPtr hwnd, string path) {
        RECT r; GetWindowRect(hwnd, out r); int w=r.right-r.left, h=r.bottom-r.top;
        using (Bitmap bitmap = new Bitmap(w, h, PixelFormat.Format32bppArgb)) using (Graphics graphics = Graphics.FromImage(bitmap)) {
            IntPtr hdc=graphics.GetHdc(); try { if (!PrintWindow(hwnd, hdc, 2)) throw new System.ComponentModel.Win32Exception(); } finally { graphics.ReleaseHdc(hdc); }
            bitmap.Save(path, ImageFormat.Png);
        }
    }
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
    public static void Thumbnail(string input, string output) {
        Guid iid=typeof(IShellItemImageFactory).GUID; IShellItemImageFactory item; SHCreateItemFromParsingName(input, IntPtr.Zero, ref iid, out item);
        IntPtr h; item.GetImage(new SIZE(512,512), 9, out h); if (h==IntPtr.Zero) throw new COMException("Shell returned no thumbnail.");
        try { using(Image image=Image.FromHbitmap(h)) image.Save(output, ImageFormat.Png); } finally { DeleteObject(h); Marshal.ReleaseComObject(item); }
    }
    public static void CaptureWindow(IntPtr hwnd, string output) { Capture(hwnd, output); }
    public static void CloseWindow(IntPtr hwnd) { PostMessage(hwnd, 0x0010, IntPtr.Zero, IntPtr.Zero); }
    public static int[] GetDescendantProcessIds(IntPtr hwnd) {
        HashSet<int> ids=new HashSet<int>();
        EnumChildWindows(hwnd, delegate(IntPtr child, IntPtr unused) { uint pid; GetWindowThreadProcessId(child,out pid); if(pid!=0) ids.Add((int)pid); return true; }, IntPtr.Zero);
        return ids.OrderBy(id => id).ToArray();
    }
    public static void NotifyAssociations() { SHChangeNotify(0x08000000, 0, IntPtr.Zero, IntPtr.Zero); }
    public static void Preview(string input, string output) {
        IPreviewHandler preview=null; string stage="create host";
        try {
            StartHost();
            stage="activate registered CLSID"; object obj=Activator.CreateInstance(Type.GetTypeFromCLSID(new Guid("2C8F1A3D-6B4E-4D9C-A1F2-7E3B5C8D0A46")));
            stage="initialize stream"; ((IInitializeWithStream)obj).Initialize(Open(input),0); preview=(IPreviewHandler)obj;
            stage="set preview window"; RECT rect=new RECT { left=0, top=0, right=host.ClientSize.Width, bottom=host.ClientSize.Height }; preview.SetWindow(host.Handle,ref rect);
            stage="render preview"; preview.DoPreview();
            for(int i=0;i<30;i++) { Application.DoEvents(); System.Threading.Thread.Sleep(100); }
            stage="capture preview"; Capture(host.Handle,output); preview.Unload();
        } catch(Exception ex) { throw new InvalidOperationException(stage + ": " + ex.ToString(), ex);
        } finally { if(preview!=null) Marshal.ReleaseComObject(preview); StopHost(); }
    }
    static void StartHost() {
        hostReady=new AutoResetEvent(false);
        hostThread=new Thread(delegate() { host=new Form(); host.ClientSize=new Size(512,384); host.StartPosition=FormStartPosition.Manual; host.Location=new Point(-3000,-3000); host.ShowInTaskbar=false; host.Show(); hostReady.Set(); Application.Run(host); });
        hostThread.SetApartmentState(ApartmentState.STA); hostThread.Start();
        if(!hostReady.WaitOne(5000)) throw new TimeoutException("Preview host did not create its pumping window.");
    }
    static void StopHost() { if(host!=null && host.IsHandleCreated) host.BeginInvoke((Action)delegate { host.Close(); }); if(hostThread!=null) hostThread.Join(5000); host=null; hostThread=null; }
    static IStream Open(string path) { byte[] bytes=System.IO.File.ReadAllBytes(path); IntPtr memory=Marshal.AllocHGlobal(bytes.Length); Marshal.Copy(bytes,0,memory,bytes.Length); IStream stream; try { CreateStreamOnHGlobal(memory,true,out stream); memory=IntPtr.Zero; return stream; } finally { if(memory!=IntPtr.Zero) Marshal.FreeHGlobal(memory); } }
}
'@
}

function Enable-TemporaryPreviewShadow {
    $snapshot = Get-UserExtensionRegistrySnapshot
    $state = [ordered]@{
        ExtensionExisted = Test-Path -LiteralPath $userExtensionPath
        ShellexExisted = Test-Path -LiteralPath $userShellexPath
        PreviewKeyExisted = Test-Path -LiteralPath $userPreviewPath
        DefaultExisted = $false
        DefaultValue = $null
        DefaultKind = $null
        SnapshotExitCode = $snapshot.ExitCode
        SnapshotText = $snapshot.Text
    }
    if ($state.PreviewKeyExisted) {
        $existing = Get-Item -LiteralPath $userPreviewPath -ErrorAction Stop
        $state.DefaultExisted = @($existing.GetValueNames()) -contains ''
        if ($state.DefaultExisted) {
            $state.DefaultValue = $existing.GetValue('', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            $state.DefaultKind = $existing.GetValueKind('')
        }
    }
    # Publish the rollback state before the first mutation so the outer finally
    # can restore a partially-created shadow if any later assertion fails.
    $script:shadowState = [pscustomobject]$state
    $key = New-Item -Path $userPreviewPath -Force
    $key.SetValue('', $previewClsid, [Microsoft.Win32.RegistryValueKind]::String)
    [St2kInstalledEpsiProbe]::NotifyAssociations()
    Start-Sleep -Milliseconds 500
    $effective = Get-RegistryDefaultValue "Registry::HKEY_CLASSES_ROOT\.eps\shellex\$previewCategory"
    if (-not [string]::Equals($effective, $previewClsid, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Current-user preview shadow did not become effective: '$effective'"
    }
    return $script:shadowState
}

function Remove-EmptyRegistryKey([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return }
    $key = Get-Item -LiteralPath $Path
    if ($key.GetValueNames().Count -eq 0 -and $key.GetSubKeyNames().Count -eq 0) {
        Remove-Item -LiteralPath $Path -Force
    }
}

function Restore-TemporaryPreviewShadow([pscustomobject]$State) {
    if ($State.PreviewKeyExisted) {
        $key = Get-Item -LiteralPath $userPreviewPath -ErrorAction Stop
        if ($State.DefaultExisted) {
            $key.SetValue('', $State.DefaultValue, $State.DefaultKind)
        } else {
            $key.DeleteValue('', $false)
        }
    } elseif (Test-Path -LiteralPath $userPreviewPath) {
        Remove-Item -LiteralPath $userPreviewPath -Recurse -Force
    }
    if (-not $State.ShellexExisted) { Remove-EmptyRegistryKey $userShellexPath }
    if (-not $State.ExtensionExisted) { Remove-EmptyRegistryKey $userExtensionPath }
    [St2kInstalledEpsiProbe]::NotifyAssociations()
    Start-Sleep -Milliseconds 500
    $after = Get-UserExtensionRegistrySnapshot
    if ($after.ExitCode -ne $State.SnapshotExitCode -or
        -not [string]::Equals($after.Text, $State.SnapshotText, [StringComparison]::Ordinal)) {
        throw 'The current-user .eps association subtree was not restored exactly.'
    }
}

Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

function Wait-DedicatedExplorerWindow {
    param([Parameter(Mandatory)] $Shell, [Parameter(Mandatory)] [string]$Path)
    $deadline = [datetime]::UtcNow.AddSeconds(20)
    do {
        foreach ($window in @($Shell.Windows())) {
            try {
                if ($window.FullName -and ([IO.Path]::GetFileName($window.FullName) -ieq 'explorer.exe') -and
                    ([string]::Equals(([Uri]$window.LocationURL).LocalPath.TrimEnd('\'), $Path, [StringComparison]::OrdinalIgnoreCase))) {
                    return $window
                }
            } catch { }
        }
        Start-Sleep -Milliseconds 150
    } while ([datetime]::UtcNow -lt $deadline)
    throw "Explorer did not open the isolated directory: $Path"
}

function Get-UiaElementByName {
    param([Parameter(Mandatory)] $Root, [Parameter(Mandatory)] [string]$Name, [Windows.Automation.TreeScope]$Scope = [Windows.Automation.TreeScope]::Descendants)
    $condition = New-Object Windows.Automation.PropertyCondition([Windows.Automation.AutomationElement]::NameProperty, $Name)
    $element = $Root.FindFirst($Scope, $condition)
    if (-not $element) { throw "UI Automation did not find '$Name'." }
    return $element
}

function Set-ExplorerPreviewPane {
    param([Parameter(Mandatory)] $ExplorerRoot, [Parameter(Mandatory)] [bool]$Enabled)
    $frame = $ExplorerRoot.Current.BoundingRectangle
    $viewCondition = [Windows.Automation.AndCondition]::new(@(
        [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::ControlTypeProperty, [Windows.Automation.ControlType]::Button),
        [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, 'View')
    ))
    $viewMatches = [Collections.Generic.List[object]]::new()
    foreach ($candidate in $ExplorerRoot.FindAll([Windows.Automation.TreeScope]::Descendants, $viewCondition)) {
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
    $viewMatches[0].GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    # The Windows 11 flyout is a desktop popup, not a child of Explorer's HWND.
    # Search the desktop only after invoking this exact window, scope the visible
    # toggle to its screen bounds, and fail closed on ambiguity. No keyboard,
    # mouse, registry, or global Explorer preference is used.
    $previewCondition = [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, 'Preview pane')
    $deadline = [datetime]::UtcNow.AddSeconds(5)
    $toggle = $null
    do {
        $matches = [Collections.Generic.List[object]]::new()
        foreach ($candidate in [Windows.Automation.AutomationElement]::RootElement.FindAll([Windows.Automation.TreeScope]::Descendants, $previewCondition)) {
            if ($candidate.Current.IsOffscreen) { continue }
            $bounds = $candidate.Current.BoundingRectangle
            $centreX = $bounds.Left + ($bounds.Width / 2)
            $centreY = $bounds.Top + ($bounds.Height / 2)
            if ($centreX -lt $frame.Left -or $centreX -gt $frame.Right -or
                $centreY -lt $frame.Top -or $centreY -gt $frame.Bottom) { continue }
            try {
                $candidateToggle = $candidate.GetCurrentPattern([Windows.Automation.TogglePattern]::Pattern)
                $matches.Add([pscustomobject]@{ Element = $candidate; Toggle = $candidateToggle })
            } catch { }
        }
        if ($matches.Count -gt 1) {
            throw "More than one visible Preview pane toggle appeared inside the dedicated Explorer frame."
        }
        if ($matches.Count -eq 1) { $toggle = $matches[0].Toggle; break }
        Start-Sleep -Milliseconds 100
    } while ([datetime]::UtcNow -lt $deadline)
    if (-not $toggle) { throw 'Explorer did not expose a unique Preview pane toggle for the dedicated window.' }
    $isEnabled = $toggle.Current.ToggleState -eq [Windows.Automation.ToggleState]::On
    if ($isEnabled -ne $Enabled) { $toggle.Toggle() }
    Start-Sleep -Milliseconds 400
    return $isEnabled
}

function Get-LoadedPreviewModules {
    param([Parameter(Mandatory)] [int[]]$ProcessIds)
    foreach ($processId in $ProcessIds) {
        $process = Get-Process -Id $processId -ErrorAction SilentlyContinue
        if (-not $process -or $process.ProcessName -ine 'prevhost') { continue }
        try {
            $process.Modules | Where-Object { $_.FileName -match '(?i)(sagethumbs2k|previewhandler|illustrator|adobe)' } |
                Select-Object @{ Name = 'ProcessId'; Expression = { $process.Id } }, ModuleName, FileName
        } catch {
            Write-Warning "Unable to inspect dedicated prevhost.exe PID $processId`: $($_.Exception.Message)"
        }
    }
}

$bodyError = $null
$cleanupFailures = [Collections.Generic.List[string]]::new()
$resultHashes = $null
try {
    New-Item -ItemType Directory -Path $tempDir | Out-Null
    $sample = Join-Path $tempDir 'sample-epsi.eps'
    $thumbnail = Join-Path $tempDir 'thumbnail.png'
    $directPreview = Join-Path $tempDir 'direct-handler-preview.png'
    $explorerPreview = Join-Path $tempDir 'explorer-preview-pane.png'
    Copy-Item -LiteralPath $source -Destination $sample
    [St2kInstalledEpsiProbe]::Thumbnail($sample, $thumbnail)
    # This is a direct COM-server check.  The Explorer check below separately
    # proves which handler Windows selected for this file association.
    [St2kInstalledEpsiProbe]::Preview($sample, $directPreview)
    foreach ($path in @($thumbnail, $directPreview)) {
        $image = [Drawing.Bitmap]::FromFile($path)
        try {
            if ($image.Width -lt 64 -or $image.Height -lt 64 -or (Get-Item -LiteralPath $path).Length -lt 1024) { throw "Implausible capture: $path" }
            $colours = [Collections.Generic.HashSet[int]]::new()
            for ($y = 0; $y -lt $image.Height; $y += [Math]::Max(1, [int]($image.Height / 20))) {
                for ($x = 0; $x -lt $image.Width; $x += [Math]::Max(1, [int]($image.Width / 20))) {
                    $pixel = $image.GetPixel($x, $y)
                    [void]$colours.Add(($pixel.R -shl 16) -bor ($pixel.G -shl 8) -bor $pixel.B)
                }
            }
            if ($colours.Count -lt 4) { throw "Capture is blank or implausibly uniform: $path" }
        } finally { $image.Dispose() }
    }

    $effectivePreview = Get-RegistryDefaultValue "Registry::HKEY_CLASSES_ROOT\.eps\shellex\$previewCategory"
    if (-not [string]::Equals($effectivePreview, $previewClsid, [StringComparison]::OrdinalIgnoreCase)) {
        if (-not $TemporarilyShadowForeignPreviewHandler) {
            throw "A foreign higher-priority .eps PreviewHandler owns the Explorer surface: '$effectivePreview'. Re-run with -TemporarilyShadowForeignPreviewHandler to simulate a clean machine transactionally."
        }
        [void](Enable-TemporaryPreviewShadow)
        Write-Host '[installed-epsi] using temporary current-user .eps preview shadow (clean-machine simulation)' -ForegroundColor Yellow
    }

    $shell = New-Object -ComObject Shell.Application
    $explorerHwnd = [IntPtr]::Zero
    $previewWasEnabled = $null
    try {
        $shell.Explore($tempDir)
        $explorer = Wait-DedicatedExplorerWindow $shell $tempDir
        $explorerHwnd = [IntPtr][int64]$explorer.HWND
        $root = [Windows.Automation.AutomationElement]::FromHandle($explorerHwnd)
        $previewWasEnabled = Set-ExplorerPreviewPane $root $true
        $baselineDescendantPids = @([St2kInstalledEpsiProbe]::GetDescendantProcessIds($explorerHwnd))
        $item = Get-UiaElementByName $root 'sample-epsi.eps'
        $item.GetCurrentPattern([Windows.Automation.SelectionItemPattern]::Pattern).Select()
        $deadline = [datetime]::UtcNow.AddSeconds(10)
        $dedicatedDescendantPids = @()
        $loadedModules = @()
        do {
            $dedicatedDescendantPids = @([St2kInstalledEpsiProbe]::GetDescendantProcessIds($explorerHwnd))
            $loadedModules = @(Get-LoadedPreviewModules -ProcessIds $dedicatedDescendantPids)
            if ($loadedModules | Where-Object { [string]::Equals($_.FileName, $expectedDll, [StringComparison]::OrdinalIgnoreCase) }) { break }
            Start-Sleep -Milliseconds 150
        } while ([datetime]::UtcNow -lt $deadline)
        $loadedModules | Format-Table -AutoSize | Out-Host
        if (-not ($loadedModules | Where-Object { [string]::Equals($_.FileName, $expectedDll, [StringComparison]::OrdinalIgnoreCase) })) {
            $actual = ($loadedModules | ForEach-Object { "$($_.ProcessId):$($_.FileName)" }) -join '; '
            throw "The dedicated Explorer Preview pane did not load SageThumbs2K. Baseline descendant PIDs: $($baselineDescendantPids -join ','); current descendant PIDs: $($dedicatedDescendantPids -join ','); relevant modules: $actual"
        }
        Start-Sleep -Milliseconds 500
        [St2kInstalledEpsiProbe]::CaptureWindow($explorerHwnd, $explorerPreview)
    } finally {
        if ($null -ne $previewWasEnabled -and $explorerHwnd -ne [IntPtr]::Zero) {
            try { [void](Set-ExplorerPreviewPane ([Windows.Automation.AutomationElement]::FromHandle($explorerHwnd)) ([bool]$previewWasEnabled)) } catch { Write-Warning "Could not restore this dedicated window's Preview pane state: $($_.Exception.Message)" }
        }
        if ($explorerHwnd -ne [IntPtr]::Zero) { [St2kInstalledEpsiProbe]::CloseWindow($explorerHwnd) }
        if ($shell) { [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell) }
    }
    $resultHashes = @(Get-FileHash -Algorithm SHA256 -LiteralPath $thumbnail, $directPreview, $explorerPreview | Select-Object Path, Hash)
} catch {
    $bodyError = $_
} finally {
    if ($null -ne $script:shadowState) {
        try {
            Restore-TemporaryPreviewShadow $script:shadowState
            Write-Host '[installed-epsi] restored the exact pre-existing current-user .eps association subtree' -ForegroundColor DarkGray
        } catch {
            $cleanupFailures.Add("registry restore: $($_.Exception.Message)")
        }
    }
    if (-not $Keep -and (Test-Path -LiteralPath $tempDir)) {
        try { Remove-Item -LiteralPath $tempDir -Recurse -Force }
        catch { $cleanupFailures.Add("temporary directory cleanup: $($_.Exception.Message)") }
    }
}
if ($null -ne $bodyError) {
    foreach ($failure in $cleanupFailures) { Write-Warning "Additional cleanup failure: $failure" }
    throw $bodyError
}
if ($cleanupFailures.Count -gt 0) { throw ($cleanupFailures -join '; ') }
Write-Host "[installed-epsi] PASS  $tempDir" -ForegroundColor Green
$resultHashes
