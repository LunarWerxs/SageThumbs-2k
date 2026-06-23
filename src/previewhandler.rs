//! The preview-pane handler: `IPreviewHandler` (+ `IInitializeWithStream`,
//! `IObjectWithSite`, `IPreviewHandlerVisuals`).
//!
//! Where the thumbnail provider returns a tiny `HBITMAP`, this renders the image
//! LARGE into Explorer's reading/preview pane. The shell hands us an `IStream`
//! (via `IInitializeWithStream`), a parent `HWND` + bounds (`SetWindow`), and a
//! themed background colour (`SetBackgroundColor`); on `DoPreview` we decode the
//! stream with the SAME tiered decoder the thumbnail path uses
//! (`decode::decode_preview` — so all registered formats, ebook/comic covers, audio
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
use windows::Win32::System::Com::{
    CoTaskMemFree, IStream, STATFLAG_DEFAULT, STATSTG, STREAM_SEEK_SET,
};
use windows::Win32::System::Ole::{IObjectWithSite, IObjectWithSite_Impl};
use windows::Win32::UI::Shell::PropertiesSystem::{IInitializeWithStream, IInitializeWithStream_Impl};
use windows::Win32::UI::Shell::{
    IPreviewHandler, IPreviewHandler_Impl, IPreviewHandlerVisuals, IPreviewHandlerVisuals_Impl,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetMessageW, GetWindowLongPtrW, LoadCursorW, MoveWindow, PostMessageW, PostQuitMessage,
    RegisterClassW, SetWindowLongPtrW, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
    GWLP_USERDATA, IDC_ARROW, MSG, SW_SHOW, WINDOW_EX_STYLE, WM_APP, WM_ERASEBKGND,
    WM_NCDESTROY, WM_PAINT, WM_PRINTCLIENT, WNDCLASSW, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS,
    WS_VISIBLE,
};

/// Posted to our preview window to ask its OWNING (dedicated UI) thread to destroy it on that
/// thread — a same-thread DestroyWindow that the thread's own message loop services instantly.
const WM_PREVIEW_CLOSE: u32 = WM_APP + 1;
/// Posted (with `lparam` = `Box::into_raw(Box<(DecodedRgba, bg)>)`) to hand a freshly-decoded
/// image to the window-owning UI thread, which builds the DIB + repaints THERE. Rendering must
/// happen on the window's own thread — doing the make_dib / RenderData swap from the COM thread
/// would race the UI thread's WM_PAINT (use-after-free of the old RenderData).
const WM_PREVIEW_RENDER: u32 = WM_APP + 2;

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

/// True when the OS *app* theme is dark (`AppsUseLightTheme == 0`).
fn theme_is_dark() -> bool {
    windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_u32("AppsUseLightTheme"))
        .map(|v| v == 0)
        .unwrap_or(false)
}

/// Background the preview pane uses for the current OS theme. The Windows file-open dialog hands
/// the preview pane WHITE even in a dark dialog, which left us glaring white; defaulting to the OS
/// theme (and refusing a host colour that conflicts with it — see `SetBackgroundColor`) makes the
/// pane and the letterbox around an aspect-fit image blend in. COLORREF (0x00BBGGRR); 0x202020 ≈
/// the Win11 dark content surface.
fn theme_default_bg() -> u32 {
    if theme_is_dark() {
        0x0020_2020
    } else {
        0x00FF_FFFF
    }
}

/// Perceived-light test for a COLORREF (0x00BBGGRR): average channel above mid.
fn colorref_is_light(c: u32) -> bool {
    let (r, g, b) = (c & 0xFF, (c >> 8) & 0xFF, (c >> 16) & 0xFF);
    (r + g + b) / 3 > 128
}

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
    hwnd: Cell<isize>, // our child window (owned by `ui_thread`)
    /// The DEDICATED UI thread that creates, OWNS, and pumps messages for the preview window.
    /// prevhost's COM apartment thread does NOT pump window messages while idle, so a window on
    /// it takes ~133s to tear down cross-process on dialog close (measured). A thread we own +
    /// pump services that WM_DESTROY instantly. Joined in `destroy_window`.
    ui_thread: RefCell<Option<std::thread::JoinHandle<()>>>,
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
            bg: Cell::new(theme_default_bg()), // match the OS theme until the host themes us
            hwnd: Cell::new(0),
            ui_thread: RefCell::new(None),
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
            let site = punksite.ok().ok().cloned();
            // A null site = the host is DETACHING us (it does this as the dialog tears down).
            // Destroy our child window NOW, on THIS (the window-owning prevhost STA) thread — a
            // fast, same-thread destroy. If we leave it, the host then destroys the pane and our
            // window gets torn down CROSS-PROCESS, which times out for ~2 minutes (the hang).
            if site.is_none() {
                self.destroy_window();
            }
            *self.site.borrow_mut() = site;
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
                if let Some(name) = unsafe { stream_name(stream) } {
                    safety::log_debug(&format!("DoPreview: file {name}"));
                }
                unsafe { read_stream(stream, MAX_BYTES) }
            };
            safety::log_debug(&format!(
                "DoPreview: read {} bytes from stream",
                bytes.as_ref().map_or(0, |b| b.len())
            ));

            // Decode OFF the host thread under a wall-clock budget so a slow/exotic decode can't
            // freeze the preview host's message pump; a failure/timeout leaves the pane empty.
            let decoded = bytes.and_then(decode_preview_budgeted);
            match &decoded {
                Some(img) => {
                    safety::log_debug(&format!("DoPreview: decoded {}x{}", img.width(), img.height()))
                }
                None => safety::log_debug("DoPreview: decode failed/timed out -> blank pane"),
            }
            *self.pixels.borrow_mut() = decoded.map(|img| {
                let rgba = img.to_rgba8();
                let (w, h) = (rgba.width(), rgba.height());
                DecodedRgba { w, h, rgba: rgba.into_raw() }
            });
            // Hand the decoded pixels to the window-owning UI thread, which builds the DIB + paints
            // there (rendering on this COM thread would race the UI thread's WM_PAINT).
            self.post_render();
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
            // Honor the host's colour ONLY when it agrees with the OS theme. The Windows file-open
            // dialog hands the preview pane WHITE even in a dark dialog; on that conflict (dark OS +
            // light colour, or vice-versa) we keep our themed background so the pane blends in
            // instead of glaring. A host that themes correctly (its colour matches the OS theme)
            // still wins, so we pick up its exact shade when it bothers to be right.
            // "Agrees with the theme" = light colour in light mode, or dark colour in dark mode,
            // i.e. host-is-light XOR theme-is-dark is false → the two booleans differ. (`a != b`,
            // which clippy prefers over the equivalent `a == !b`.)
            let bg = if colorref_is_light(color.0) != theme_is_dark() {
                color.0
            } else {
                theme_default_bg()
            };
            self.bg.set(bg);
            // Re-composite from the cached pixels (no re-decode) so transparency + the letterbox
            // sit on the chosen colour.
            self.post_render();
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
        let parent_isize = self.parent.get();
        if parent_isize == 0 {
            return false;
        }
        let r = self.rect.get();
        let hinst_isize = crate::dll_hmodule().0 as isize;
        let (tx, rx) = std::sync::mpsc::channel::<isize>();
        // Create + OWN the preview window on a DEDICATED UI thread whose own GetMessage loop pumps
        // its messages — including the cross-process WM_DESTROY when the dialog closes — so teardown
        // is INSTANT instead of the ~133s timeout caused by prevhost's idle COM thread never pumping
        // window messages (measured). The thread holds a `ModuleRef`, pinning the DLL for the whole
        // window+thread lifetime (the wndproc lives in this DLL), so it can't unload underneath it.
        let handle = std::thread::spawn(move || {
            #[allow(clippy::default_constructed_unit_structs)]
            let _module = crate::ModuleRef::default();
            ensure_class();
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
                    Some(HWND(parent_isize as *mut c_void)),
                    None,
                    Some(HINSTANCE(hinst_isize as *mut c_void)),
                    None,
                )
            };
            match hwnd {
                Ok(h) => {
                    unsafe { _ = ShowWindow(h, SW_SHOW) };
                    let _ = tx.send(h.0 as isize);
                    // Pump THIS window's messages until it's destroyed (WM_NCDESTROY posts WM_QUIT).
                    let mut msg = MSG::default();
                    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
                        unsafe {
                            _ = TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }
                }
                Err(_) => {
                    let _ = tx.send(0);
                }
            }
        });
        let hwnd = rx.recv().unwrap_or(0);
        if hwnd == 0 {
            return false;
        }
        self.hwnd.set(hwnd);
        *self.ui_thread.borrow_mut() = Some(handle);
        true
    }

    /// Move our child window to the current parent/rect (no-op until it exists).
    fn reposition(&self) {
        let hwnd = self.child();
        if hwnd.0.is_null() {
            return;
        }
        let r = self.rect.get();
        unsafe {
            // MoveWindow + InvalidateRect are cross-thread (COM thread -> UI-thread-owned window),
            // both fine. The host calls SetWindow with a tiny/zero rect FIRST, then SetRect with the
            // real pane size; the dedicated UI thread PUMPS, so the resulting WM_PAINT is delivered
            // and the (already-attached) image repaints at the new size. No forced UpdateWindow
            // needed any more — that was a workaround for prevhost's non-pumping COM thread.
            _ = MoveWindow(hwnd, r.left, r.top, (r.right - r.left).max(0), (r.bottom - r.top).max(0), true);
            _ = InvalidateRect(Some(hwnd), None, true);
        }
    }

    /// Hand the cached decoded pixels to the window-OWNING UI thread, which builds the composited
    /// DIB + repaints THERE. The make_dib / RenderData swap MUST happen on the window's own thread:
    /// doing it from the COM thread would race the UI thread's WM_PAINT (use-after-free of the old
    /// RenderData). No-op until the window exists / there's something to show.
    fn post_render(&self) {
        let hwnd = self.child();
        if hwnd.0.is_null() {
            return;
        }
        // Clone the pixels into a heap payload the UI thread takes ownership of (and frees). Keeping
        // `self.pixels` lets a later SetBackgroundColor re-composite without re-decoding.
        let payload = match self.pixels.borrow().as_ref() {
            Some(px) => Box::new((DecodedRgba { w: px.w, h: px.h, rgba: px.rgba.clone() }, self.bg.get())),
            None => return,
        };
        unsafe {
            _ = PostMessageW(
                Some(hwnd),
                WM_PREVIEW_RENDER,
                WPARAM(0),
                LPARAM(Box::into_raw(payload) as isize),
            );
        }
    }

    fn destroy_window(&self) {
        let hwnd = self.child();
        if !hwnd.0.is_null() {
            // Post to the UI thread so IT calls DestroyWindow on its own window (same-thread, fast).
            // Its loop then ends (WM_NCDESTROY -> PostQuitMessage). PostMessage is thread-safe.
            unsafe { _ = PostMessageW(Some(hwnd), WM_PREVIEW_CLOSE, WPARAM(0), LPARAM(0)) };
            self.hwnd.set(0);
        }
        // Join the UI thread so its window is fully gone before we return (and its ModuleRef drops).
        let handle = self.ui_thread.borrow_mut().take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

impl Drop for PreviewHandler {
    fn drop(&mut self) {
        // The host should call Unload, but on final-release tear the window down too: ask the UI
        // thread to destroy its window, then join it so the window is gone (and its ModuleRef
        // dropped) before this object dies. PostMessage to a dead window is a harmless no-op.
        let hwnd = HWND(self.hwnd.get() as *mut c_void);
        if !hwnd.0.is_null() {
            unsafe { _ = PostMessageW(Some(hwnd), WM_PREVIEW_CLOSE, WPARAM(0), LPARAM(0)) };
        }
        if let Some(h) = self.ui_thread.borrow_mut().take() {
            let _ = h.join();
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
            // The window is gone — end its dedicated UI thread's message loop. (The thread's
            // ModuleRef then drops, letting the DLL unload.)
            PostQuitMessage(0);
            LRESULT(0)
        }
        // Our own "close" request: the COM thread asks us (the window-owning UI thread) to destroy
        // the window on THIS thread — a same-thread DestroyWindow the loop services instantly.
        WM_PREVIEW_CLOSE => {
            _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        // Fresh decoded image handed over from the COM thread (lparam = Box<(DecodedRgba, bg)>).
        // Build the composited DIB + swap the RenderData HERE (this thread owns the window), then
        // invalidate — the loop pumps WM_PAINT next, so it actually paints (no cross-thread race).
        WM_PREVIEW_RENDER => {
            let p = lparam.0 as *mut (DecodedRgba, u32);
            if !p.is_null() {
                let (dec, bg) = *Box::from_raw(p);
                let old = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RenderData;
                if !old.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    let rd = Box::from_raw(old);
                    _ = DeleteObject(rd.hbmp.into());
                }
                if let Some(hbmp) = make_dib(dec.w as i32, dec.h as i32, &dec.rgba, bg) {
                    let rd = Box::new(RenderData { hbmp, iw: dec.w as i32, ih: dec.h as i32, bg });
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(rd) as isize);
                }
                _ = InvalidateRect(Some(hwnd), None, true);
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
    // No image yet / decode failed: fill with the themed default rather than hardcoded white.
    let bg = if rd.is_null() { theme_default_bg() } else { (*rd).bg };

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
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // Pin the DLL for this detached worker's whole lifetime. On a timeout we return but
        // leave this thread running, and `MODULE_REFS`/`DllCanUnloadNow` does NOT count it —
        // so when the host releases the preview object (the dialog CLOSING), the DLL could
        // unload while this thread is still executing its code → crash-on-close. Mirrors
        // `verbs::actions::run_action_detached`. `ModuleRef::default()`'s side effect IS the
        // `dll_add_ref`; clippy's "use `ModuleRef`" would skip it.
        #[allow(clippy::default_constructed_unit_structs)]
        let _module = crate::ModuleRef::default();
        // This worker MUST hold a COM apartment: the WIC decode tier (HEIC / camera-RAW /
        // JPEG-XR — exactly the phone-photo & camera formats) calls `CoCreateInstance` and
        // fails with `CoInitialize has not been called (0x800401F0)` on a bare thread. When
        // that happened the preview came up BLANK (white pane) and fell through to the slow
        // ImageMagick subprocess — a pegged core for nothing. MTA matches the shell's own
        // out-of-process thumbnail host and the apartment the video (Media Foundation) and
        // PDF (WinRT) tiers self-init, so every tier resolves here. Balance `CoUninitialize`
        // only when we actually took a ref (S_OK/S_FALSE); `RPC_E_CHANGED_MODE` did not.
        // Mirrors the per-worker guard in `parallel.rs` / `propstore.rs`.
        let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let out = decode::decode_preview(&bytes).ok();
        // `out` is a plain `DynamicImage`; all WIC/MF objects are already dropped inside
        // `decode_preview`, so the apartment holds no live COM ref at teardown.
        if inited {
            unsafe { CoUninitialize() };
        }
        let _ = tx.send(out);
    });
    rx.recv_timeout(PREVIEW_DECODE_BUDGET).ok().flatten()
}

/// Best-effort backing file name/path of the shell's preview `IStream` (via `IStream::Stat`,
/// `STATFLAG_DEFAULT` fills `pwcsName`). Logged so a "white preview" report names the exact
/// file. `pwcsName` is a CoTaskMem allocation we own and must free.
unsafe fn stream_name(stream: &IStream) -> Option<String> {
    let mut stat = STATSTG::default();
    stream.Stat(&mut stat, STATFLAG_DEFAULT).ok()?;
    if stat.pwcsName.is_null() {
        return None;
    }
    let s = stat.pwcsName.to_string().ok();
    CoTaskMemFree(Some(stat.pwcsName.0 as *const c_void));
    s
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

#[cfg(test)]
mod tests {
    use super::make_dib;
    use windows::Win32::Graphics::Gdi::{DeleteObject, GetObjectW, HBITMAP, BITMAP};

    /// Read the top-left BGRA quad out of a DIB-section HBITMAP, then free it.
    unsafe fn first_px(hbmp: HBITMAP) -> [u8; 4] {
        let mut bm = BITMAP::default();
        let n = GetObjectW(
            hbmp.into(),
            core::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut core::ffi::c_void),
        );
        assert!(n != 0 && !bm.bmBits.is_null(), "make_dib must produce a real DIB section");
        let px = core::slice::from_raw_parts(bm.bmBits as *const u8, 4);
        let out = [px[0], px[1], px[2], px[3]];
        let _ = DeleteObject(hbmp.into());
        out
    }

    /// Untrusted decoded dimensions must be rejected (None), never deref/overflow —
    /// this runs in prevhost on attacker-influenced sizes.
    #[test]
    fn make_dib_rejects_bad_dims_without_crashing() {
        unsafe {
            assert!(make_dib(0, 5, &[0u8; 64], 0).is_none(), "zero width");
            assert!(make_dib(5, 0, &[0u8; 64], 0).is_none(), "zero height");
            assert!(make_dib(-3, 4, &[0u8; 64], 0).is_none(), "negative width");
            assert!(make_dib(2, 2, &[0u8; 4], 0).is_none(), "buffer too short (2x2 needs 16 bytes)");
            assert!(make_dib(i32::MAX, i32::MAX, &[0u8; 4], 0).is_none(), "w*h overflow guard");
        }
    }

    /// The alpha-over-background compositing math (the bit `WM_PAINT` later StretchBlts).
    /// `bg` is a COLORREF 0x00BBGGRR; 0x00FF_0000 is opaque blue.
    #[test]
    fn make_dib_composites_alpha_over_background() {
        unsafe {
            // Opaque red over blue copies straight through -> BGRA [0,0,255,255].
            let red = make_dib(1, 1, &[255, 0, 0, 255], 0x00FF_0000).unwrap();
            assert_eq!(first_px(red), [0, 0, 255, 255], "opaque red");

            // 50% red over blue: R ≈ 200*128/255 ≈ 100, B ≈ 255*127/255 ≈ 127.
            let half = make_dib(1, 1, &[200, 0, 0, 128], 0x00FF_0000).unwrap();
            let [b, g, r, a] = first_px(half);
            assert_eq!((g, a), (0, 255), "no green; DIB opaque");
            assert!((r as i32 - 100).abs() <= 2, "R composited ~100, got {r}");
            assert!((b as i32 - 127).abs() <= 2, "B composited ~127, got {b}");
        }
    }
}
