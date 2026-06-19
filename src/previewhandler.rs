//! The preview-pane handler: `IPreviewHandler` (+ `IInitializeWithStream`,
//! `IObjectWithSite`, `IPreviewHandlerVisuals`).
//!
//! Where the thumbnail provider returns a tiny `HBITMAP`, this renders the image
//! LARGE into Explorer's reading/preview pane. The shell hands us an `IStream`
//! (via `IInitializeWithStream`), a parent `HWND` + bounds (`SetWindow`), and a
//! themed background colour (`SetBackgroundColor`); on `DoPreview` we decode the
//! stream with the SAME tiered decoder the thumbnail path uses
//! (`decode::decode_preview` — so all 287 formats, ebook/comic covers, audio
//! waveforms, etc. work here too) and paint it, aspect-preserved, into a child
//! window.
//!
//! Crash isolation: a preview handler is loaded by the shell's OUT-OF-PROCESS
//! preview host (`prevhost.exe`) via its surrogate `AppID` (set in `register.rs`),
//! never inside `explorer.exe`. Every COM method funnels through `safety::guard`,
//! and the painting is plain GDI on an already-bounds-checked, already-bomb-capped
//! decoded buffer — bad input yields an empty pane, never a crash.

use core::cell::{Cell, RefCell};
use core::ffi::c_void;

use windows_implement::implement;
use windows::core::{Error, Interface, Ref, Result, GUID, IUnknown};
use windows::Win32::Foundation::{COLORREF, E_FAIL, E_POINTER, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleDC, CreateDIBSection, CreateSolidBrush, DeleteDC, DeleteObject,
    EndPaint, FillRect, InvalidateRect, SelectObject, SetStretchBltMode, StretchBlt, BITMAPINFO,
    BITMAPINFOHEADER, DIB_RGB_COLORS, HALFTONE, HBITMAP, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::Com::{IStream, STREAM_SEEK_SET};
use windows::Win32::System::Ole::{IObjectWithSite, IObjectWithSite_Impl};
use windows::Win32::UI::Shell::PropertiesSystem::{IInitializeWithStream, IInitializeWithStream_Impl};
use windows::Win32::UI::Shell::{
    IPreviewHandler, IPreviewHandler_Impl, IPreviewHandlerVisuals, IPreviewHandlerVisuals_Impl,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowLongPtrW, LoadCursorW,
    MoveWindow, RegisterClassW, SetWindowLongPtrW, ShowWindow, CS_HREDRAW, CS_VREDRAW,
    GWLP_USERDATA, IDC_ARROW, SW_SHOW, WINDOW_EX_STYLE, WM_ERASEBKGND, WM_NCDESTROY, WM_PAINT,
    WM_PRINTCLIENT, WNDCLASSW, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_VISIBLE,
};

use crate::{decode, safety};

/// Whole-stream read ceiling — shared with the thumbnail path's DoS budget.
const MAX_BYTES: usize = decode::limits::MAX_INPUT_BYTES as usize;

/// Wall-clock budget for a single preview decode, enforced OFF the host thread (see
/// [`decode_preview_budgeted`]) so a slow/exotic decode can never freeze prevhost's
/// message pump. The image/WIC fast tiers finish far under this; the ImageMagick
/// subprocess long tail is the only thing that can approach it. On expiry the pane
/// shows empty and the worker finishes + drops its result on its own. Sized above a
/// typical magick decode (~1–4s) so normal exotic previews still render, but well
/// under the ~20s the host could otherwise be frozen for.
const PREVIEW_DECODE_BUDGET: core::time::Duration = core::time::Duration::from_secs(12);

/// Our child window class name (registered once per process).
const CLASS_NAME: windows::core::PCWSTR = windows::core::w!("SageThumbs2KPreview");

/// Per-window paint state, owned via the child window's `GWLP_USERDATA`. Holds the
/// composited (over the host background colour) 32bpp DIB plus its source size, so
/// `WM_PAINT` is a plain aspect-fit `StretchBlt`.
struct RenderData {
    hbmp: HBITMAP,
    iw: i32,
    ih: i32,
    bg: u32,
}

#[implement(IInitializeWithStream, IObjectWithSite, IPreviewHandler, IPreviewHandlerVisuals)]
pub struct PreviewHandler {
    _ref: crate::ModuleRef,
    stream: RefCell<Option<IStream>>,
    site: RefCell<Option<IUnknown>>,
    parent: Cell<isize>, // host parent HWND (as isize, so the struct stays Cell-friendly)
    rect: Cell<RECT>,
    bg: Cell<u32>, // COLORREF value the host gave us (0x00BBGGRR)
    hwnd: Cell<isize>, // our child window
    /// Decoded RGBA cache, kept so a later `SetBackgroundColor` re-composites
    /// without re-decoding the stream.
    pixels: RefCell<Option<DecodedRgba>>,
}

struct DecodedRgba {
    w: u32,
    h: u32,
    rgba: Vec<u8>,
}

impl Default for PreviewHandler {
    #[allow(clippy::default_constructed_unit_structs)]
    fn default() -> Self {
        Self {
            _ref: crate::ModuleRef::default(),
            stream: RefCell::new(None),
            site: RefCell::new(None),
            parent: Cell::new(0),
            rect: Cell::new(RECT::default()),
            bg: Cell::new(0x00FF_FFFF), // white until the host themes us
            hwnd: Cell::new(0),
            pixels: RefCell::new(None),
        }
    }
}

impl IInitializeWithStream_Impl for PreviewHandler_Impl {
    fn Initialize(&self, pstream: Ref<'_, IStream>, _grfmode: u32) -> Result<()> {
        safety::guard(|| {
            let stream = pstream.ok()?;
            *self.stream.try_borrow_mut().map_err(|_| Error::from(E_FAIL))? = Some(stream.clone());
            Ok(())
        })
    }
}

impl IObjectWithSite_Impl for PreviewHandler_Impl {
    fn SetSite(&self, punksite: Ref<'_, IUnknown>) -> Result<()> {
        safety::guard(|| {
            // A null site clears it (the host detaching us); otherwise stash it.
            *self.site.borrow_mut() = punksite.ok().ok().cloned();
            Ok(())
        })
    }

    fn GetSite(&self, riid: *const GUID, ppvsite: *mut *mut c_void) -> Result<()> {
        safety::guard(|| unsafe {
            if ppvsite.is_null() {
                return Err(Error::from(E_POINTER));
            }
            *ppvsite = core::ptr::null_mut();
            match self.site.borrow().as_ref() {
                Some(s) => s.query(riid, ppvsite).ok(),
                None => Err(Error::from(E_FAIL)),
            }
        })
    }
}

impl IPreviewHandler_Impl for PreviewHandler_Impl {
    fn SetWindow(&self, hwnd: HWND, prc: *const RECT) -> Result<()> {
        safety::guard(|| {
            self.parent.set(hwnd.0 as isize);
            if !prc.is_null() {
                self.rect.set(unsafe { *prc });
            }
            self.reposition();
            Ok(())
        })
    }

    fn SetRect(&self, prc: *const RECT) -> Result<()> {
        safety::guard(|| {
            if !prc.is_null() {
                self.rect.set(unsafe { *prc });
            }
            self.reposition();
            Ok(())
        })
    }

    fn DoPreview(&self) -> Result<()> {
        safety::guard(|| {
            if !self.ensure_window() {
                return Err(Error::from(E_FAIL));
            }
            // Drain the shell's IStream on THIS thread: a stream marshaled into our
            // STA apartment can't be touched from a worker thread.
            let bytes = {
                let borrow = self.stream.borrow();
                let stream = borrow.as_ref().ok_or_else(|| Error::from(E_FAIL))?;
                unsafe { read_stream(stream, MAX_BYTES) }
            };
            // Decode OFF the host thread under a wall-clock budget. The fast tiers
            // return well under it; only the exotic long tail (an ImageMagick
            // subprocess, an OS video codec, a huge image) can approach it — and we
            // would rather paint an empty pane than freeze prevhost's message pump
            // waiting on it. Decode is best-effort either way: a failure OR a timeout
            // shows the themed-but-empty pane, never a loud host error.
            let decoded = bytes.and_then(decode_preview_budgeted);
            *self.pixels.borrow_mut() = decoded.map(|img| {
                let rgba = img.to_rgba8();
                let (w, h) = (rgba.width(), rgba.height());
                DecodedRgba { w, h, rgba: rgba.into_raw() }
            });
            self.rebuild();
            Ok(())
        })
    }

    fn Unload(&self) -> Result<()> {
        safety::guard(|| {
            self.destroy_window();
            *self.pixels.borrow_mut() = None;
            *self.stream.borrow_mut() = None;
            Ok(())
        })
    }

    fn SetFocus(&self) -> Result<()> {
        safety::guard(|| {
            let hwnd = self.child();
            if !hwnd.0.is_null() {
                unsafe { _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(hwnd)) };
            }
            Ok(())
        })
    }

    fn QueryFocus(&self) -> Result<HWND> {
        safety::guard_val(|| {
            let h = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() };
            Ok(h)
        })
    }

    fn TranslateAccelerator(&self, _pmsg: *const windows::Win32::UI::WindowsAndMessaging::MSG) -> Result<()> {
        // An image preview consumes no accelerators; S_FALSE = "not handled" so the
        // host keeps routing them (Tab out of the pane, etc.).
        Err(Error::from(windows::Win32::Foundation::S_FALSE))
    }
}

impl IPreviewHandlerVisuals_Impl for PreviewHandler_Impl {
    fn SetBackgroundColor(&self, color: COLORREF) -> Result<()> {
        safety::guard(|| {
            self.bg.set(color.0);
            // Re-composite from the cached pixels (no re-decode) so transparency sits
            // on the new themed colour.
            self.rebuild();
            Ok(())
        })
    }

    fn SetFont(&self, _plogfontw: *const windows::Win32::Graphics::Gdi::LOGFONTW) -> Result<()> {
        Ok(()) // images carry no text
    }

    fn SetTextColor(&self, _color: COLORREF) -> Result<()> {
        Ok(())
    }
}

impl PreviewHandler_Impl {
    fn child(&self) -> HWND {
        HWND(self.hwnd.get() as *mut c_void)
    }

    /// Create the child window if we have a parent and haven't already. Returns
    /// whether a usable window now exists.
    fn ensure_window(&self) -> bool {
        if !self.child().0.is_null() {
            return true;
        }
        let parent = HWND(self.parent.get() as *mut c_void);
        if parent.0.is_null() {
            return false;
        }
        ensure_class();
        let r = self.rect.get();
        let hinst = HINSTANCE(crate::dll_hmodule().0);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS_NAME,
                windows::core::w!(""),
                WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WS_CLIPCHILDREN,
                r.left,
                r.top,
                (r.right - r.left).max(0),
                (r.bottom - r.top).max(0),
                Some(parent),
                None,
                Some(hinst),
                None,
            )
        };
        match hwnd {
            Ok(h) => {
                self.hwnd.set(h.0 as isize);
                unsafe { _ = ShowWindow(h, SW_SHOW) };
                true
            }
            Err(_) => false,
        }
    }

    /// Move our child window to the current parent/rect (no-op until it exists).
    fn reposition(&self) {
        let hwnd = self.child();
        if hwnd.0.is_null() {
            return;
        }
        let r = self.rect.get();
        unsafe {
            _ = MoveWindow(hwnd, r.left, r.top, (r.right - r.left).max(0), (r.bottom - r.top).max(0), true);
        }
    }

    /// (Re)build the composited DIB from the cached pixels + current background and
    /// hand it to the window via `GWLP_USERDATA`, then repaint.
    fn rebuild(&self) {
        let hwnd = self.child();
        if hwnd.0.is_null() {
            return;
        }
        unsafe {
            // Free any previous render state first.
            let old = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RenderData;
            if !old.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let rd = Box::from_raw(old);
                _ = DeleteObject(rd.hbmp.into());
            }
            if let Some(px) = self.pixels.borrow().as_ref() {
                if let Some(hbmp) = make_dib(px.w as i32, px.h as i32, &px.rgba, self.bg.get()) {
                    let rd = Box::new(RenderData { hbmp, iw: px.w as i32, ih: px.h as i32, bg: self.bg.get() });
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(rd) as isize);
                }
            }
            _ = InvalidateRect(Some(hwnd), None, true);
        }
    }

    fn destroy_window(&self) {
        let hwnd = self.child();
        if !hwnd.0.is_null() {
            // WM_NCDESTROY frees the boxed RenderData + its HBITMAP.
            unsafe { _ = DestroyWindow(hwnd) };
            self.hwnd.set(0);
        }
    }
}

impl Drop for PreviewHandler {
    fn drop(&mut self) {
        // Defensive: the host should call Unload, but if final-release races it, tear
        // the window down here too (best-effort; a wrong-thread DestroyWindow no-ops).
        let hwnd = HWND(self.hwnd.get() as *mut c_void);
        if !hwnd.0.is_null() {
            unsafe { _ = DestroyWindow(hwnd) };
        }
    }
}

// ── window class + paint ──────────────────────────────────────────────────────

/// Register our child window class once per process.
fn ensure_class() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: HINSTANCE(crate::dll_hmodule().0),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        RegisterClassW(&wc); // ATOM 0 on failure is fine — DefWindowProc still applies
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            // Painting touches only GDI on a validated DIB; still guard so a freak
            // panic can't unwind across the system-driven callback.
            let _ = safety::guard_hr(|| {
                paint(hwnd);
                windows::Win32::Foundation::S_OK
            });
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1), // WM_PAINT fills the whole client itself
        WM_PRINTCLIENT => {
            // Render into the caller-supplied DC (PrintWindow / thumbnail capture).
            let hdc = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void);
            let mut rc = RECT::default();
            _ = GetClientRect(hwnd, &mut rc);
            draw(hwnd, hdc, &rc);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RenderData;
            if !p.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let rd = Box::from_raw(p);
                _ = DeleteObject(rd.hbmp.into());
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    if hdc.is_invalid() {
        return;
    }
    let mut rc = RECT::default();
    _ = GetClientRect(hwnd, &mut rc);
    draw(hwnd, hdc, &rc);
    _ = EndPaint(hwnd, &ps);
}

/// Paint the (background-filled, aspect-fit) image into `hdc` for the client `rc`.
/// Shared by `WM_PAINT` and `WM_PRINTCLIENT`.
unsafe fn draw(hwnd: HWND, hdc: windows::Win32::Graphics::Gdi::HDC, rc: &RECT) {
    let rd = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RenderData;
    let bg = if rd.is_null() { 0x00FF_FFFF } else { (*rd).bg };

    // Fill the whole client with the host background colour first.
    let brush = CreateSolidBrush(COLORREF(bg));
    FillRect(hdc, rc, brush);
    _ = DeleteObject(brush.into());

    if !rd.is_null() {
        let rd = &*rd;
        let cw = rc.right - rc.left;
        let ch = rc.bottom - rc.top;
        if cw > 0 && ch > 0 && rd.iw > 0 && rd.ih > 0 {
            // Aspect-preserving fit (scales up or down — preview panes show small
            // images large, unlike the never-upscale thumbnail path).
            let scale = f64::min(cw as f64 / rd.iw as f64, ch as f64 / rd.ih as f64);
            let dw = ((rd.iw as f64 * scale).round() as i32).max(1);
            let dh = ((rd.ih as f64 * scale).round() as i32).max(1);
            let dx = (cw - dw) / 2;
            let dy = (ch - dh) / 2;
            let memdc = CreateCompatibleDC(Some(hdc));
            let old = SelectObject(memdc, rd.hbmp.into());
            SetStretchBltMode(hdc, HALFTONE);
            _ = StretchBlt(hdc, dx, dy, dw, dh, Some(memdc), 0, 0, rd.iw, rd.ih, SRCCOPY);
            SelectObject(memdc, old);
            _ = DeleteDC(memdc);
        }
    }
}

/// Build a top-down 32bpp DIB of `rgba` composited over the opaque `bg`
/// (`COLORREF` 0x00BBGGRR), so painting is a plain `StretchBlt`. `None` on a
/// malformed size / allocation failure.
unsafe fn make_dib(iw: i32, ih: i32, rgba: &[u8], bg: u32) -> Option<HBITMAP> {
    if iw <= 0 || ih <= 0 {
        return None;
    }
    let px = (iw as usize).checked_mul(ih as usize)?;
    if rgba.len() < px.checked_mul(4)? {
        return None;
    }
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = iw;
    bmi.bmiHeader.biHeight = -ih; // top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = 0; // BI_RGB

    let mut bits: *mut c_void = core::ptr::null_mut();
    let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        _ = DeleteObject(hbmp.into());
        return None;
    }
    let (bg_r, bg_g, bg_b) = (bg & 0xFF, (bg >> 8) & 0xFF, (bg >> 16) & 0xFF);
    let dst = core::slice::from_raw_parts_mut(bits as *mut u8, px * 4);
    for i in 0..px {
        let r = rgba[i * 4] as u32;
        let g = rgba[i * 4 + 1] as u32;
        let b = rgba[i * 4 + 2] as u32;
        let a = rgba[i * 4 + 3] as u32;
        // out = (src*a + bg*(255-a)) / 255, rounded. Opaque pixels copy through.
        let comp = |s: u32, d: u32| (((s * a) + (d * (255 - a)) + 127) / 255) as u8;
        dst[i * 4] = comp(b, bg_b); // B
        dst[i * 4 + 1] = comp(g, bg_g); // G
        dst[i * 4 + 2] = comp(r, bg_r); // R
        dst[i * 4 + 3] = 255;
    }
    Some(hbmp)
}

/// Run [`decode::decode_preview`] on a detached worker thread, returning its result
/// only if it finishes within [`PREVIEW_DECODE_BUDGET`]. On timeout returns `None` and
/// leaves the worker running — it sends into a now-dropped channel (the send simply
/// errors) and exits on its own — so the calling host thread is blocked for at most the
/// budget. Safe off the apartment thread: `DynamicImage` is `Send` and the worker
/// touches only the pure decoder, no COM/GDI/HWND state.
fn decode_preview_budgeted(bytes: Vec<u8>) -> Option<image::DynamicImage> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(decode::decode_preview(&bytes).ok());
    });
    rx.recv_timeout(PREVIEW_DECODE_BUDGET).ok().flatten()
}

/// Drain an `IStream` into a `Vec`, bounded by `max`. Rewinds first. `None` on a
/// transport error or if the stream exceeds `max`.
unsafe fn read_stream(stream: &IStream, max: usize) -> Option<Vec<u8>> {
    _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut out: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 1 << 16];
    loop {
        let mut got: u32 = 0;
        let hr = stream.Read(chunk.as_mut_ptr() as *mut c_void, chunk.len() as u32, Some(&mut got));
        if hr.is_err() {
            return None;
        }
        if got == 0 {
            break;
        }
        let n = (got as usize).min(chunk.len());
        out.extend_from_slice(&chunk[..n]);
        if out.len() > max {
            return None;
        }
    }
    Some(out)
}
