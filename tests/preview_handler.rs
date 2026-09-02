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

mod common;

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
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::{
    IPreviewHandler, IThumbnailProvider, SHCreateMemStream, SHCreateStreamOnFileEx, WTS_ALPHATYPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowExW, GetMessageW,
    PostMessageW, PostQuitMessage, RegisterClassW, SendMessageW, TranslateMessage, MSG,
    WINDOW_EX_STYLE, WM_APP, WM_NCDESTROY, WM_PRINTCLIENT, WNDCLASSW, WS_OVERLAPPED,
};

const CLSID_PREVIEW_HANDLER: GUID = GUID::from_u128(0x2C8F1A3D_6B4E_4D9C_A1F2_7E3B5C8D0A46);
/// The thumbnail coclass (`tests/com_roundtrip.rs`) — used to put the pane under the
/// same load Explorer does when a folder full of jp2 files is on screen.
const CLSID_THUMBNAIL_PROVIDER: GUID = GUID::from_u128(0x7B2E6A14_9C3D_4F8A_B1E7_2A5D9F0C6E31);

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

/// Load the DLL and create the preview handler asking for the initializer, the
/// same handshake prevhost performs.
unsafe fn create_handler() -> Result<IInitializeWithStream> {
    let path = common::dll_path();
    assert!(
        path.exists(),
        "cdylib not built at {path:?} — run `cargo build` first"
    );
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let module: HMODULE = LoadLibraryW(PCWSTR(wide.as_ptr()))?;

    let proc =
        GetProcAddress(module, s!("DllGetClassObject")).ok_or_else(|| Error::from(E_FAIL))?;
    let dll_get_class_object: DllGetClassObjectFn = std::mem::transmute(proc);

    let mut factory_ptr: *mut c_void = std::ptr::null_mut();
    dll_get_class_object(
        &CLSID_PREVIEW_HANDLER,
        &IClassFactory::IID,
        &mut factory_ptr,
    )
    .ok()?;
    assert!(!factory_ptr.is_null(), "null class factory");
    let factory = IClassFactory::from_raw(factory_ptr);
    factory.CreateInstance(None)
}

/// Same handshake, for the THUMBNAIL coclass.
unsafe fn create_thumb_provider() -> Result<IInitializeWithStream> {
    let path = common::dll_path();
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let module: HMODULE = LoadLibraryW(PCWSTR(wide.as_ptr()))?;
    let proc =
        GetProcAddress(module, s!("DllGetClassObject")).ok_or_else(|| Error::from(E_FAIL))?;
    let dll_get_class_object: DllGetClassObjectFn = std::mem::transmute(proc);
    let mut factory_ptr: *mut c_void = std::ptr::null_mut();
    dll_get_class_object(
        &CLSID_THUMBNAIL_PROVIDER,
        &IClassFactory::IID,
        &mut factory_ptr,
    )
    .ok()?;
    assert!(!factory_ptr.is_null(), "null thumbnail class factory");
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
    let rect = RECT {
        left: 0,
        top: 0,
        right: PANE_W,
        bottom: PANE_H,
    };
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
    SendMessageW(
        child,
        WM_PRINTCLIENT,
        Some(WPARAM(memdc.0 as usize)),
        Some(LPARAM(0)),
    );
    let px_index = ((PANE_H / 2) * PANE_W + PANE_W / 2) as usize * 4;
    let buf = std::slice::from_raw_parts(bits as *const u8, (PANE_W * PANE_H) as usize * 4);
    let px = [
        buf[px_index],
        buf[px_index + 1],
        buf[px_index + 2],
        buf[px_index + 3],
    ];
    SelectObject(memdc, old);
    let _ = DeleteObject(hbmp.into());
    let _ = DeleteDC(memdc);
    Some(px)
}

fn red_png() -> Vec<u8> {
    solid_png([255, 0, 0, 255])
}

fn solid_png(rgba: [u8; 4]) -> Vec<u8> {
    let mut img = RgbaImage::new(64, 64);
    for p in img.pixels_mut() {
        *p = Rgba(rgba);
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
    // Build <tmp>\st2k-preview-huge-<pid>.cbz: a red cover + >256 MiB of STORED zeros
    // (stored, so the on-disk size really exceeds the ceiling no matter the
    // user's MaxSize setting — the effective cap is min(MaxSize, 256 MiB)).
    // PID-suffixed (matching `preview_keeps_up_with_a_folder_of_jp2_under_thumbnail_load`
    // below) so two concurrent `cargo test` runs can't race File::create-truncate /
    // remove_file against the same ~304 MiB file.
    let path = std::env::temp_dir().join(format!("st2k-preview-huge-{}.cbz", std::process::id()));
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
    assert!(
        rendered,
        "oversized CBZ cover never rendered — streamed-cover rescue missing"
    );
}

/// Issue #11: "preview of jp2 files are not shown, but thumbnails are shown."
/// jp2 has no `image`-crate and no in-box WIC codec, so it is decoded by the
/// ImageMagick subprocess tier — the ONLY tier the preview pane reaches through a
/// spawned child process. This drives the real COM preview handler over an in-memory
/// stream to prove the pane actually paints, rather than reasoning about it.
#[test]
fn preview_renders_a_jp2_from_memory_stream() {
    let jp2 = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../test-corpus/sample.jp2"),
    );
    let Ok(jp2) = jp2 else {
        eprintln!("skipping: ../test-corpus/sample.jp2 not present");
        return;
    };
    unsafe {
        let stream: IStream = SHCreateMemStream(Some(&jp2)).expect("SHCreateMemStream");
        // Any non-background pixel proves the pane painted the image rather than
        // staying empty; the corpus sample's centre is not the pane background.
        assert!(
            preview_renders(&stream, |px| !(px[0] > 240 && px[1] > 240 && px[2] > 240)),
            "jp2 never rendered in the preview pane (issue #11)"
        );
    }
}

/// BGRA "is clearly blue".
fn is_blue(px: [u8; 4]) -> bool {
    px[0] > 180 && px[1] < 80 && px[2] < 80
}

/// BGRA "is near-black" (the corpus jp2's centre pixel is 0,0,0).
fn is_black(px: [u8; 4]) -> bool {
    px[0] < 60 && px[1] < 60 && px[2] < 60
}

/// Poll the handler's child window until `is_hit` matches, or time out.
unsafe fn wait_for_pixel(parent: HWND, is_hit: impl Fn([u8; 4]) -> bool, secs: u64) -> [u8; 4] {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut last = [0u8; 4];
    while std::time::Instant::now() < deadline {
        let child = FindWindowExW(Some(parent), None, PREVIEW_CLASS, None).unwrap_or_default();
        if !child.is_invalid() {
            if let Some(px) = print_client_center(child) {
                last = px;
                if is_hit(px) {
                    return px;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    last
}

/// Issue #11 follow-up: *"if there are several jp2 files, and you click on different
/// files, the preview would stop refreshing."*
///
/// The single-file test above passes, which is why this was not reproducible. The
/// difference is REUSE: Explorer's preview pane keeps ONE handler instance alive for a
/// given CLSID and re-drives it per selection — `Initialize(new stream)` + `DoPreview`,
/// with no guarantee of an `Unload` in between. This drives that exact sequence over
/// three files (fast tier -> ImageMagick-subprocess tier -> fast tier) and asserts the
/// pane actually changes each time, rather than keeping the previous file's pixels.
#[test]
fn preview_refreshes_when_one_handler_is_reused_across_files() {
    let jp2 = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../test-corpus/sample.jp2"),
    );
    let Ok(jp2) = jp2 else {
        eprintln!("skipping: ../test-corpus/sample.jp2 not present");
        return;
    };

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        ensure_host_class();

        // Pane host on its own pumping thread, exactly as `preview_renders` does.
        let (tx, rx) = std::sync::mpsc::channel::<isize>();
        let host = std::thread::spawn(move || {
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

        // ONE handler for the whole run — the point of the test.
        let init = create_handler().expect("create preview handler");
        let handler: IPreviewHandler = init.cast().expect("QI IPreviewHandler");
        let rect = RECT {
            left: 0,
            top: 0,
            right: PANE_W,
            bottom: PANE_H,
        };
        handler.SetWindow(parent, &rect).expect("SetWindow");

        // Selection 1: a red PNG (fast tier) — establishes a known "previous file".
        let red = red_png();
        init.Initialize(&SHCreateMemStream(Some(&red)).unwrap(), 0)
            .expect("Initialize #1");
        handler.DoPreview().expect("DoPreview #1");
        let px1 = wait_for_pixel(parent, is_red, 20);
        assert!(is_red(px1), "first selection never rendered: {px1:?}");

        // Selection 2: the jp2 (ImageMagick-subprocess tier), SAME handler, no Unload.
        init.Initialize(&SHCreateMemStream(Some(&jp2)).unwrap(), 0)
            .expect("Initialize #2");
        handler.DoPreview().expect("DoPreview #2");
        let px2 = wait_for_pixel(parent, is_black, 20);
        assert!(
            !is_red(px2),
            "pane still shows the PREVIOUS file after switching to the jp2 \
             (issue #11: 'the preview would stop refreshing'): {px2:?}"
        );
        assert!(is_black(px2), "jp2 never rendered on reuse: {px2:?}");

        // Selection 3: back to a fast-tier file — the pane must follow again.
        let blue = solid_png([0, 0, 255, 255]);
        init.Initialize(&SHCreateMemStream(Some(&blue)).unwrap(), 0)
            .expect("Initialize #3");
        handler.DoPreview().expect("DoPreview #3");
        let px3 = wait_for_pixel(parent, is_blue, 20);
        assert!(
            is_blue(px3),
            "pane did not refresh on the third selection: {px3:?}"
        );

        handler.Unload().expect("Unload");
        drop(handler);
        drop(init);
        let _ = PostMessageW(Some(parent), WM_HOST_CLOSE, WPARAM(0), LPARAM(0));
        let _ = host.join();
    }
}

/// Issue #11, the reporter's ACTUAL situation, reproduced locally rather than asked
/// about: a folder holding several jp2 files, the preview pane being clicked from one to
/// the next, while Explorer builds thumbnails for the neighbouring files at the same
/// time. jp2 is decoded by spawning ImageMagick, so every one of those thumbnails is its
/// own subprocess competing with the pane's — the load the single-file test never had.
///
/// Every selection must render. A blank pane here is the bug the reporter is seeing.
#[test]
fn preview_keeps_up_with_a_folder_of_jp2_under_thumbnail_load() {
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../test-corpus/sample.jp2");
    let Ok(jp2) = std::fs::read(&corpus) else {
        eprintln!("skipping: ../test-corpus/sample.jp2 not present");
        return;
    };

    // A folder of DISTINCT jp2 files, so nothing can be served from a cache.
    let dir = std::env::temp_dir().join(format!("st2k_jp2_folder_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    const FILES: usize = 12;
    let paths: Vec<std::path::PathBuf> = (0..FILES)
        .map(|i| {
            let p = dir.join(format!("photo_{i:02}.jp2"));
            // Append i bytes of trailing slack so each file is byte-distinct.
            let mut bytes = jp2.clone();
            bytes.extend(std::iter::repeat_n(0u8, i));
            std::fs::write(&p, &bytes).unwrap();
            p
        })
        .collect();

    // Explorer's thumbnail pass over the neighbours: keep hammering GetThumbnail on
    // background threads for as long as the pane test runs. One counter PER LANE (rather
    // than one shared total) so readiness can be checked per lane below — a shared total
    // could clear its floor from three fast lanes while a fourth had not started at all.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    const LANES: usize = 4;
    let lane_calls: Vec<std::sync::Arc<std::sync::atomic::AtomicUsize>> = (0..LANES)
        .map(|_| std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)))
        .collect();
    let mut load = Vec::new();
    for (lane, calls) in lane_calls.iter().enumerate() {
        let stop = stop.clone();
        let calls = calls.clone();
        let paths = paths.clone();
        load.push(std::thread::spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let mut i = lane;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let bytes = std::fs::read(&paths[i % paths.len()]).unwrap();
                i += 1;
                let Ok(init) = create_thumb_provider() else {
                    continue;
                };
                let Ok(stream) = SHCreateMemStream(Some(&bytes)).ok_or(Error::from(E_FAIL)) else {
                    continue;
                };
                if init.Initialize(&stream, 0).is_err() {
                    continue;
                }
                let Ok(provider) = init.cast::<IThumbnailProvider>() else {
                    continue;
                };
                let mut hbmp = windows::Win32::Graphics::Gdi::HBITMAP::default();
                let mut alpha = WTS_ALPHATYPE::default();
                if provider.GetThumbnail(256, &mut hbmp, &mut alpha).is_ok() && !hbmp.is_invalid() {
                    let _ = DeleteObject(hbmp.into());
                }
                calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }));
    }

    // Wait for the thumbnail storm to actually be running on EVERY lane, rather than
    // hoping a fixed sleep was long enough: poll each lane's own counter until it has
    // completed at least MIN_CALLS_PER_LANE full GetThumbnail round trips. A deadline
    // still bounds the wait so a genuinely stuck lane fails the test instead of hanging.
    const MIN_CALLS_PER_LANE: usize = 1;
    let readiness_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let ready = lane_calls
            .iter()
            .all(|c| c.load(std::sync::atomic::Ordering::Relaxed) >= MIN_CALLS_PER_LANE);
        if ready {
            break;
        }
        assert!(
            std::time::Instant::now() < readiness_deadline,
            "thumbnail load storm never reached {MIN_CALLS_PER_LANE} call(s) on every lane \
             within the deadline — the storm this test depends on never really started"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let mut blanks: Vec<String> = Vec::new();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        ensure_host_class();

        let (tx, rx) = std::sync::mpsc::channel::<isize>();
        let host = std::thread::spawn(move || {
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

        // ONE handler, clicked from file to file, exactly as the pane does.
        let init = create_handler().expect("create preview handler");
        let handler: IPreviewHandler = init.cast().expect("QI IPreviewHandler");
        let rect = RECT {
            left: 0,
            top: 0,
            right: PANE_W,
            bottom: PANE_H,
        };
        handler.SetWindow(parent, &rect).expect("SetWindow");

        for p in &paths {
            let bytes = std::fs::read(p).unwrap();
            let stream = SHCreateMemStream(Some(&bytes)).expect("SHCreateMemStream");
            init.Initialize(&stream, 0).expect("Initialize");
            handler.DoPreview().expect("DoPreview");
            // The corpus sample's centre pixel is black; anything else means the pane
            // did not end up showing THIS file.
            let px = wait_for_pixel(parent, is_black, 20);
            if !is_black(px) {
                blanks.push(format!(
                    "{} -> {px:?}",
                    p.file_name().unwrap().to_string_lossy()
                ));
            }
        }

        handler.Unload().expect("Unload");
        drop(handler);
        drop(init);
        let _ = PostMessageW(Some(parent), WM_HOST_CLOSE, WPARAM(0), LPARAM(0));
        let _ = host.join();
    }

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for t in load {
        let _ = t.join();
    }
    let calls: usize = lane_calls
        .iter()
        .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
        .sum();
    let _ = std::fs::remove_dir_all(&dir);

    eprintln!("competing thumbnail decodes during the run: {calls}");
    assert!(
        calls > 0,
        "the thumbnail load never ran — this test proved nothing"
    );
    assert!(
        blanks.is_empty(),
        "{}/{FILES} selections did not render under thumbnail load (issue #11): {blanks:?}",
        blanks.len()
    );
}

/// Issue #11 again, after 1.7.3: a file that is huge in PIXELS rather than bytes.
///
/// The reporter's follow-up was a 9958x7686 (76 MP) map scan from archive.org that is only
/// ~11 MB on disk. It still timed out, because the decode did not scale with what the pane
/// can show: ImageMagick was asked for a fixed 4096 px surface, which was then PNG-encoded
/// (22 MB) and decoded back, and the whole round trip missed the 12 s budget. The pane gave
/// up on a file that decodes fine, which is why "too big" looked like a size problem.
///
/// Decoding now stops at the pane's own target, so this must render, AND it must land well
/// inside the budget rather than scraping past it. `../test-corpus/huge.jp2` comes from
/// `scripts/build-corpus.ps1`; the test skips when the corpus has not been built.
#[test]
fn preview_renders_a_76_megapixel_jp2_inside_the_budget() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../test-corpus/huge.jp2");
    let Ok(huge) = std::fs::read(&path) else {
        eprintln!("skipping: ../test-corpus/huge.jp2 not present (run scripts/build-corpus.ps1)");
        return;
    };

    // Sanity: if the sample ever stops being enormous this test proves nothing, so read
    // the dimensions out of the JP2 image header box (`ihdr` = height then width, BE u32).
    let megapixels = huge.windows(4).position(|w| w == b"ihdr").map(|i| {
        let n = |o: usize| u32::from_be_bytes(huge[i + o..i + o + 4].try_into().unwrap()) as u64;
        (n(4) * n(8)) / 1_000_000
    });
    assert!(
        megapixels.is_none_or(|mp| mp > 50),
        "huge.jp2 must be >50 MP to exercise the budget, got {megapixels:?} MP"
    );

    let start = std::time::Instant::now();
    let rendered = unsafe {
        let stream: IStream = SHCreateMemStream(Some(&huge)).expect("SHCreateMemStream");
        // Any non-background pixel proves it painted rather than timing out to empty.
        preview_renders(&stream, |px| !(px[0] > 240 && px[1] > 240 && px[2] > 240))
    };
    let elapsed = start.elapsed();
    assert!(
        rendered,
        "a 76 MP jp2 never rendered in the preview pane after {elapsed:?} (issue #11)"
    );
    // The handler's own budget is 12s. Anything close to that on a warm machine means
    // a loaded machine (Explorer thumbnailing the neighbours) will still miss it.
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "rendered, but took {elapsed:?} — too close to the 12s budget to survive a busy folder"
    );
}

/// Issue #11, the actual failure mode: when a decode MISSES on a reused handler —
/// undecodable bytes, or (the real-world case) an ImageMagick subprocess that blows the
/// wall-clock budget because Explorer is running several of them at once for the other
/// jp2 files in the folder — the pane must go EMPTY, not keep showing the file you were
/// looking at before. Leaving the previous pixels up is indistinguishable from "the
/// preview stopped refreshing".
#[test]
fn preview_clears_when_the_next_file_fails_to_decode() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        ensure_host_class();

        let (tx, rx) = std::sync::mpsc::channel::<isize>();
        let host = std::thread::spawn(move || {
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
        let handler: IPreviewHandler = init.cast().expect("QI IPreviewHandler");
        let rect = RECT {
            left: 0,
            top: 0,
            right: PANE_W,
            bottom: PANE_H,
        };
        handler.SetWindow(parent, &rect).expect("SetWindow");

        // Selection 1: renders red.
        let red = red_png();
        init.Initialize(&SHCreateMemStream(Some(&red)).unwrap(), 0)
            .expect("Initialize #1");
        handler.DoPreview().expect("DoPreview #1");
        let px1 = wait_for_pixel(parent, is_red, 20);
        assert!(is_red(px1), "first selection never rendered: {px1:?}");

        // Selection 2: bytes no tier can decode — stands in for a budget-expired
        // ImageMagick decode, which reaches the pane as the same `None`.
        let junk = vec![0x7Au8; 4096];
        init.Initialize(&SHCreateMemStream(Some(&junk)).unwrap(), 0)
            .expect("Initialize #2");
        handler.DoPreview().expect("DoPreview #2");
        let px2 = wait_for_pixel(parent, |px| !is_red(px), 20);

        handler.Unload().expect("Unload");
        drop(handler);
        drop(init);
        let _ = PostMessageW(Some(parent), WM_HOST_CLOSE, WPARAM(0), LPARAM(0));
        let _ = host.join();

        assert!(
            !is_red(px2),
            "pane is STILL showing the previous file after the next one failed to \
             decode (issue #11: 'the preview would stop refreshing'): {px2:?}"
        );
    }
}

/// Issue #11's third report: after the pane sat idle for ~30 minutes, it "missed to refresh
/// the preview a couple of times".
///
/// The host owns the parent and recycles the pane when it has been idle — destroying OUR
/// child window WITHOUT calling `Unload`. The handler kept the now-dangling `HWND`, so
/// `ensure_window` reported "already have one", `post_render` posted to a dead window,
/// `PostMessageW` failed, the payload was dropped, and the pane silently went on showing the
/// PREVIOUS file. Intermittent and idle-correlated, because that is when the host recycles.
///
/// Simulated here by destroying the child between two previews on ONE reused handler — the
/// same state the handler is left in, without waiting half an hour for Explorer to do it.
#[test]
fn preview_recovers_when_the_host_destroys_our_window_without_unloading() {
    let red = red_png();
    let blue = solid_png([0, 0, 255, 255]);
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        ensure_host_class();

        let (tx, rx) = std::sync::mpsc::channel::<isize>();
        let host = std::thread::spawn(move || {
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
        let rect = RECT {
            left: 0,
            top: 0,
            right: PANE_W,
            bottom: PANE_H,
        };

        // ── First file: renders normally.
        let init = create_handler().expect("create preview handler");
        let s1: IStream = SHCreateMemStream(Some(&red)).expect("stream");
        init.Initialize(&s1, 0).expect("Initialize");
        let handler: IPreviewHandler = init.cast().expect("QI IPreviewHandler");
        handler.SetWindow(parent, &rect).expect("SetWindow");
        handler.DoPreview().expect("DoPreview");
        assert!(
            is_red(wait_for_pixel(parent, is_red, 20)),
            "first file never rendered red"
        );

        // ── The host tears our child down behind our back (no Unload), as it does when it
        // recycles an idle pane. Destroy it from the child's OWN thread via our close
        // message, which is exactly what a host-driven teardown looks like to us.
        let child = FindWindowExW(Some(parent), None, PREVIEW_CLASS, None).unwrap_or_default();
        assert!(!child.is_invalid(), "expected a child window to destroy");
        let _ = PostMessageW(Some(child), WM_APP + 1, WPARAM(0), LPARAM(0));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && FindWindowExW(Some(parent), None, PREVIEW_CLASS, None)
                .unwrap_or_default()
                .0
                == child.0
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // ── Second file on the SAME handler: must notice the window is gone and rebuild.
        let init2: IInitializeWithStream = handler.cast().expect("QI IInitializeWithStream");
        let s2: IStream = SHCreateMemStream(Some(&blue)).expect("stream");
        init2.Initialize(&s2, 0).expect("Initialize #2");
        handler.SetWindow(parent, &rect).expect("SetWindow #2");
        handler.DoPreview().expect("DoPreview #2");
        assert!(
            is_blue(wait_for_pixel(parent, is_blue, 20)),
            "after the host destroyed our window, the pane never refreshed to the new file \
             (it silently kept the old one)"
        );

        handler.Unload().expect("Unload");
        drop(handler);
        drop(init);
        let _ = PostMessageW(Some(parent), WM_HOST_CLOSE, WPARAM(0), LPARAM(0));
        let _ = host.join();
    }
}
