<#
  The COM half of test-installed-shell-surfaces.ps1. Runs under WINDOWS PowerShell
  (5.1, .NET Framework), launched with -STA. Both of those are deliberate:

  * .NET Framework compiles this against the real System.Drawing / System.Windows.Forms.
    Under PowerShell 7 (.NET 10) the same Add-Type cannot be satisfied at all: passing
    -ReferencedAssemblies drops the default reference set, and type-forwarding then
    scatters Thread, AutoResetEvent, Image and Win32Exception across assemblies that end
    at System.Private.Windows.GdiPlus, which is not a referenceable ref assembly.
  * -STA gives the process an apartment the shell COM objects can be created in.
    It does NOT remove the need for the separate pumping host thread; see the comment
    on StartHost() for the deadlock that one exists to avoid.

  Windows PowerShell is in-box on Windows 11, including ARM64, so this costs no install
  on a GitHub-hosted runner. Called only by test-installed-shell-surfaces.ps1; not a
  standalone entry point.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$InputPath,
    [Parameter(Mandatory)][string]$ThumbnailOut,
    [Parameter(Mandatory)][string]$PreviewOut,
    [Parameter(Mandatory)][string]$PreviewClsid,
    # Which surfaces to exercise. Split so a hang can be attributed to one of them
    # instead of to "the probe".
    [ValidateSet('thumbnail', 'preview')][string[]]$Surfaces = @('thumbnail', 'preview')
)

$ErrorActionPreference = 'Stop'

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type -ReferencedAssemblies System.Windows.Forms, System.Drawing @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;
using System.Threading;
using System.Windows.Forms;

public static class St2kShellSurfaceProbe {
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
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);

    // SHTIF flags 9 = EXTRACTDONOTCACHE | RESIZETOFIT: force a real extraction rather
    // than letting the shell hand back whatever it cached for this path earlier.
    public static void Thumbnail(string input, string output) {
        Guid iid = typeof(IShellItemImageFactory).GUID; IShellItemImageFactory item;
        SHCreateItemFromParsingName(input, IntPtr.Zero, ref iid, out item);
        IntPtr h; item.GetImage(new SIZE(512, 512), 9, out h);
        if (h == IntPtr.Zero) throw new COMException("shell returned no thumbnail");
        try { using (Image image = Image.FromHbitmap(h)) image.Save(output, ImageFormat.Png); }
        finally { DeleteObject(h); Marshal.ReleaseComObject(item); }
    }

    static void Trace(string message) { Console.Out.WriteLine("probe:   " + message); Console.Out.Flush(); }

    static Form host;
    static Thread hostThread;
    static AutoResetEvent hostReady;

    // The host window MUST live on its own STA thread running a real message loop, and
    // DoPreview MUST be called from a different thread. IPreviewHandler::DoPreview
    // parents the handler's own child window onto the HWND we hand it, and window
    // creation sends messages to the OWNING thread. Call DoPreview from the same thread
    // that owns the window and it deadlocks: that thread is blocked inside the call and
    // can never dispatch the messages the call is waiting on. (Verified 2026-08-01:
    // a single-threaded version hangs in DoPreview forever and never returns.)
    // Application.DoEvents() does not save you, because it never runs while blocked.
    static void StartHost() {
        hostReady = new AutoResetEvent(false);
        hostThread = new Thread(delegate() {
            host = new Form();
            host.ClientSize = new Size(512, 384);
            host.StartPosition = FormStartPosition.Manual;
            // Parked off-screen rather than hidden: PrintWindow renders nothing for a
            // hidden or zero-size window, but renders fully for a real window sitting
            // outside the virtual desktop.
            host.Location = new Point(-3000, -3000);
            host.ShowInTaskbar = false;
            host.Show();
            hostReady.Set();
            Application.Run(host);
        });
        hostThread.SetApartmentState(ApartmentState.STA);
        hostThread.IsBackground = true;
        hostThread.Start();
        if (!hostReady.WaitOne(15000)) throw new TimeoutException("preview host never created its window");
    }
    static void StopHost() {
        if (host != null && host.IsHandleCreated) {
            try { host.BeginInvoke((Action)delegate { host.Close(); }); } catch { }
        }
        if (hostThread != null) hostThread.Join(10000);
        host = null; hostThread = null;
    }

    public static void Preview(string clsid, string input, string output) {
        IPreviewHandler preview = null; string stage = "create host";
        try {
            Trace("starting pumping host thread");
            StartHost();
            Trace("host window shown, hwnd=" + host.Handle.ToString("X"));
            stage = "activate registered CLSID";
            Trace("CoCreateInstance on " + clsid);
            object obj = Activator.CreateInstance(Type.GetTypeFromCLSID(new Guid(clsid)));
            stage = "initialize stream"; ((IInitializeWithStream)obj).Initialize(Open(input), 0);
            preview = (IPreviewHandler)obj;
            stage = "set preview window";
            RECT rect = new RECT { left = 0, top = 0, right = 512, bottom = 384 };
            preview.SetWindow(host.Handle, ref rect);
            Trace("DoPreview");
            stage = "render preview"; preview.DoPreview();
            Trace("DoPreview returned");
            // Let the handler finish its first paint before we grab the pixels.
            Thread.Sleep(2500);
            stage = "capture preview"; Capture(host.Handle, output);
            Trace("captured; unloading"); preview.Unload(); Trace("unloaded");
        } catch (Exception ex) {
            throw new InvalidOperationException(stage + ": " + ex.Message, ex);
        } finally {
            if (preview != null) Marshal.ReleaseComObject(preview);
            StopHost();
        }
    }

    static void Capture(IntPtr hwnd, string path) {
        RECT r; GetWindowRect(hwnd, out r); int w = r.right - r.left, h = r.bottom - r.top;
        using (Bitmap bitmap = new Bitmap(w, h, PixelFormat.Format32bppArgb))
        using (Graphics graphics = Graphics.FromImage(bitmap)) {
            IntPtr hdc = graphics.GetHdc();
            try { if (!PrintWindow(hwnd, hdc, 2)) throw new System.ComponentModel.Win32Exception(); }
            finally { graphics.ReleaseHdc(hdc); }
            bitmap.Save(path, ImageFormat.Png);
        }
    }
    static IStream Open(string path) {
        byte[] bytes = System.IO.File.ReadAllBytes(path);
        IntPtr memory = Marshal.AllocHGlobal(bytes.Length);
        Marshal.Copy(bytes, 0, memory, bytes.Length);
        IStream stream;
        try { CreateStreamOnHGlobal(memory, true, out stream); memory = IntPtr.Zero; return stream; }
        finally { if (memory != IntPtr.Zero) Marshal.FreeHGlobal(memory); }
    }
}
'@

if ([System.Threading.Thread]::CurrentThread.GetApartmentState() -ne 'STA') {
    throw 'probe must run in an STA apartment; launch powershell.exe with -STA'
}

function Say([string]$message) { [Console]::Out.WriteLine("probe: $message"); [Console]::Out.Flush() }

Say "ready (apartment=STA, input=$InputPath)"
if ($Surfaces -contains 'thumbnail') {
    Say 'thumbnail: calling IShellItemImageFactory::GetImage'
    [St2kShellSurfaceProbe]::Thumbnail($InputPath, $ThumbnailOut)
    Say "thumbnail -> $ThumbnailOut"
}
if ($Surfaces -contains 'preview') {
    Say 'preview: activating the registered CLSID'
    [St2kShellSurfaceProbe]::Preview($PreviewClsid, $InputPath, $PreviewOut)
    Say "preview -> $PreviewOut"
}
Say 'done'
