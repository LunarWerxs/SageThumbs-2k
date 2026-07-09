//! End-to-end COM test that drives the built DLL's PREVIEW handler the way
//! `prevhost.exe` does — no registration, no admin, no Explorer:
//!
//!   LoadLibrary(DLL) -> DllGetClassObject -> IClassFactory::CreateInstance
//!   -> QI IInitializeWithStream -> Initialize(IStream) -> QI IPreviewHandler
//!   -> SetWindow -> DoPreview -> WM_PRINTCLIENT the child window and check pixels.
//!
//! The oversized-CBZ case is the regression proof for the shared streaming
//! cascade (`streamsrc`): before the preview handler used it, any file past the
//! read cap drained to nothing and the pane went BLANK; now the cover streams
//! out of the archive (central directory + one entry) and renders.
//!
//! The parent pane window is hosted on its OWN message-pumping thread — the
//! handler creates its `WS_CHILD` window from a dedicated UI thread, and Windows
//! sends `WM_PARENTNOTIFY` synchronously to the parent's thread on child
//! creation; a non-pumping parent would deadlock `DoPreview`. Real `prevhost`
//! always pumps its pane, so this mirrors production.
//!
//! IMPORTANT: run via `scripts/test.ps1` (or `cargo build` before `cargo test`).
//! Plain `cargo test` does NOT refresh target/<profile>/sagethumbs2k.dll, so the
//! LoadLibrary below could otherwise pick up a stale cdylib.
#![cfg(windows)]

use std::ffi::c_void;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;

use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use windows::core::{s, w, Error, Interface, Result, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{E_FAIL, HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, DIB_RGB_COLORS,
};
use windows::Win32::System::Com::{
    CoInitializeEx, IClassFactory, IStream, COINIT_APARTMENTTHREADED, STGM_READ,
    STGM_SHARE_DENY_NONE,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleW, GetProcAddress, LoadLibraryW,
};
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::{IPreviewHandler, SHCreateMemStream, SHCreateStreamOnFileEx};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowExW, GetMessageW,
    PostMessageW, PostQuitMessage, RegisterClassW, SendMessageW, TranslateMessage, MSG,
    WINDOW_EX_STYLE, WM_APP, WM_NCDESTROY, WM_PRINTCLIENT, WNDCLASSW, WS_OVERLAPPED,
};

const CLSID_PREVIEW_HANDLER: GUID = GUID::from_u128(0x2C8F1A3D_6B4E_4D9C_A1F2_7E3B5C8D0A46);

/// The handler's child-window class (previewhandler.rs `CLASS_NAME`).
const PREVIEW_CLASS: PCWSTR = w!("SageThumbs2KPreview");
/// Our stand-in pane-host window class (a pumping parent for the child).
const HOST_CLASS: PCWSTR = w!("SageThumbs2KPreviewTestHost");
/// Posted to the host window to make it tear itself down + end its pump.
const WM_HOST_CLOSE: u32 = WM_APP + 9;

const PANE_W: i32 = 320;
const PANE_H: i32 = 240;

type DllGetClassObjectFn =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

/// The cdylib sits one dir above the test exe (…/release/sagethumbs2k.dll vs
/// …/release/deps/<test>.exe).
fn dll_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sagethumbs2k.dll")
}

/// Load the DLL and create the preview handler asking for the initializer, the
/// same handshake prevhost performs.
unsafe fn create_handler() -> Result<IInitializeWithStream> {
    let path = dll_path();
    assert!(path.exists(), "cdylib not built at {path:?} — run `cargo build` first");
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let module: HMODULE = LoadLibraryW(PCWSTR(wide.as_ptr()))?;

    let proc = GetProcAddress(module, s!("DllGetClassObject"))
        .ok_or_else(|| Error::from(E_FAIL))?;
    let dll_get_class_object: DllGetClassObjectFn = std::mem::transmute(proc);

    let mut factory_ptr: *mut c_void = std::ptr::null_mut();
    dll_get_class_object(&CLSID_PREVIEW_HANDLER, &IClassFactory::IID, &mut factory_ptr).ok()?;
    assert!(!factory_ptr.is_null(), "null class factory");
    let factory = IClassFactory::from_raw(factory_ptr);
    factory.CreateInstance(None)
}

/// Minimal pane-host wndproc: on our close message it destroys itself; on
/// `WM_NCDESTROY` it ends the thread's pump.
unsafe extern "system" fn host_proc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_HOST_CLOSE => {
            let _ = DestroyWindow(h);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, w, l),
    }
}

fn ensure_host_class() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(host_proc),
            hInstance: hinst.into(),
            lpszClassName: HOST_CLASS,
            ..Default::default()
        };
        RegisterClassW(&wc);
    });
}

/// Initialize + SetWindow + DoPreview against a parent hosted on its OWN pumping
/// thread, then poll the handler's child window via `WM_PRINTCLIENT` until a
/// pixel matching `is_hit` shows up (or time out). Returns whether the expected
/// pixel ever rendered. Tears the handler down (Unload) before returning,
/// exercising the UI-thread join as well.
unsafe fn preview_renders(stream: &IStream, is_hit: impl Fn([u8; 4]) -> bool) -> bool {
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    ensure_host_class();

    // Host the parent pane on a thread that pumps messages (see module docs).
    let (tx, rx) = std::sync::mpsc::channel::<isize>();
    let host = std::thread::spawn(move || unsafe {
        let parent = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            HOST_CLASS,
            w!(""),
            WS_OVERLAPPED,
            0,
            0,
            PANE_W,
            PANE_H,
            None,
            None,
            None,
            None,
        )
        .expect("host parent window");
        let _ = tx.send(parent.0 as isize);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
    let parent = HWND(rx.recv().unwrap() as *mut c_void);

    let init = create_handler().expect("create preview handler");
    init.Initialize(stream, 0).expect("Initialize(IStream)");
    let handler: IPreviewHandler = init.cast().expect("QI IPreviewHandler");
    let rect = RECT { left: 0, top: 0, right: PANE_W, bottom: PANE_H };
    handler.SetWindow(parent, &rect).expect("SetWindow");
    handler.DoPreview().expect("DoPreview");

    // The render lands asynchronously on the handler's own UI thread
    // (WM_PREVIEW_RENDER -> InvalidateRect); poll its output.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut hit = false;
    while std::time::Instant::now() < deadline {
        let child = FindWindowExW(Some(parent), None, PREVIEW_CLASS, None).unwrap_or_default();
        if !child.is_invalid() {
            if let Some(px) = print_client_center(child) {
                if is_hit(px) {
                    hit = true;
                    break;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    handler.Unload().expect("Unload");
    drop(handler);
    drop(init);
    // Tear the host window + its pump thread down.
    let _ = PostMessageW(Some(parent), WM_HOST_CLOSE, WPARAM(0), LPARAM(0));
    let _ = host.join();
    hit
}

/// Ask the child window to render into a memory DC (`WM_PRINTCLIENT`, the same
/// path PrintWindow uses) and return the BGRA quad at the pane's centre.
unsafe fn print_client_center(child: HWND) -> Option<[u8; 4]> {
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = PANE_W;
    bmi.bmiHeader.biHeight = -PANE_H; // top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;

    let memdc = CreateCompatibleDC(None);
    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(hbmp.into());
        let _ = DeleteDC(memdc);
        return None;
    }
    let old = SelectObject(memdc, hbmp.into());
    // Delivered to the handler's UI thread (it pumps), rendered synchronously.
    SendMessageW(child, WM_PRINTCLIENT, Some(WPARAM(memdc.0 as usize)), Some(LPARAM(0)));
    let px_index = ((PANE_H / 2) * PANE_W + PANE_W / 2) as usize * 4;
    let buf = std::slice::from_raw_parts(bits as *const u8, (PANE_W * PANE_H) as usize * 4);
    let px = [buf[px_index], buf[px_index + 1], buf[px_index + 2], buf[px_index + 3]];
    SelectObject(memdc, old);
    let _ = DeleteObject(hbmp.into());
    let _ = DeleteDC(memdc);
    Some(px)
}

fn red_png() -> Vec<u8> {
    let mut img = RgbaImage::new(64, 64);
    for p in img.pixels_mut() {
        *p = Rgba([255, 0, 0, 255]);
    }
    let mut bytes = Vec::new();
    DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
        .unwrap();
    bytes
}

/// BGRA "is clearly red" (the pane letterbox/background never is).
fn is_red(px: [u8; 4]) -> bool {
    px[2] > 180 && px[1] < 80 && px[0] < 80
}

#[test]
fn preview_renders_a_png_from_memory_stream() {
    let png = red_png();
    unsafe {
        let stream: IStream = SHCreateMemStream(Some(&png)).expect("SHCreateMemStream");
        assert!(
            preview_renders(&stream, is_red),
            "in-memory PNG never rendered red in the preview pane"
        );
    }
}

/// The streaming-cascade proof: a CBZ bigger than the hard read ceiling
/// (`decode::limits::MAX_INPUT_BYTES`, 256 MiB) must STILL preview — the cover
/// streams out of the archive over the IStream instead of the whole file being
/// buffered (which the cap forbids; before the shared cascade this was a
/// guaranteed blank pane).
#[test]
fn preview_streams_cover_from_oversized_cbz() {
    // Build <tmp>\st2k-preview-huge.cbz: a red cover + >256 MiB of STORED zeros
    // (stored, so the on-disk size really exceeds the ceiling no matter the
    // user's MaxSize setting — the effective cap is min(MaxSize, 256 MiB)).
    let path = std::env::temp_dir().join("st2k-preview-huge.cbz");
    {
        let f = std::fs::File::create(&path).expect("create temp cbz");
        let mut zw = zip::ZipWriter::new(f);
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .large_file(true);
        zw.start_file("0001_cover.png", stored).unwrap();
        zw.write_all(&red_png()).unwrap();
        zw.start_file("zzz_ballast.bin", stored).unwrap();
        let chunk = vec![0u8; 8 << 20];
        for _ in 0..38 {
            // 38 * 8 MiB = 304 MiB > the 256 MiB ceiling
            zw.write_all(&chunk).unwrap();
        }
        zw.finish().unwrap();
    }
    assert!(
        std::fs::metadata(&path).unwrap().len() > 256 * 1024 * 1024,
        "ballast must push the archive past the hard read ceiling"
    );

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let rendered = unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let stream = SHCreateStreamOnFileEx(
            PCWSTR(wide.as_ptr()),
            (STGM_READ | STGM_SHARE_DENY_NONE).0,
            0,
            false,
            None,
        )
        .expect("SHCreateStreamOnFileEx");
        preview_renders(&stream, is_red)
    };
    let _ = std::fs::remove_file(&path);
    assert!(rendered, "oversized CBZ cover never rendered — streamed-cover rescue missing");
}
