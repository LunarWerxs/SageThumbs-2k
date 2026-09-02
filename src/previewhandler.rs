//! The preview-pane handler: `IPreviewHandler` (+ `IInitializeWithStream`,
//! `IObjectWithSite`, `IPreviewHandlerVisuals`).
//!
//! Where the thumbnail provider returns a tiny `HBITMAP`, this renders the image
//! LARGE into Explorer's reading/preview pane. The shell hands us an `IStream`
//! (via `IInitializeWithStream`), a parent `HWND` + bounds (`SetWindow`), and a
//! themed background colour (`SetBackgroundColor`); on `DoPreview` we acquire the
//! stream through the SAME streaming cascade the thumbnail path uses
//! ([`crate::streamsrc`] — video frame-grab tiers, seek-only album art, streamed
//! archive covers, the head-preview rescue, the bounded whole-file read) and
//! decode with the same tiered decoder (`decode::decode_preview` — so all
//! registered formats, ebook/comic covers, audio waveforms, etc. work here too),
//! then paint it, aspect-preserved, into a child window.
//!
//! Crash isolation: a preview handler is loaded by the shell's OUT-OF-PROCESS
//! preview host (`prevhost.exe`) via its surrogate `AppID` (set in `register.rs`),
//! never inside `explorer.exe`. Every COM method funnels through `safety::guard`,
//! and the painting is plain GDI on an already-bounds-checked, already-bomb-capped
//! decoded buffer — bad input yields an empty pane, never a crash.

use core::cell::{Cell, RefCell};
use core::ffi::c_void;

use windows::core::{Error, IUnknown, Interface, Ref, Result, GUID};
use windows::Win32::Foundation::{
    COLORREF, E_FAIL, E_POINTER, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleDC, CreateSolidBrush, DeleteDC, DeleteObject, EndPaint, FillRect,
    InvalidateRect, SelectObject, SetStretchBltMode, StretchBlt, HALFTONE, HBITMAP, PAINTSTRUCT,
    SRCCOPY,
};
use windows::Win32::System::Com::IStream;
use windows::Win32::System::Ole::{IObjectWithSite, IObjectWithSite_Impl};
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithStream, IInitializeWithStream_Impl,
};
use windows::Win32::UI::Shell::{
    IPreviewHandler, IPreviewHandlerVisuals, IPreviewHandlerVisuals_Impl, IPreviewHandler_Impl,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    GetParent, GetWindowLongPtrW, IsWindow, LoadCursorW, MoveWindow, PostMessageW, PostQuitMessage,
    RegisterClassW, SetWindowLongPtrW, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
    GWLP_USERDATA, IDC_ARROW, MSG, SW_SHOW, WINDOW_EX_STYLE, WM_APP, WM_ERASEBKGND, WM_NCDESTROY,
    WM_PAINT, WM_PRINTCLIENT, WNDCLASSW, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_VISIBLE,
};
use windows_implement::implement;

/// Posted to our preview window to ask its OWNING (dedicated UI) thread to destroy it on that
/// thread — a same-thread DestroyWindow that the thread's own message loop services instantly.
const WM_PREVIEW_CLOSE: u32 = WM_APP + 1;
/// Posted (with `lparam` = `Box::into_raw(Box<(DecodedRgba, bg)>)`) to hand a freshly-decoded
/// image to the window-owning UI thread, which builds the DIB + repaints THERE. Rendering must
/// happen on the window's own thread — doing the DIB build / RenderData swap from the COM thread
/// would race the UI thread's WM_PAINT (use-after-free of the old RenderData).
const WM_PREVIEW_RENDER: u32 = WM_APP + 2;

use crate::streamsrc::{self, StreamSource};
use crate::{decode, safety, settings, stream_name};

/// Decodes this host may have in flight at once (see [`safety::LeasePool`]). `prevhost`
/// hosts one pane, so a couple of slots cover a decode still running past its budget when
/// the next selection arrives; four leaves room for a host that runs several panes in one
/// process (the integration tests do), and a fifth selection while all four are stuck gets
/// an empty pane instead of a fifth detached worker.
const PREVIEW_DECODE_SLOTS: usize = 4;

/// A decode that never returns loses its slot after this long (five budgets), so a stalled
/// ImageMagick child or a hung read cannot blank the pane for the life of the host.
const PREVIEW_DECODE_LEASE_MS: u64 = 60_000;

static DECODE_SLOTS: safety::LeasePool<PREVIEW_DECODE_SLOTS> =
    safety::LeasePool::new(PREVIEW_DECODE_LEASE_MS);

/// Our child window class name (registered once per process).
const CLASS_NAME: windows::core::PCWSTR = windows::core::w!("SageThumbs2KPreview");

/// Background the preview pane uses for the current OS theme. The Windows file-open dialog hands
/// the preview pane WHITE even in a dark dialog, which left us glaring white; defaulting to the OS
/// theme (and refusing a host colour that conflicts with it — see `SetBackgroundColor`) makes the
/// pane and the letterbox around an aspect-fit image blend in. COLORREF (0x00BBGGRR); 0x202020 ≈
/// the Win11 dark content surface.
fn theme_default_bg() -> u32 {
    if safety::apps_use_dark_theme() {
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

#[implement(
    IInitializeWithStream,
    IObjectWithSite,
    IPreviewHandler,
    IPreviewHandlerVisuals
)]
pub struct PreviewHandler {
    _ref: crate::ModuleRef,
    stream: RefCell<Option<IStream>>,
    site: RefCell<Option<IUnknown>>,
    parent: Cell<isize>, // host parent HWND (as isize, so the struct stays Cell-friendly)
    rect: Cell<RECT>,
    bg: Cell<u32>,     // COLORREF value the host gave us (0x00BBGGRR)
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
            *self
                .stream
                .try_borrow_mut()
                .map_err(|_| Error::from(E_FAIL))? = Some(stream.clone());
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
            // BOTH out-params, not just the buffer. `query` forwards `riid` straight into
            // QueryInterface, which dereferences it, so a null `riid` from a careless host
            // is a null read inside our process. `factory::CreateInstance` already guards its
            // twin of this call; this site was the other half of the same finding and was
            // lost when this file was reverted, so it is spelled out rather than assumed.
            if ppvsite.is_null() || riid.is_null() {
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

            // Honor the user's MaxSize cap like the thumbnail path does (the streaming
            // tiers sidestep it for video/audio/archives, exactly as thumbnails do).
            // The EnableThumbs master switch is deliberately NOT consulted: the pane
            // is its own feature, already gated per-format by registration.
            let cfg = settings::thumb_settings();

            // Acquire the source through the shared cascade on THIS thread: a stream
            // marshaled into our STA apartment can't be touched from a worker thread.
            // Video gets a frame-grab (never buffering a multi-GB movie), audio a
            // seek-only album-art read, oversized archives/.blend/PSD a streamed
            // cover / head prefix — everything else a bounded whole-file read.
            let source = {
                let borrow = self.stream.borrow();
                let stream = borrow.as_ref().ok_or_else(|| Error::from(E_FAIL))?;
                if let Some(name) = unsafe { stream_name(stream) } {
                    safety::log_debug(&format!("DoPreview: file {name}"));
                }
                // 1024 px matches the PDF/contact-sheet rasterize target below —
                // crisp at any pane size, and it is what the streaming EXR tier
                // scales to as it reads.
                unsafe {
                    streamsrc::stream_source(
                        stream,
                        &cfg,
                        crate::safety::PREVIEW_TARGET_EDGE,
                        "DoPreview",
                    )
                }
            };

            // A cascade miss (oversized past every rescue, artless audio, undecodable
            // video) leaves the pane empty — same terminal state as a failed decode. Each
            // miss leaves one always-on `ERROR` line naming the file, so a "blank pane"
            // report can be read from the log without `Debug=1`.
            let decoded = match source {
                // A video frame arrives already decoded by Media Foundation.
                Ok(StreamSource::Frame(frame)) => Some(frame),
                // Decode bytes OFF the host thread under a wall-clock budget so a
                // slow/exotic decode can't freeze the preview host's message pump.
                Ok(StreamSource::Bytes(bytes)) => {
                    let len = bytes.len();
                    safety::log_debug(&format!("DoPreview: read {len} bytes from stream"));
                    match decode_preview_budgeted(bytes) {
                        Ok(img) => Some(img),
                        Err(why) => {
                            safety::log_error(&format!(
                                "DoPreview: decode failed for {} ({len} bytes): {why}",
                                self.stream_label()
                            ));
                            None
                        }
                    }
                }
                // A generic archive's contact sheet: the covers are ordinary
                // JPEG/PNG members decoded by the CHEAP tiers only (no subprocess,
                // no video/PDF), so no wall-clock budget is needed. The pane's edge
                // matches the PDF rasterize target — crisp at any pane size.
                Ok(StreamSource::Covers(covers)) => {
                    safety::log_debug(&format!("DoPreview: {} archive covers", covers.len()));
                    match decode::thumbnail_from_covers(&covers, safety::PREVIEW_TARGET_EDGE) {
                        Ok(d) => image::RgbaImage::from_raw(d.width, d.height, d.rgba)
                            .map(image::DynamicImage::ImageRgba8),
                        Err(e) => {
                            safety::log_error(&format!(
                                "DoPreview: contact sheet failed for {} hr={:#010x}",
                                self.stream_label(),
                                e.code().0
                            ));
                            None
                        }
                    }
                }
                Err(e) => {
                    safety::log_error(&format!(
                        "DoPreview: stream_source failed for {} hr={:#010x}",
                        self.stream_label(),
                        e.code().0
                    ));
                    None
                }
            };
            match &decoded {
                Some(img) => safety::log_debug(&format!(
                    "DoPreview: decoded {}x{}",
                    img.width(),
                    img.height()
                )),
                None => safety::log_debug("DoPreview: decode failed/timed out -> blank pane"),
            }
            *self.pixels.borrow_mut() = decoded.map(|img| {
                let rgba = img.to_rgba8();
                let (w, h) = (rgba.width(), rgba.height());
                DecodedRgba {
                    w,
                    h,
                    rgba: rgba.into_raw(),
                }
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

    fn TranslateAccelerator(
        &self,
        _pmsg: *const windows::Win32::UI::WindowsAndMessaging::MSG,
    ) -> Result<()> {
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
            let bg = if colorref_is_light(color.0) != safety::apps_use_dark_theme() {
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

    /// The stream's reported name for a log line, or a placeholder when it has none.
    /// `try_borrow`: a log line must never turn into a `RefCell` panic under `panic = "abort"`.
    fn stream_label(&self) -> String {
        self.stream
            .try_borrow()
            .ok()
            .and_then(|b| b.as_ref().and_then(|s| unsafe { stream_name(s) }))
            .unwrap_or_else(|| "<unnamed stream>".to_string())
    }

    /// Create the child window if we have a parent and don't already have a LIVE one.
    /// Returns whether a usable window now exists.
    ///
    /// "Live" is the load-bearing word. Holding a non-null `hwnd` is not proof the window
    /// still exists: the host can destroy our child out from under us — it owns the parent,
    /// and it recycles the pane after the preview sits idle — WITHOUT calling `Unload`.
    /// Trusting the stale handle meant `post_render` posted to a dead window, `PostMessageW`
    /// failed, the payload was freed, and the pane silently kept showing the PREVIOUS file:
    /// "it missed the refresh a couple of times" after half an hour idle (issue #11). The
    /// same reasoning covers a re-parent: a child of a window we are no longer inside can
    /// never paint into the visible pane, so it is just as dead for our purposes.
    fn ensure_window(&self) -> bool {
        let existing = self.child();
        if !existing.0.is_null() {
            let alive = unsafe { IsWindow(Some(existing)) }.as_bool();
            let same_parent = unsafe { GetParent(existing) }
                .map(|p| p.0 as isize == self.parent.get())
                .unwrap_or(false);
            if alive && same_parent {
                return true;
            }
            // Stale: drop the handle and reap the old UI thread before building a new one,
            // so we never accumulate threads across host recycles.
            crate::safety::log_debug(
                "preview: child window went stale (host recycled the pane) - rebuilding",
            );
            self.hwnd.set(0);
            if alive {
                unsafe { _ = PostMessageW(Some(existing), WM_PREVIEW_CLOSE, WPARAM(0), LPARAM(0)) };
            }
            if let Some(h) = self.ui_thread.borrow_mut().take() {
                let _ = h.join();
            }
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
        // `Builder::spawn`, never `thread::spawn`: the latter panics when the OS refuses a
        // thread, which under `panic = "abort"` takes prevhost down; here it is an empty pane.
        let spawned = std::thread::Builder::new()
            .name("st2k-preview-ui".to_string())
            .spawn(move || {
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
        let handle = match spawned {
            Ok(h) => h,
            Err(e) => {
                safety::log_error(&format!("preview: could not start the UI thread: {e}"));
                return false;
            }
        };
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
            _ = MoveWindow(
                hwnd,
                r.left,
                r.top,
                (r.right - r.left).max(0),
                (r.bottom - r.top).max(0),
                true,
            );
            _ = InvalidateRect(Some(hwnd), None, true);
        }
    }

    /// Hand the cached decoded pixels to the window-OWNING UI thread, which builds the composited
    /// DIB + repaints THERE. The DIB build / RenderData swap MUST happen on the window's own thread:
    /// doing it from the COM thread would race the UI thread's WM_PAINT (use-after-free of the old
    /// RenderData). No-op until the window exists.
    ///
    /// With NO pixels (decode failed or blew the wall-clock budget) we post a NULL payload, which
    /// tells the UI thread to drop what it is showing and repaint empty. Returning early here
    /// instead would leave the PREVIOUS file's image on screen: the host reuses one handler across
    /// selections (`Initialize` + `DoPreview` again, no `Unload` in between), so a miss on file B
    /// left file A up and the pane looked frozen (issue #11).
    fn post_render(&self) {
        let hwnd = self.child();
        if hwnd.0.is_null() {
            return;
        }
        // Clone the pixels into a heap payload the UI thread takes ownership of (and frees). Keeping
        // `self.pixels` lets a later SetBackgroundColor re-composite without re-decoding.
        let payload = self.pixels.borrow().as_ref().map(|px| {
            Box::new((
                DecodedRgba {
                    w: px.w,
                    h: px.h,
                    rgba: px.rgba.clone(),
                },
                self.bg.get(),
            ))
        });
        let raw = match payload {
            Some(b) => Box::into_raw(b),
            None => core::ptr::null_mut(),
        };
        unsafe {
            if PostMessageW(
                Some(hwnd),
                WM_PREVIEW_RENDER,
                WPARAM(0),
                LPARAM(raw as isize),
            )
            .is_err()
                && !raw.is_null()
            {
                // The window died between the child() check and here — the message will never be
                // processed (and so never reclaim the Box). Free it now so it doesn't leak.
                drop(Box::from_raw(raw));
            }
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
            // Guarded exactly like the WM_PAINT arm above. This is the SAME `draw` reached
            // by a different system-driven callback (PrintWindow / thumbnail capture), so
            // leaving it bare meant a panic that WM_PAINT would have contained instead
            // unwound across the callback and aborted the host.
            let _ = safety::guard_hr(|| {
                draw(hwnd, hdc, &rc);
                windows::Win32::Foundation::S_OK
            });
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
            // Drop what we are showing FIRST, unconditionally. A NULL lparam means the new
            // selection produced no image, and the pane must then go EMPTY: keeping the previous
            // file's pixels up is exactly what "the preview stopped refreshing" looks like when
            // the host reuses one handler across selections (issue #11).
            let old = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RenderData;
            if !old.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let rd = Box::from_raw(old);
                _ = DeleteObject(rd.hbmp.into());
            }
            let p = lparam.0 as *mut (DecodedRgba, u32);
            if !p.is_null() {
                let (dec, bg) = *Box::from_raw(p);
                // `opaque: None`: nothing upstream has scanned the alpha channel, so the
                // shared compositor works it out itself (the same scan the private copy did).
                let hbmp =
                    safety::composite_rgba_over_bg(dec.w as i32, dec.h as i32, &dec.rgba, bg, None);
                if let Some(hbmp) = hbmp {
                    let rd = Box::new(RenderData {
                        hbmp,
                        iw: dec.w as i32,
                        ih: dec.h as i32,
                        bg,
                    });
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(rd) as isize);
                }
            }
            _ = InvalidateRect(Some(hwnd), None, true);
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
    let bg = if rd.is_null() {
        theme_default_bg()
    } else {
        (*rd).bg
    };

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
            _ = StretchBlt(
                hdc,
                dx,
                dy,
                dw,
                dh,
                Some(memdc),
                0,
                0,
                rd.iw,
                rd.ih,
                SRCCOPY,
            );
            SelectObject(memdc, old);
            _ = DeleteDC(memdc);
        }
    }
}

/// Run [`decode::decode_preview_capped`] on a budgeted worker (see
/// [`safety::spawn_budgeted`]), returning the image only if it finishes within
/// [`safety::PREVIEW_DECODE_BUDGET`]. The `Err` text names which of the three ways it can
/// fail happened (no free slot, the decoder's own error, the budget expiring) so `DoPreview`
/// can log it. On expiry the worker keeps running on its own, holding its DLL pin and its
/// [`DECODE_SLOTS`] lease until it finishes or the lease runs out; the host thread is blocked
/// for at most the budget. Safe off the apartment thread: `DynamicImage` is `Send` and the
/// worker touches only the pure decoder, no GDI/HWND state.
fn decode_preview_budgeted(bytes: Vec<u8>) -> std::result::Result<image::DynamicImage, String> {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
    // The lease is taken HERE and moved into the worker, so a refused `Builder::spawn` drops
    // it (freeing the slot) exactly like a normal worker exit would. A burst of selections
    // waits for a slot rather than being refused on the spot: the wait is bounded by one
    // decode budget (the slot holders are bounded by the same budget, and a hung one loses
    // its lease), so the worker cap still holds and only a pane whose earlier decodes are
    // genuinely stuck ends up empty.
    let slot_deadline = std::time::Instant::now() + safety::PREVIEW_DECODE_BUDGET;
    let lease = loop {
        if let Some(lease) = DECODE_SLOTS.acquire() {
            break lease;
        }
        if std::time::Instant::now() >= slot_deadline {
            return Err(format!(
                "all {PREVIEW_DECODE_SLOTS} decode slots are held by earlier decodes still running"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let outcome = safety::spawn_budgeted(
        "st2k-preview-decode",
        safety::PREVIEW_DECODE_BUDGET,
        move || {
            let _lease = lease;
            // This worker MUST hold a COM apartment: the WIC decode tier (HEIC / camera-RAW /
            // JPEG-XR — exactly the phone-photo & camera formats) calls `CoCreateInstance` and
            // fails with `CoInitialize has not been called (0x800401F0)` on a bare thread.
            // When that happened the preview came up BLANK (white pane) and fell through to
            // the slow ImageMagick subprocess — a pegged core for nothing. MTA matches the
            // shell's own out-of-process thumbnail host and the apartment the video (Media
            // Foundation) and PDF (WinRT) tiers self-init, so every tier resolves here.
            // Balance `CoUninitialize` only when we actually took a ref (S_OK/S_FALSE);
            // `RPC_E_CHANGED_MODE` did not. Mirrors the per-worker guard in `parallel.rs`.
            let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
            // Cap what the decoders render at the pane's own target (the same edge the
            // stream cascade scales to). Without it a 76 MP JPEG 2000 spent 15.6s producing
            // a 4096px surface we immediately threw away, blew the budget, and left the pane
            // blank on a perfectly good file (issue #11).
            let out = decode::decode_preview_capped(&bytes, safety::PREVIEW_TARGET_EDGE)
                .map_err(|e| e.to_string());
            // `out` is a plain `DynamicImage`; all WIC/MF objects are already dropped inside
            // the decoder, so the apartment holds no live COM ref at teardown.
            if inited {
                unsafe { CoUninitialize() };
            }
            out
        },
    );
    outcome.unwrap_or_else(|| {
        Err(format!(
            "decode did not finish within {:?} (or the host is over its abandoned-worker cap)",
            safety::PREVIEW_DECODE_BUDGET
        ))
    })
}
