//! The DLL half of the big-file gate (`scripts/bigfiles/bigfiles.py`): every file in a plan
//! goes through the two shell surfaces exactly as Windows drives them, from a FILE stream -
//!
//!   * the Explorer thumbnail: `IInitializeWithStream` + `IThumbnailProvider::GetThumbnail(256)`;
//!   * the preview pane: `IInitializeWithStream` + `IPreviewHandler` on a pumping host window,
//!     captured with `WM_PRINTCLIENT` once the render stops changing -
//!
//! and what each surface produced is written out for the gate to compare with the same
//! picture's normal-size twin. The gate is what judges; this only drives and records.
//!
//! Why it exists: issue #46. A 300 MB Photoshop document stayed on its 160-pixel preview in
//! Quick preview, and nothing in the suite had ever handed ANY surface a file past the 256 MiB
//! input ceiling except three hand-picked cases. The gate grows every format past the size
//! gates (sparse ballast the format ignores) and runs every surface on it.
//!
//! Ignored by default - it needs a plan the gate writes:
//!
//! `BIGFILES_PLAN=<plan.tsv> BIGFILES_OUT=<dir> cargo test --test big_files -- --ignored --test-threads=1`
//!
//! The plan is one `id<TAB>path` per line. For each id this writes `<id>.thumb.png`,
//! `<id>.pane.png` and a line of `<dir>/results.tsv`:
//! `id  thumb_ok thumb_w thumb_h thumb_ms  pane_ok pane_ms`.
#![cfg(windows)]

mod common;

use std::ffi::c_void;
use std::io::Write;
use std::time::{Duration, Instant};

use image::RgbaImage;
use windows::core::{w, Interface, Result, GUID, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetObjectW, SelectObject, BITMAP,
    BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::System::Com::{
    CoInitializeEx, IStream, COINIT_APARTMENTTHREADED, STGM_READ, STGM_SHARE_DENY_NONE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    IPreviewHandler, IThumbnailProvider, SHCreateStreamOnFileEx, WTSAT_UNKNOWN, WTS_ALPHATYPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowExW, GetMessageW,
    PostMessageW, PostQuitMessage, RegisterClassW, SendMessageW, TranslateMessage, MSG,
    WINDOW_EX_STYLE, WM_APP, WM_NCDESTROY, WM_PRINTCLIENT, WNDCLASSW, WS_OVERLAPPED,
};

const CLSID_THUMBNAIL_PROVIDER: GUID = GUID::from_u128(0x7B2E6A14_9C3D_4F8A_B1E7_2A5D9F0C6E31);
const CLSID_PREVIEW_HANDLER: GUID = GUID::from_u128(0x2C8F1A3D_6B4E_4D9C_A1F2_7E3B5C8D0A46);
/// The handler's child-window class (previewhandler.rs `CLASS_NAME`).
const PREVIEW_CLASS: PCWSTR = w!("SageThumbs2KPreview");
const HOST_CLASS: PCWSTR = w!("SageThumbs2KBigFilesHost");
const WM_HOST_CLOSE: u32 = WM_APP + 9;
/// A preview pane of a typical size, so a stand-in smaller than the pane shows as blur.
const PANE_W: i32 = 640;
const PANE_H: i32 = 480;
/// How long a surface may take before the gate calls it a failure of its own.
const PANE_DEADLINE: Duration = Duration::from_secs(20);
/// The render is "done" once two captures this far apart agree.
const SETTLE: Duration = Duration::from_millis(1500);

/// A scratch settings root, so the gate measures the shipped defaults (MaxSize 4096 MB and
/// the rest), never whatever this desk's own settings happen to be.
const TEST_SETTINGS_ROOT: &str = r"Software\SageThumbs2K-test\big_files";

unsafe fn file_stream(path: &str) -> Result<IStream> {
    let wide = common::to_wide(std::ffi::OsStr::new(path));
    unsafe {
        SHCreateStreamOnFileEx(
            PCWSTR(wide.as_ptr()),
            STGM_READ.0 | STGM_SHARE_DENY_NONE.0,
            0,
            false,
            None,
        )
    }
}

/// The Explorer thumbnail at 256, as RGBA, from a file stream.
unsafe fn thumbnail(path: &str) -> Result<RgbaImage> {
    let stream = unsafe { file_stream(path) }?;
    let init = unsafe { common::create_instance(&CLSID_THUMBNAIL_PROVIDER) }?;
    unsafe { init.Initialize(&stream, 0) }?;
    let provider: IThumbnailProvider = init.cast()?;
    let mut hbmp = HBITMAP::default();
    let mut alpha: WTS_ALPHATYPE = WTSAT_UNKNOWN;
    unsafe { provider.GetThumbnail(256, &mut hbmp, &mut alpha) }?;
    let img = unsafe { dib_to_rgba(hbmp) };
    let _ = unsafe { DeleteObject(hbmp.into()) };
    Ok(img)
}

/// A top-down 32 bpp DIB section as RGBA.
unsafe fn dib_to_rgba(hbmp: HBITMAP) -> RgbaImage {
    let mut bm = BITMAP::default();
    unsafe {
        GetObjectW(
            hbmp.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut c_void),
        )
    };
    let (w, h, stride) = (
        bm.bmWidth as usize,
        bm.bmHeight as usize,
        bm.bmWidthBytes as usize,
    );
    let mut img = RgbaImage::new(w as u32, h as u32);
    if bm.bmBits.is_null() {
        return img;
    }
    let src = unsafe { std::slice::from_raw_parts(bm.bmBits as *const u8, stride * h) };
    for (y, row) in src.chunks(stride).take(h).enumerate() {
        for (x, px) in row.chunks(4).take(w).enumerate() {
            img.put_pixel(
                x as u32,
                y as u32,
                image::Rgba([px[2], px[1], px[0], px[3]]),
            );
        }
    }
    img
}

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

/// The whole pane, rendered into a memory DC the way `PrintWindow` asks for it.
unsafe fn capture(child: HWND) -> Option<RgbaImage> {
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = PANE_W;
    bmi.bmiHeader.biHeight = -PANE_H;
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    let memdc = unsafe { CreateCompatibleDC(None) };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbmp = unsafe { CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }.ok()?;
    let old = unsafe { SelectObject(memdc, hbmp.into()) };
    unsafe {
        SendMessageW(
            child,
            WM_PRINTCLIENT,
            Some(WPARAM(memdc.0 as usize)),
            Some(LPARAM(0)),
        )
    };
    let img = unsafe { dib_to_rgba(hbmp) };
    unsafe { SelectObject(memdc, old) };
    let _ = unsafe { DeleteObject(hbmp.into()) };
    let _ = unsafe { DeleteDC(memdc) };
    Some(img)
}

/// Is every pixel the same colour - the empty pane, before anything has drawn?
fn blank(img: &RgbaImage) -> bool {
    let first = img.get_pixel(0, 0);
    img.pixels().all(|p| p == first)
}

/// Poll the pane until its capture stops changing (and is not blank), or the deadline.
///
/// A pane that never stops changing - an animated GIF, a playing video - hands back the FIRST
/// picture it drew, which is its first frame for the normal-size file and its twin alike; so
/// it is compared like everything else instead of costing the whole deadline for nothing.
unsafe fn settle(parent: HWND, start: Instant) -> Option<(RgbaImage, Duration)> {
    let mut first: Option<(RgbaImage, Duration)> = None;
    let mut last: Option<(RgbaImage, Instant)> = None;
    while start.elapsed() < PANE_DEADLINE {
        std::thread::sleep(Duration::from_millis(100));
        let Some(img) = (unsafe { pane_capture(parent) }) else {
            continue;
        };
        first.get_or_insert_with(|| (img.clone(), start.elapsed()));
        match &last {
            Some((prev, since)) if *prev == img => {
                if since.elapsed() >= SETTLE {
                    return Some((img, since.duration_since(start)));
                }
            }
            _ => last = Some((img, Instant::now())),
        }
        if first
            .as_ref()
            .is_some_and(|(_, at)| start.elapsed() > *at + SETTLE * 4)
        {
            break; // still moving long after it first drew: an animation
        }
    }
    first
}

/// The handler's child window, captured, when it exists and has drawn something.
unsafe fn pane_capture(parent: HWND) -> Option<RgbaImage> {
    let child = unsafe { FindWindowExW(Some(parent), None, PREVIEW_CLASS, None) }.ok()?;
    if child.is_invalid() {
        return None;
    }
    unsafe { capture(child) }.filter(|img| !blank(img))
}

/// The preview pane's settled picture and how long it took to get there.
unsafe fn preview_pane(path: &str) -> Option<(RgbaImage, Duration)> {
    ensure_host_class();
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
    let parent = HWND(rx.recv().ok()? as *mut c_void);
    let result = unsafe { drive_pane(path, parent) };
    let _ = unsafe { PostMessageW(Some(parent), WM_HOST_CLOSE, WPARAM(0), LPARAM(0)) };
    let _ = host.join();
    result
}

unsafe fn drive_pane(path: &str, parent: HWND) -> Option<(RgbaImage, Duration)> {
    let start = Instant::now();
    let stream = unsafe { file_stream(path) }.ok()?;
    let init = unsafe { common::create_instance(&CLSID_PREVIEW_HANDLER) }.ok()?;
    unsafe { init.Initialize(&stream, 0) }.ok()?;
    let handler: IPreviewHandler = init.cast().ok()?;
    let rect = RECT {
        left: 0,
        top: 0,
        right: PANE_W,
        bottom: PANE_H,
    };
    unsafe { handler.SetWindow(parent, &rect) }.ok()?;
    unsafe { handler.DoPreview() }.ok()?;
    let settled = unsafe { settle(parent, start) };
    let _ = unsafe { handler.Unload() };
    settled
}

fn plan() -> Option<(Vec<(String, String)>, std::path::PathBuf)> {
    let plan = std::env::var("BIGFILES_PLAN").ok()?;
    let out = std::path::PathBuf::from(std::env::var("BIGFILES_OUT").ok()?);
    let text = std::fs::read_to_string(plan).expect("read the plan");
    let rows = text
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(id, path)| (id.to_string(), path.to_string()))
        .collect();
    Some((rows, out))
}

fn run_one(id: &str, path: &str, out: &std::path::Path) -> String {
    let t = Instant::now();
    let thumb = unsafe { thumbnail(path) };
    let thumb_ms = t.elapsed().as_millis();
    let (thumb_ok, tw, th) = match &thumb {
        Ok(img) => {
            let _ = img.save(out.join(format!("{id}.thumb.png")));
            (1, img.width(), img.height())
        }
        Err(_) => (0, 0, 0),
    };
    let (pane_ok, pane_ms) = match unsafe { preview_pane(path) } {
        Some((img, took)) => {
            let _ = img.save(out.join(format!("{id}.pane.png")));
            (1, took.as_millis())
        }
        None => (0, PANE_DEADLINE.as_millis()),
    };
    format!("{id}\t{thumb_ok}\t{tw}\t{th}\t{thumb_ms}\t{pane_ok}\t{pane_ms}\n")
}

#[test]
#[ignore = "driven by scripts/bigfiles/bigfiles.py with BIGFILES_PLAN / BIGFILES_OUT"]
fn big_files_through_the_shell_surfaces() {
    let Some((rows, out)) = plan() else {
        eprintln!("NOT MEASURED: BIGFILES_PLAN / BIGFILES_OUT not set");
        return;
    };
    unsafe {
        common::set_test_env("ST2K_SETTINGS_ROOT", TEST_SETTINGS_ROOT);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    std::fs::create_dir_all(&out).expect("output dir");
    let mut results = std::fs::File::create(out.join("results.tsv")).expect("results file");
    for (id, path) in rows {
        let line = run_one(&id, &path, &out);
        results.write_all(line.as_bytes()).expect("write a result");
        results.flush().expect("flush");
    }
}
