//! The preview pane as Explorer really runs it: the INSTALLED preview handler, activated out of
//! process in `prevhost.exe` (our CLSID's AppID is the shell's preview-host surrogate), handed a
//! file stream across the process boundary and parented to a window in this process.
//!
//! Every other test loads the DLL in-process from the build tree, so none of them can see what
//! only the surrogate shows: the shipped DLL loading in `prevhost.exe`, a render that takes the
//! surrogate down with it (`panic = "abort"`), or a decoder child (`st2k <codec>-frame`) still
//! running after the pane has moved on. The release ritual left this to a person clicking files
//! in Explorer's preview pane; this does the same thing without a mouse.
//!
//! Run it after installing a build, never as part of the suite:
//!
//! `cargo test --release --test prevhost_pane -- --ignored --nocapture`
//!
//! `PREVHOST_FILES=a;b;c` replaces the default list (real samples of the formats a release
//! touched, plus two videos for the child-process path). A sample that is absent is NOT MEASURED.
#![cfg(windows)]

mod common;

use std::ffi::c_void;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::core::{w, Interface, Result, GUID, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, DIB_RGB_COLORS,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IStream, CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED,
    STGM_READ, STGM_SHARE_DENY_NONE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::{IPreviewHandler, SHCreateStreamOnFileEx};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowExW, GetMessageW,
    PostMessageW, PostQuitMessage, RegisterClassW, SetLayeredWindowAttributes, ShowWindow,
    TranslateMessage, LWA_ALPHA, MSG, SW_SHOWNOACTIVATE, WM_APP, WM_NCDESTROY, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

const CLSID_PREVIEW_HANDLER: GUID = GUID::from_u128(0x2C8F1A3D_6B4E_4D9C_A1F2_7E3B5C8D0A46);
/// The handler's child-window class (previewhandler.rs `CLASS_NAME`).
const PREVIEW_CLASS: PCWSTR = w!("SageThumbs2KPreview");
const HOST_CLASS: PCWSTR = w!("SageThumbs2KPrevhostHost");
const WM_HOST_CLOSE: u32 = WM_APP + 9;
const PANE_W: i32 = 640;
const PANE_H: i32 = 480;
const PANE_DEADLINE: Duration = Duration::from_secs(20);
/// Render child and DirectComposition content too (the windows crate does not name it).
const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(0x0000_0002);
/// How long a decoder child may outlive `Unload` before it counts as left behind.
const CHILD_GRACE: Duration = Duration::from_secs(10);

/// Real samples of what 3.3.0 changed, plus two videos so the `st2k <codec>-frame` child path runs.
const DEFAULT_SAMPLES: &[&str] = &[
    "real.vtf",
    "real.ktx",
    "real.ani",
    "real.six",
    "real.sixel",
    "real.avifs",
    "real.dxf",
    "real.xmind",
    "real.vstx",
    "real.vssx",
    "real.nupkg",
    "real.vsix",
    "real.wma",
    "real.psd",
    "real.ts",
    "real.mp4",
];

unsafe extern "system" fn host_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_HOST_CLOSE => {
            let _ = unsafe { DestroyWindow(h) };
            LRESULT(0)
        }
        WM_NCDESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(h, msg, wp, lp) },
    }
}

fn ensure_host_class() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(host_proc),
            hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
            lpszClassName: HOST_CLASS,
            ..Default::default()
        };
        RegisterClassW(&wc);
    });
}

unsafe fn file_stream(path: &str) -> Result<IStream> {
    let wide = common::to_wide(std::ffi::OsStr::new(path));
    unsafe {
        SHCreateStreamOnFileEx(
            PCWSTR(wide.as_ptr()),
            (STGM_READ | STGM_SHARE_DENY_NONE).0,
            0,
            false,
            None,
        )
    }
}

/// What a capture of the handler's window came back as: drawn, or which step came back empty.
unsafe fn capture_state(child: HWND, flags: PRINT_WINDOW_FLAGS) -> std::result::Result<(), String> {
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = PANE_W;
    bmi.bmiHeader.biHeight = -PANE_H;
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    let memdc = unsafe { CreateCompatibleDC(None) };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let Ok(hbmp) = (unsafe { CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) })
    else {
        let _ = unsafe { DeleteDC(memdc) };
        return Err("no DIB".into());
    };
    let old = unsafe { SelectObject(memdc, hbmp.into()) };
    let printed = unsafe { PrintWindow(child, memdc, flags) }.as_bool();
    let px = unsafe { std::slice::from_raw_parts(bits as *const u32, (PANE_W * PANE_H) as usize) };
    let state = if !printed {
        Err("PrintWindow refused".to_string())
    } else if px.iter().all(|p| p & 0x00FF_FFFF == px[0] & 0x00FF_FFFF) {
        Err(format!("flat #{:06X}", px[0] & 0x00FF_FFFF))
    } else {
        Ok(())
    };
    unsafe { SelectObject(memdc, old) };
    let _ = unsafe { DeleteObject(hbmp.into()) };
    let _ = unsafe { DeleteDC(memdc) };
    state
}

/// Did the handler's window draw anything? `Err` says which step came back empty.
unsafe fn drew_something(parent: HWND) -> std::result::Result<(), String> {
    let child = unsafe { FindWindowExW(Some(parent), None, PREVIEW_CLASS, None) };
    let child = child
        .ok()
        .filter(|c| !c.is_invalid())
        .ok_or("no handler window under the host")?;
    unsafe { capture_state(child, PW_RENDERFULLCONTENT) }.or_else(|full| {
        unsafe { capture_state(child, PRINT_WINDOW_FLAGS(0)) }
            .map_err(|plain| format!("{full}; plain {plain}"))
    })
}

/// `st2k.exe` processes whose parent is a `prevhost.exe`: decoder children of a preview pane.
fn prevhost_children() -> Vec<u32> {
    let script = "Get-CimInstance Win32_Process -Filter \"Name='st2k.exe'\" | Where-Object { \
                  (Get-Process -Id $_.ParentProcessId -ErrorAction SilentlyContinue).ProcessName \
                  -eq 'prevhost' } | ForEach-Object { $_.ProcessId }";
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output();
    out.map(|o| {
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    })
    .unwrap_or_default()
}

/// One file through the surrogate: `Ok(ms to first picture)` or what went wrong.
fn preview_in_prevhost(path: &str) -> std::result::Result<u128, String> {
    ensure_host_class();
    let (tx, rx) = std::sync::mpsc::channel::<isize>();
    // A window that is never shown is never composed, so what ANOTHER process draws into it
    // captures as solid black (measured: every sample, both PrintWindow modes). So the host is
    // shown - far off every screen, 1/255 opaque, a tool window that cannot take focus - which
    // is composed like any window and still invisible to whoever is at the desk.
    let host = std::thread::spawn(move || unsafe {
        let parent = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            HOST_CLASS,
            w!(""),
            WS_POPUP,
            -20000,
            -20000,
            PANE_W,
            PANE_H,
            None,
            None,
            None,
            None,
        )
        .expect("host parent window");
        let _ = SetLayeredWindowAttributes(parent, COLORREF(0), 1, LWA_ALPHA);
        let _ = ShowWindow(parent, SW_SHOWNOACTIVATE);
        let _ = tx.send(parent.0 as isize);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
    let parent = HWND(rx.recv().map_err(|e| e.to_string())? as *mut c_void);
    let result = unsafe { drive(path, parent) };
    let _ = unsafe { PostMessageW(Some(parent), WM_HOST_CLOSE, WPARAM(0), LPARAM(0)) };
    let _ = host.join();
    result
}

unsafe fn drive(path: &str, parent: HWND) -> std::result::Result<u128, String> {
    let start = Instant::now();
    let stream = unsafe { file_stream(path) }.map_err(|e| format!("open: {e}"))?;
    let init: IInitializeWithStream =
        unsafe { CoCreateInstance(&CLSID_PREVIEW_HANDLER, None, CLSCTX_LOCAL_SERVER) }
            .map_err(|e| format!("activate in prevhost: {e}"))?;
    unsafe { init.Initialize(&stream, STGM_READ.0) }.map_err(|e| format!("Initialize: {e}"))?;
    let handler: IPreviewHandler = init.cast().map_err(|e| format!("cast: {e}"))?;
    let rect = RECT {
        left: 0,
        top: 0,
        right: PANE_W,
        bottom: PANE_H,
    };
    unsafe { handler.SetWindow(parent, &rect) }.map_err(|e| format!("SetWindow: {e}"))?;
    unsafe { handler.DoPreview() }.map_err(|e| format!("DoPreview: {e}"))?;
    let mut drew = Err(String::from("never looked"));
    while start.elapsed() < PANE_DEADLINE {
        std::thread::sleep(Duration::from_millis(150));
        drew = unsafe { drew_something(parent) }.map(|()| start.elapsed().as_millis());
        if drew.is_ok() {
            break;
        }
    }
    // A surrogate the render killed answers the next call with a disconnected-server error.
    let unloaded = unsafe { handler.Unload() };
    drop(handler);
    drop(init);
    unloaded.map_err(|e| format!("prevhost did not survive the render (Unload: {e})"))?;
    drew.map_err(|why| format!("nothing drawn within {}s ({why})", PANE_DEADLINE.as_secs()))
}

fn samples() -> Vec<(String, Option<PathBuf>)> {
    match std::env::var("PREVHOST_FILES") {
        Ok(list) => list
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|s| {
                (
                    s.to_string(),
                    Some(PathBuf::from(s)).filter(|p| p.is_file()),
                )
            })
            .collect(),
        Err(_) => DEFAULT_SAMPLES
            .iter()
            .map(|n| (n.to_string(), st2k_base::testcorpus::path(n)))
            .collect(),
    }
}

#[test]
#[ignore = "drives the INSTALLED handler through prevhost.exe; run after installing a build"]
fn installed_preview_handler_renders_in_prevhost_and_leaves_no_children() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut failures = Vec::new();
    for (name, path) in samples() {
        let Some(path) = path else {
            eprintln!("NOT MEASURED  {name}: sample absent");
            continue;
        };
        let path = path.to_string_lossy().into_owned();
        match preview_in_prevhost(&path) {
            Ok(ms) => eprintln!("ok    {name}: drew in {ms} ms"),
            Err(why) => {
                eprintln!("FAIL  {name}: {why}");
                failures.push(format!("{name}: {why}"));
            }
        }
        let until = Instant::now() + CHILD_GRACE;
        let mut left = prevhost_children();
        while !left.is_empty() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(250));
            left = prevhost_children();
        }
        if !left.is_empty() {
            eprintln!("FAIL  {name}: st2k.exe children of prevhost still running: {left:?}");
            failures.push(format!("{name}: left st2k.exe children {left:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "prevhost failures:\n{}",
        failures.join("\n")
    );
}
