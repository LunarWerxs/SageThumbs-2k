//! The viewer window: a borderless, resizable, dark/DPI-aware popup with a slim custom
//! caption + toolbar. Owns the wndproc, all painting, hit-testing (drag/resize), sizing,
//! key handling, the toolbar actions, and the `WM_COPYDATA` command handling. Content
//! painting is delegated to [`super::content`] (images) and [`super::infocard`].

use std::cell::{Cell, RefCell};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, InvalidateRect, MonitorFromWindow, ScreenToClient, HDC, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
    VK_CONTROL, VK_DOWN, VK_END, VK_ESCAPE, VK_F11, VK_HOME, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RETURN,
    VK_RIGHT, VK_SHIFT, VK_SPACE, VK_UP,
};
use windows::Win32::UI::Shell::{ShellExecuteW, StrCmpLogicalW};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::content::{self, RenderData};
use super::selection::{self, SelHit};
use super::{infocard, parse_command, CMD_CLOSE, CMD_SET_PATH, CMD_TOGGLE, VIEWER_CLASS};
use super::{loader::*, paint::*, toolbar::*, transport::*};

/// Decode result posted from the worker (`WM_APP + 1`); LPARAM = `Box<(gen, Option<DecodedRgba>)>`.
pub(super) const WM_APP_RENDER: u32 = WM_APP + 1;
/// Animated-image frames posted from the worker (`WM_APP + 7`);
/// LPARAM = `Box<(gen, Vec<(DecodedRgba, delay_ms)>)>`.
pub(super) const WM_APP_ANIM: u32 = WM_APP + 7;
/// PDF page count posted from the worker (`WM_APP + 8`); LPARAM = `Box<(gen, page_count)>`.
pub(super) const WM_APP_PDFINFO: u32 = WM_APP + 8;
/// A fetched remote markdown image (`WM_APP + 9`); LPARAM = `Box<(gen, src, Option<DecodedRgba>)>`.
pub(super) const WM_APP_MDIMG: u32 = WM_APP + 9;
/// Follow-selection switch posted from the poll thread (`WM_APP + 2`); LPARAM = `Box<String>` path.
pub(super) const WM_APP_SWITCH: u32 = WM_APP + 2;
/// Timer that shows the window even if the decode hasn't finished (so we never wait hidden).
pub(super) const SHOW_TIMER_ID: usize = 1;
/// Ticks ~4x/sec while a video plays to repaint the scrub position.
pub(super) const SCRUB_TIMER_ID: usize = 2;
/// Fires per animation frame (re-armed to the next frame's delay).
pub(super) const ANIM_TIMER_ID: usize = 3;
/// Outline-sidebar slide animation tick (~7 frames over ~100ms).
pub(super) const TOC_TIMER_ID: usize = 4;
/// Ignore Toggle/Close COMMANDS for this long after (re)open, so a key-repeat or an
/// immediate key-up race can't close a window that just appeared (plan §3, `SETTLE_CLOSE_MS`).
pub(super) const SETTLE_CLOSE_MS: u64 = 400;

// Layout, 96-dpi design px.
pub(super) const CAPTION_H: i32 = 36;
pub(super) const BTN_W: i32 = 38;
pub(super) const PAD: i32 = 6;
pub(super) const MIN_W: i32 = 400;
pub(super) const MIN_H: i32 = 200;
pub(super) const LOADING_W: i32 = 720;
pub(super) const LOADING_H: i32 = 480;
pub(super) const CARD_W: i32 = 460;
pub(super) const CARD_H: i32 = 200;
pub(super) const TEXT_W: i32 = 1000; // text/code/markdown-source default (matches the plan's md size)
pub(super) const TEXT_H: i32 = 640;
pub(super) const VIDEO_W: i32 = 960; // video default (16:9; the engine letterboxes to the real aspect)
pub(super) const VIDEO_H: i32 = 540;
pub(super) const SCRUB_H: i32 = 40; // video transport strip height (play/pause + seek + time + volume)

/// How the current file is being presented.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum ContentKind {
    Loading,
    Image,
    Text,
    Markdown,
    Video,
    InfoCard,
    /// A WebView2-hosted local HTML page or live `.url` (feature `html-preview`). The webview child
    /// renders itself over the content area; the viewer only owns its bounds. Only constructed with
    /// the feature on, but the paint/size match arms reference it either way.
    #[cfg_attr(not(feature = "html-preview"), allow(dead_code))]
    Html,
}

/// Caption toolbar buttons. `PdfPrev`/`PdfNext` only show for multi-page PDFs (see
/// [`btn_visible`]).
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Btn {
    Toc,
    /// "View source" toggle: swap a RENDERED document (Markdown, a CSV/TSV/notebook table, a
    /// WebView2 HTML page, an SVG) for its raw text, and back. Only shown when the current file
    /// actually has both views (see [`btn_visible`] / `loader::source_capable`).
    Source,
    PdfPrev,
    PdfNext,
    Pin,
    Copy,
    Info,
    Upload,
    Open,
    OpenWith,
    Close,
}

/// All buttons, in left-to-right caption order (rightmost drawn is Close).
pub(super) const BTNS: [Btn; 11] = [
    Btn::Toc,
    Btn::Source,
    Btn::PdfPrev,
    Btn::PdfNext,
    Btn::Pin,
    Btn::Copy,
    Btn::Info,
    Btn::Upload,
    Btn::OpenWith,
    Btn::Open,
    Btn::Close,
];

/// Whether a toolbar button is currently shown (PDF pager only for multi-page PDFs; the outline
/// toggle only for Markdown that has headings; the source toggle only for files that HAVE a
/// rendered view to toggle away from).
pub(super) fn btn_visible(st: &ViewerState, b: Btn) -> bool {
    match b {
        Btn::PdfPrev | Btn::PdfNext => {
            st.kind.get() == ContentKind::Image && st.pdf_pages.get() > 1
        }
        Btn::Toc => st.kind.get() == ContentKind::Markdown && st.md_has_headings.get(),
        Btn::Source => st.src_capable.get(),
        _ => true,
    }
}

pub(super) struct ViewerState {
    /// Manual mode = launched by hand with the daemon hook OFF (`preview_enabled()` false),
    /// so the viewer owns its own Space/Esc/Enter. When the hook is the authority (Phase 2),
    /// the viewer does NOT handle those keys locally (plan §3).
    pub(super) manual: bool,
    pub(super) shot: bool,
    pub(super) path: RefCell<Option<String>>,
    pub(super) kind: Cell<ContentKind>,
    pub(super) render: RefCell<Option<RenderData>>,
    /// Animated-image frames (empty for static content). When non-empty the Image path shows
    /// `frames[cur_frame]` and cycles them on `ANIM_TIMER_ID`.
    pub(super) frames: RefCell<Vec<RenderData>>,
    pub(super) frame_delays: RefCell<Vec<u32>>,
    pub(super) cur_frame: Cell<usize>,
    /// PDF page navigation: current 0-based page + total page count (0 = not a multi-page PDF).
    pub(super) pdf_page: Cell<u32>,
    pub(super) pdf_pages: Cell<u32>,
    pub(super) card: RefCell<Option<infocard::InfoCard>>,
    pub(super) text: RefCell<Option<String>>,
    pub(super) video: RefCell<Option<super::video::VideoPlayer>>,
    pub(super) hinst: HINSTANCE,
    pub(super) pinned: Cell<bool>,
    /// "Open in front": bring the window to the top of the z-order on first show (without
    /// stealing focus). Distinct from `pinned` (always-on-top); a front window can be covered.
    pub(super) open_front: Cell<bool>,
    pub(super) born: Cell<u64>,
    pub(super) shown: Cell<bool>,
    /// Bumped on every (re)load; a `WM_APP_RENDER` with a stale gen is dropped.
    pub(super) decode_gen: Cell<u64>,
    pub(super) hot: Cell<Option<usize>>, // index into BTNS currently hovered
    /// The caption toolbar's tooltip control (one RECT tool per button); `HWND::default()` if none.
    pub(super) tip: Cell<HWND>,
    /// Whether the 500 ms follow-selection poll thread has been started (daemon mode only).
    pub(super) poll_started: Cell<bool>,
    // ----- Phase 4 viewer polish -----
    /// Image zoom RELATIVE TO FIT: 1.0 = aspect-fit (the default). Wheel + double-click drive it.
    pub(super) zoom: Cell<f64>,
    /// Image pan offset in device px (0,0 = centered). Drag-to-pan when zoomed.
    pub(super) pan: Cell<(i32, i32)>,
    /// Active pan drag anchor: `(mouse_x, mouse_y, pan_x, pan_y)` captured at button-down.
    pub(super) drag: Cell<Option<(i32, i32, i32, i32)>>,
    /// Video transport: dragging the seek track / the volume slider.
    pub(super) scrub_drag: Cell<bool>,
    pub(super) vol_drag: Cell<bool>,
    /// Text preview vertical scroll offset (device px from the top).
    pub(super) text_scroll: Cell<i32>,
    /// Last-measured total text height (device px) — the wheel handler clamps scroll to it.
    pub(super) text_h: Cell<i32>,
    /// Active custom-scrollbar drag: cursor offset from the top of the thumb at button-down.
    pub(super) scroll_drag: Cell<Option<i32>>,
    /// A held click on the scrollbar track (used to swallow the matching button-up).
    pub(super) scroll_page_press: Cell<bool>,
    /// Whether the pointer is over the custom scrollbar lane (for hover feedback).
    pub(super) scroll_hot: Cell<bool>,
    /// Unconsumed high-resolution wheel delta; a full Windows wheel notch is 120 units.
    pub(super) wheel_remainder: Cell<i32>,
    /// Selection: `(anchor, focus)` RAW byte offsets into the active selection document —
    /// `text` for the Text pane, the Markdown pane's rendered text (see [`super::selection`]).
    /// Unordered (the anchor is where the drag started); equal offsets = no selection. Cleared
    /// on every load. [`sel_range`] normalizes it for painting/copying.
    pub(super) sel: Cell<Option<(usize, usize)>>,
    /// A mouse text-selection drag is active (mouse capture held).
    pub(super) sel_drag: Cell<bool>,
    /// Byte offset of each line start in `text` (first entry 0). Built lazily by the first
    /// selection hit-test, cleared on load — so per-mouse-move hit-testing never rescans a
    /// multi-MB document for line boundaries. Text kind only.
    pub(super) line_starts: RefCell<Vec<usize>>,
    /// Every text token the last Markdown paint DREW: its rect + the slice of the rendered
    /// document it shows. Markdown is a wrapped proportional flow with no line grid, so this is
    /// what selection hit-tests against (visible tokens only — the document itself is complete).
    pub(super) md_hits: RefCell<Vec<SelHit>>,
    /// Clickable link rects from the last Markdown paint (client coords, current scroll). Empty
    /// for non-Markdown content; repopulated every paint, consumed by click/hover hit-testing.
    pub(super) md_links: RefCell<Vec<super::markdown::LinkHit>>,
    /// Markdown heading outline (table of contents) from the last render — drives the sidebar.
    pub(super) md_toc: RefCell<Vec<super::markdown::TocEntry>>,
    /// Sidebar entry hit rects (client coords) → outline index, for click-to-jump/select.
    pub(super) toc_hits: RefCell<Vec<(RECT, usize)>>,
    /// Whether the Markdown outline sidebar is open (persisted via `preview_toc_open`).
    pub(super) toc_open: Cell<bool>,
    /// Explicitly-clicked outline entry. Overrides the scroll-derived "current section"
    /// highlight — so clicking a bottom section that CAN'T scroll to the pane top still
    /// visibly selects it. Cleared when the user scrolls (or a new file loads).
    pub(super) toc_sel: Cell<Option<usize>>,
    /// Mid-slide sidebar width (device px) while the open/close animation runs; `None` when
    /// settled (paint derives the settled width from `toc_open`).
    pub(super) toc_anim: Cell<Option<i32>>,
    /// Whether the current Markdown document has any headings — computed ONCE at load (a full
    /// parse), so the toolbar toggle and the sidebar don't re-parse per paint or go stale on
    /// file switches.
    pub(super) md_has_headings: Cell<bool>,
    /// Per-document inline-image cache (markdown `![]()` / raw `<img>`): src -> slot
    /// (Pending fetch / Failed → alt-text pill / Ready DIB). Cleared on every load;
    /// `RenderData::drop` frees the bitmaps.
    pub(super) md_imgs: RefCell<super::markdown::ImgCache>,
    /// Markdown layout cache (measured text-block heights) so scrolling a big document skips
    /// re-measuring off-screen paragraphs each paint. Rebuilt on doc/width/remote change.
    pub(super) md_layout: RefCell<super::markdown::MdLayout>,
    /// The remote-images toggle, read once at load (like the HTML toggles) so a mid-preview
    /// Settings save can't flip behavior between paints of the same document.
    pub(super) md_remote_ok: Cell<bool>,
    /// "View source" mode: show the raw file text instead of the rendered document. Sticky for
    /// the LIFETIME OF THE WINDOW (survives ←/→ nav and daemon file switches, so you can read a
    /// run of documents as source) but never persisted — a fresh preview always opens rendered.
    pub(super) src_view: Cell<bool>,
    /// Whether the current file HAS both a rendered and a source view — computed once per load
    /// from the extension + the Settings toggles that decide whether it renders at all (see
    /// `loader::source_capable`). Drives the toolbar toggle's visibility.
    pub(super) src_capable: Cell<bool>,
    /// Full-screen state: `Some(pre_fullscreen_window_rect)` while borderless-full-screen (F11),
    /// `None` otherwise. Saving the windowed rect lets F11/Esc restore the exact prior geometry.
    pub(super) fullscreen: Cell<Option<RECT>>,
    /// True while a synchronous WebView2 create is pumping the message loop (see `webview::create`).
    /// During this window the wndproc must NOT destroy the window or re-enter `load()` — doing so
    /// would free/replace the state under the still-running create. Close/switch requests made while
    /// busy are stashed in `pending_close`/`pending_path` and applied once the create returns.
    pub(super) busy: Cell<bool>,
    /// A close requested while `busy` — applied after the WebView2 create returns.
    pub(super) pending_close: Cell<bool>,
    /// A file-switch requested while `busy` — applied (last-wins) after the create returns.
    pub(super) pending_path: RefCell<Option<String>>,
    /// The live WebView2 host for `ContentKind::Html` (feature `html-preview`); `None` otherwise.
    #[cfg(feature = "html-preview")]
    pub(super) webview: RefCell<Option<super::webview::WebViewHost>>,
}

/// Pull the state pointer out of `GWLP_USERDATA`.
pub(super) unsafe fn state(hwnd: HWND) -> *const ViewerState {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const ViewerState
}

/// Close the viewer, but DEFER the destroy if a WebView2 create is currently pumping the message
/// loop (destroying now would free `ViewerState` under the still-running create → use-after-free).
/// The deferred close is applied by `loader::create_web` once the create returns.
pub(super) unsafe fn request_close(hwnd: HWND) {
    let st = &*state(hwnd);
    if st.busy.get() {
        st.pending_close.set(true);
    } else {
        let _ = DestroyWindow(hwnd);
    }
}

/// Switch to `path`, but DEFER (last-wins) if a WebView2 create is pumping (re-entering `load`
/// would reset/replace state under the outer create). Applied by `create_web` after the create.
pub(super) unsafe fn request_load(hwnd: HWND, path: &str) {
    let st = &*state(hwnd);
    if st.busy.get() {
        *st.pending_path.borrow_mut() = Some(path.to_string());
    } else {
        load(hwnd, path);
    }
}

/// Register the viewer window class once.
unsafe fn ensure_class(hinst: HINSTANCE) {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: VIEWER_CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS, // CS_DBLCLKS: double-click to fit/100%
            ..Default::default()
        };
        RegisterClassW(&wc);
    });
}

/// Create the viewer window (hidden). For `shot`, decode synchronously and place off-screen
/// so a `PrintWindow` capture can grab it; otherwise start the async decode + a show-fallback
/// timer and let the caller run the message loop.
pub(super) unsafe fn create_viewer(
    hinst: HINSTANCE,
    dark: bool,
    initial_path: Option<String>,
    shot: Option<&super::ShotOpts>,
) -> Option<HWND> {
    ensure_class(hinst);

    // Live previews open UN-pinned — the toolbar Pin button is the only always-on-top path.
    // (The `--pinned` shot flag still forces the pinned look for the headless glyph capture.)
    let pinned = match shot {
        Some(o) => o.pinned,
        None => false,
    };
    // "Open in front": bring the window to the top of the z-order on first show (never steals
    // focus, always coverable). Not applicable to the off-screen shot window.
    let open_front = shot.is_none() && sagethumbs2k_core::settings::preview_open_front();
    let manual = shot.is_none() && !sagethumbs2k_core::settings::preview_enabled();
    let ex = if pinned {
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST
    } else {
        WS_EX_TOOLWINDOW
    };
    let style = WS_POPUP | WS_THICKFRAME | WS_CLIPCHILDREN;

    let hwnd = CreateWindowExW(
        ex,
        VIEWER_CLASS,
        w!("SageThumbs 2K"),
        style,
        0,
        0,
        LOADING_W,
        LOADING_H,
        None,
        None,
        Some(hinst),
        None,
    )
    .ok()?;

    let st = Box::new(ViewerState {
        manual,
        shot: shot.is_some(),
        path: RefCell::new(None),
        kind: Cell::new(ContentKind::Loading),
        render: RefCell::new(None),
        frames: RefCell::new(Vec::new()),
        frame_delays: RefCell::new(Vec::new()),
        cur_frame: Cell::new(0),
        pdf_page: Cell::new(0),
        pdf_pages: Cell::new(0),
        card: RefCell::new(None),
        text: RefCell::new(None),
        video: RefCell::new(None),
        hinst,
        pinned: Cell::new(pinned),
        open_front: Cell::new(open_front),
        born: Cell::new(GetTickCount64()),
        shown: Cell::new(false),
        decode_gen: Cell::new(0),
        hot: Cell::new(None),
        tip: Cell::new(HWND::default()),
        poll_started: Cell::new(false),
        zoom: Cell::new(1.0),
        pan: Cell::new((0, 0)),
        drag: Cell::new(None),
        scrub_drag: Cell::new(false),
        vol_drag: Cell::new(false),
        text_scroll: Cell::new(0),
        text_h: Cell::new(0),
        scroll_drag: Cell::new(None),
        scroll_page_press: Cell::new(false),
        scroll_hot: Cell::new(false),
        wheel_remainder: Cell::new(0),
        sel: Cell::new(None),
        sel_drag: Cell::new(false),
        line_starts: RefCell::new(Vec::new()),
        md_hits: RefCell::new(Vec::new()),
        md_links: RefCell::new(Vec::new()),
        md_toc: RefCell::new(Vec::new()),
        toc_hits: RefCell::new(Vec::new()),
        toc_open: Cell::new(sagethumbs2k_core::settings::preview_toc_open()),
        toc_sel: Cell::new(None),
        toc_anim: Cell::new(None),
        md_has_headings: Cell::new(false),
        md_imgs: RefCell::new(super::markdown::ImgCache::new()),
        md_layout: RefCell::new(super::markdown::MdLayout::default()),
        md_remote_ok: Cell::new(false),
        // The headless shot can open straight into source view (`--shot --window preview --source`).
        src_view: Cell::new(shot.map(|o| o.source).unwrap_or(false)),
        src_capable: Cell::new(false),
        fullscreen: Cell::new(None),
        busy: Cell::new(false),
        pending_close: Cell::new(false),
        pending_path: RefCell::new(None),
        #[cfg(feature = "html-preview")]
        webview: RefCell::new(None),
    });
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(st) as isize);
    if dark {
        crate::dark::dark_titlebar(hwnd);
    }

    if let Some(opts) = shot {
        load_sync(hwnd, initial_path.as_deref(), opts);
    } else {
        (*state(hwnd)).tip.set(create_tooltips(hwnd, hinst));
        if let Some(p) = initial_path {
            load(hwnd, &p);
        }
        SetTimer(Some(hwnd), SHOW_TIMER_ID, 120, None);
    }
    Some(hwnd)
}

/// The letterbox / content background as a raw `COLORREF` u32.
pub(super) fn letterbox_bg(st: &ViewerState) -> u32 {
    let _ = st;
    crate::dark::SURFACE().0
}

/// Set the window title text to the current file's leaf name (used by tools reading the title).
pub(super) unsafe fn set_title(hwnd: HWND) {
    let st = &*state(hwnd);
    let name = st
        .path
        .borrow()
        .as_ref()
        .and_then(|p| {
            std::path::Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "SageThumbs 2K".to_string());
    let w = crate::win::wide(&name);
    let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
}

/// (x, y) from a mouse `LPARAM` (signed 16-bit halves).
pub(super) fn lparam_xy(lparam: LPARAM) -> (i32, i32) {
    (
        (lparam.0 & 0xFFFF) as i16 as i32,
        ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
    )
}

/// The link URL (if any) under the client-space point, from the last Markdown paint. Only
/// Markdown content records link rects.
unsafe fn hit_link(hwnd: HWND, x: i32, y: i32) -> Option<String> {
    let st = &*state(hwnd);
    if st.kind.get() != ContentKind::Markdown {
        return None;
    }
    st.md_links
        .borrow()
        .iter()
        .find(|h| x >= h.rect.left && x < h.rect.right && y >= h.rect.top && y < h.rect.bottom)
        .map(|h| h.url.clone())
}

/// The outline-sidebar entry index (if any) under the client-space point, from the last paint.
unsafe fn hit_toc(hwnd: HWND, x: i32, y: i32) -> Option<usize> {
    let st = &*state(hwnd);
    if st.kind.get() != ContentKind::Markdown {
        return None;
    }
    st.toc_hits
        .borrow()
        .iter()
        .find(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
        .map(|(_, idx)| *idx)
}

/// Open a clicked Markdown link. Allow-list: http(s) + mailto only, no control chars — a rendered
/// `.md` must not be able to launch `file://` / an exe / a custom protocol handler from a click.
unsafe fn open_preview_link(hwnd: HWND, url: &str) {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    let ok = (lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:"))
        && !u.bytes().any(|b| b < 0x20);
    if !ok {
        return;
    }
    let w = crate::win::wide(u);
    let _ = ShellExecuteW(
        Some(hwnd),
        w!("open"),
        PCWSTR(w.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        // Any message dispatched before GWLP_USERDATA is set (the synchronous WM_NCCREATE /
        // WM_CREATE / WM_GETMINMAXINFO that fire DURING CreateWindowExW, before we store the
        // state pointer) has no state — hand it to DefWindowProc rather than deref null. Every
        // state-touching arm below is thus guaranteed a live pointer. WM_DESTROY always still
        // has its state (it's zeroed inside that handler), so this never skips teardown.
        if state(hwnd).is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        match msg {
            WM_NCHITTEST => {
                // Native thick frame handles resize; make the caption strip draggable.
                let hit = DefWindowProcW(hwnd, msg, wparam, lparam);
                if hit.0 == HTCLIENT as isize {
                    let (sx, sy) = lparam_xy(lparam);
                    let mut pt = POINT { x: sx, y: sy };
                    let _ = ScreenToClient(hwnd, &mut pt);
                    let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
                    if pt.y < cap && hit_button(hwnd, pt.x, pt.y).is_none() {
                        return LRESULT(HTCAPTION as isize);
                    }
                }
                hit
            }
            WM_GETMINMAXINFO => {
                let mmi = &mut *(lparam.0 as *mut MINMAXINFO);
                mmi.ptMinTrackSize.x = crate::win::dpi_scale(hwnd, MIN_W);
                mmi.ptMinTrackSize.y = crate::win::dpi_scale(hwnd, MIN_H);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1), // WM_PAINT fills the whole client; skip the erase flash
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_PRINTCLIENT => {
                paint_into(hwnd, HDC(wparam.0 as *mut _));
                LRESULT(0)
            }
            WM_TIMER => {
                if wparam.0 == SHOW_TIMER_ID {
                    let _ = KillTimer(Some(hwnd), SHOW_TIMER_ID);
                    let st = state(hwnd);
                    if !st.is_null() && !(*st).shown.get() {
                        ensure_shown(hwnd);
                    }
                } else if wparam.0 == SCRUB_TIMER_ID {
                    let st = &*state(hwnd);
                    if st.kind.get() == ContentKind::Video {
                        // repaint ONLY the strip (never the video child) so the tick can't flicker
                        let sr = scrub_rect(hwnd);
                        let _ = InvalidateRect(Some(hwnd), Some(&sr), false);
                    }
                } else if wparam.0 == ANIM_TIMER_ID {
                    advance_frame(hwnd);
                } else if wparam.0 == TOC_TIMER_ID {
                    tick_toc_anim(hwnd);
                }
                LRESULT(0)
            }
            WM_APP_RENDER => {
                on_render(hwnd, wparam, lparam);
                LRESULT(0)
            }
            WM_APP_ANIM => {
                on_anim(hwnd, lparam);
                LRESULT(0)
            }
            WM_APP_MDIMG => {
                // A remote markdown image landed: install it (stale gen / wrong kind → drop).
                let boxed = Box::from_raw(
                    lparam.0 as *mut (u64, String, Option<super::content::DecodedRgba>),
                );
                let (gen, src, dec) = *boxed;
                let st = &*state(hwnd);
                if gen == st.decode_gen.get() && st.kind.get() == ContentKind::Markdown {
                    let slot = match dec.and_then(|d| {
                        super::content::make_dib(d.w, d.h, &d.rgba, crate::dark::SURFACE().0).map(
                            |hbmp| super::content::RenderData {
                                hbmp,
                                iw: d.w,
                                ih: d.h,
                            },
                        )
                    }) {
                        Some(rd) => super::markdown::ImgSlot::Ready(rd),
                        None => super::markdown::ImgSlot::Failed,
                    };
                    st.md_imgs.borrow_mut().insert(src, slot);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_APP_PDFINFO => {
                let boxed = Box::from_raw(lparam.0 as *mut (u64, u32));
                let (gen, count) = *boxed;
                let st = &*state(hwnd);
                if gen == st.decode_gen.get() {
                    // Cap the UNTRUSTED count (a crafted PDF can report > i32::MAX pages, which
                    // would wrap the nav math negative and panic a clamp — panic=abort).
                    st.pdf_pages.set(count.min(1_000_000));
                    update_tooltips(hwnd, st.tip.get()); // the pager buttons just (dis)appeared
                    let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
                    let mut r = RECT::default();
                    let _ = GetClientRect(hwnd, &mut r);
                    r.bottom = cap;
                    let _ = InvalidateRect(Some(hwnd), Some(&r), false); // repaint the page indicator + pager
                }
                LRESULT(0)
            }
            m if m == super::video::WM_APP_VIDEO => {
                let st = &*state(hwnd);
                if let Some(p) = st.video.borrow().as_ref() {
                    p.on_event(wparam.0 as u32); // CANPLAY -> autoplay, etc.
                }
                LRESULT(0)
            }
            WM_SIZE => {
                let st = &*state(hwnd);
                if let Some(p) = st.video.borrow().as_ref() {
                    p.place(&video_rect(hwnd)); // child fills content minus the scrub strip
                }
                #[cfg(feature = "html-preview")]
                if let Some(w) = st.webview.borrow().as_ref() {
                    w.place(&content_rect(hwnd)); // webview fills the content area
                }
                // The visible height changed. Clamp immediately using the last measured document
                // height; the next paint clamps once more if Markdown reflow changes that height.
                let _ = clamp_text_scroll(hwnd);
                update_tooltips(hwnd, st.tip.get()); // buttons are right-anchored — re-track them
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_APP_SWITCH => {
                // The follow-selection poll saw a new selection: switch to it (unless it's
                // already what we're showing).
                let path = *Box::from_raw(lparam.0 as *mut String);
                let st = &*state(hwnd);
                if st.path.borrow().as_deref() != Some(path.as_str()) {
                    request_load(hwnd, &path);
                }
                LRESULT(0)
            }
            WM_ACTIVATE => {
                // Close-on-focus-loss (opt-in setting; never when pinned; not during the open
                // grace so a just-shown, never-activated window can't self-close).
                let st = &*state(hwnd);
                if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE
                    && !st.pinned.get()
                    && GetTickCount64().saturating_sub(st.born.get()) >= SETTLE_CLOSE_MS
                    && sagethumbs2k_core::settings::preview_close_on_focus_loss()
                {
                    request_close(hwnd);
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = lparam_xy(lparam);
                let st = &*state(hwnd);
                // Active drag of the custom text/Markdown scrollbar thumb.
                if let Some(grab_y) = st.scroll_drag.get() {
                    drag_text_scroll_thumb(hwnd, y, grab_y);
                    return LRESULT(0);
                }
                // A track click captures until button-up so it cannot turn into a content click
                // if the pointer moves away. Native auto-repeat is intentionally not emulated.
                if st.scroll_page_press.get() {
                    let _ = set_scroll_hot(hwnd, hit_text_scrollbar(hwnd, x, y).is_some());
                    return LRESULT(0);
                }
                // Active seek / volume drag on the video strip.
                if st.scrub_drag.get() || st.vol_drag.get() {
                    let sr = scrub_rect(hwnd);
                    let (_, track, vol) = scrub_parts(hwnd, &sr);
                    if let Some(v) = st.video.borrow().as_ref() {
                        if st.scrub_drag.get() {
                            apply_seek(v, x, &track);
                        } else {
                            apply_vol(v, x, &vol);
                        }
                    }
                    let _ = InvalidateRect(Some(hwnd), Some(&sr), false);
                    return LRESULT(0);
                }
                // Active text-selection drag: extend to the cursor, auto-scrolling past the
                // pane edges so a drag can select beyond the viewport. Hit-test BEFORE
                // scrolling — the offset must match the frame the user is looking at (and the
                // Markdown rects are from that paint); the next move picks up the new scroll.
                if st.sel_drag.get() {
                    if let Some(off) = selection::hit(hwnd, x, y) {
                        if let Some((a, _)) = st.sel.get() {
                            st.sel.set(Some((a, off)));
                        }
                    }
                    let c = content_rect(hwnd);
                    let overshoot = if y < c.top {
                        y - c.top
                    } else if y > c.bottom {
                        y - c.bottom
                    } else {
                        0
                    };
                    if overshoot != 0 {
                        let step_cap = crate::win::dpi_scale(hwnd, 40);
                        selection::scroll_by(hwnd, overshoot.clamp(-step_cap, step_cap));
                    }
                    let _ = InvalidateRect(Some(hwnd), Some(&c), false);
                    return LRESULT(0);
                }
                // Active pan drag: move the image with the cursor.
                if let Some((ax, ay, apx, apy)) = st.drag.get() {
                    st.pan.set((apx + (x - ax), apy + (y - ay)));
                    clamp_pan(hwnd);
                    let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
                    let mut r = RECT::default();
                    let _ = GetClientRect(hwnd, &mut r);
                    r.top = cap;
                    let _ = InvalidateRect(Some(hwnd), Some(&r), false);
                    return LRESULT(0);
                }
                let now = hit_button(hwnd, x, y);
                let button_changed = now != st.hot.get();
                if button_changed {
                    st.hot.set(now);
                    let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
                    let mut r = RECT::default();
                    let _ = GetClientRect(hwnd, &mut r);
                    r.bottom = cap;
                    let _ = InvalidateRect(Some(hwnd), Some(&r), false);
                }
                let scroll_changed = set_scroll_hot(hwnd, hit_text_scrollbar(hwnd, x, y).is_some());
                if button_changed || scroll_changed {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: core::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                let st = &*state(hwnd);
                if st.hot.get().is_some() {
                    st.hot.set(None);
                    let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
                    let mut r = RECT::default();
                    let _ = GetClientRect(hwnd, &mut r);
                    r.bottom = cap;
                    let _ = InvalidateRect(Some(hwnd), Some(&r), false);
                }
                let _ = set_scroll_hot(hwnd, false);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let (x, y) = lparam_xy(lparam);
                if let Some(i) = hit_button(hwnd, x, y) {
                    do_action(hwnd, BTNS[i]);
                } else {
                    let st = &*state(hwnd);
                    let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
                    if let Some(hit) = hit_text_scrollbar(hwnd, x, y) {
                        let _ = set_scroll_hot(hwnd, true);
                        match hit {
                            TextScrollHit::Thumb(grab_y) => {
                                // The thumb is owner-drawn, so explicitly capture the mouse and
                                // map subsequent pointer movement back to the document range.
                                st.scroll_drag.set(Some(grab_y));
                            }
                            TextScrollHit::Page(dy) => {
                                let _ = scroll_text_by(hwnd, dy);
                                st.scroll_page_press.set(true);
                            }
                        }
                        invalidate_text_scrollbar(hwnd); // pressed feedback
                        let _ = SetCapture(hwnd);
                    } else if st.kind.get() == ContentKind::Video {
                        scrub_mouse_down(hwnd, x, y);
                    } else if y >= cap && st.kind.get() == ContentKind::Image && st.zoom.get() > 1.0
                    {
                        // In the content area, over a zoomed image → begin a pan drag.
                        let (px, py) = st.pan.get();
                        st.drag.set(Some((x, y, px, py)));
                        let _ = SetCapture(hwnd);
                    } else if y >= cap
                        && selection::selectable(st.kind.get())
                        && hit_toc(hwnd, x, y).is_none()
                    {
                        // In a text/Markdown pane (not the outline sidebar) → begin a selection
                        // drag, anchored at the hit. A drag starting on a Markdown link is fine:
                        // the link only opens if the button comes up with nothing selected.
                        if let Some(off) = selection::hit(hwnd, x, y) {
                            st.sel.set(Some((off, off)));
                            st.sel_drag.set(true);
                            let _ = SetCapture(hwnd);
                            let cr = content_rect(hwnd);
                            let _ = InvalidateRect(Some(hwnd), Some(&cr), false);
                        }
                    }
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let st = &*state(hwnd);
                if st.scroll_drag.get().is_some() || st.scroll_page_press.get() {
                    st.scroll_drag.set(None);
                    st.scroll_page_press.set(false);
                    let _ = ReleaseCapture();
                    let (x, y) = lparam_xy(lparam);
                    let _ = set_scroll_hot(hwnd, hit_text_scrollbar(hwnd, x, y).is_some());
                    invalidate_text_scrollbar(hwnd); // pressed → hover/idle feedback
                } else if st.scrub_drag.get() || st.vol_drag.get() {
                    st.scrub_drag.set(false);
                    st.vol_drag.set(false);
                    let _ = ReleaseCapture();
                } else if st.drag.get().is_some() {
                    st.drag.set(None);
                    let _ = ReleaseCapture();
                } else if st.sel_drag.get() {
                    st.sel_drag.set(false);
                    let _ = ReleaseCapture();
                    // Nothing was dragged out (anchor == focus): that's a plain CLICK — drop any
                    // old selection and let it act like one (outline jump / link open).
                    if matches!(st.sel.get(), Some((a, b)) if a == b) {
                        st.sel.set(None);
                        let (x, y) = lparam_xy(lparam);
                        click_content(hwnd, x, y);
                        let cr = content_rect(hwnd);
                        let _ = InvalidateRect(Some(hwnd), Some(&cr), false);
                    }
                } else {
                    let (x, y) = lparam_xy(lparam);
                    click_content(hwnd, x, y);
                }
                LRESULT(0)
            }
            WM_CAPTURECHANGED => {
                // Capture stolen mid-drag (alt-tab, another SetCapture) — end every drag so a
                // buttonless mouse-move can't keep seeking/panning/selecting.
                let st = &*state(hwnd);
                let scrollbar_was_pressed =
                    st.scroll_drag.get().is_some() || st.scroll_page_press.get();
                st.drag.set(None);
                st.scroll_drag.set(None);
                st.scroll_page_press.set(false);
                st.scrub_drag.set(false);
                st.vol_drag.set(false);
                st.sel_drag.set(false);
                let _ = set_scroll_hot(hwnd, false);
                if scrollbar_was_pressed {
                    invalidate_text_scrollbar(hwnd);
                }
                LRESULT(0)
            }
            WM_SETCURSOR => {
                // Hand cursor over a Markdown link, I-beam over selectable text; otherwise
                // default handling so the resize border + caption keep their sizing/move cursors.
                if (lparam.0 & 0xFFFF) as i32 == HTCLIENT as i32 {
                    let st = &*state(hwnd);
                    let mut pt = POINT::default();
                    let _ = GetCursorPos(&mut pt);
                    let _ = ScreenToClient(hwnd, &mut pt);
                    // Keep the standard arrow over the scrollbar instead of presenting the
                    // text-selection I-beam, which made the painted thumb look non-interactive.
                    if st.scroll_drag.get().is_some()
                        || st.scroll_page_press.get()
                        || hit_text_scrollbar(hwnd, pt.x, pt.y).is_some()
                    {
                        if let Ok(arrow) = LoadCursorW(None, IDC_ARROW) {
                            SetCursor(Some(arrow));
                        }
                        return LRESULT(1);
                    }
                    if st.kind.get() == ContentKind::Markdown
                        && (hit_link(hwnd, pt.x, pt.y).is_some()
                            || hit_toc(hwnd, pt.x, pt.y).is_some())
                    {
                        if let Ok(hand) = LoadCursorW(None, IDC_HAND) {
                            SetCursor(Some(hand));
                        }
                        return LRESULT(1);
                    }
                    if selection::selectable(st.kind.get())
                        && pt.y >= crate::win::dpi_scale(hwnd, CAPTION_H)
                        && hit_toc(hwnd, pt.x, pt.y).is_none()
                    {
                        if let Ok(ibeam) = LoadCursorW(None, IDC_IBEAM) {
                            SetCursor(Some(ibeam));
                        }
                        return LRESULT(1);
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_LBUTTONDBLCLK => {
                let (x, y) = lparam_xy(lparam);
                let st = &*state(hwnd);
                let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
                if hit_text_scrollbar(hwnd, x, y).is_some() {
                    // A double-click on the scrollbar must not select the document text beneath it.
                } else if y >= cap
                    && st.kind.get() == ContentKind::Image
                    && hit_button(hwnd, x, y).is_none()
                {
                    toggle_fit_100(hwnd); // double-click content → toggle fit / 100%
                } else if y >= cap
                    && selection::selectable(st.kind.get())
                    && hit_toc(hwnd, x, y).is_none()
                {
                    // Double-click in a text/Markdown pane → select the word under the cursor.
                    // Claiming the drag (capture + flag) keeps the button-up that follows from
                    // being read as a click — which would open a double-clicked link.
                    if let Some((a, b)) =
                        selection::hit(hwnd, x, y).and_then(|o| selection::word_range(hwnd, o))
                    {
                        st.sel.set(Some((a, b)));
                        st.sel_drag.set(true);
                        let _ = SetCapture(hwnd);
                        let cr = content_rect(hwnd);
                        let _ = InvalidateRect(Some(hwnd), Some(&cr), false);
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                // GET_WHEEL_DELTA_WPARAM (signed high word).
                let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let st = &*state(hwnd);
                match st.kind.get() {
                    ContentKind::Image => zoom_at_cursor(hwnd, delta, lparam),
                    ContentKind::Text | ContentKind::Markdown => scroll_text(hwnd, delta),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let st = &*state(hwnd);
                let vk = wparam.0 as u16;
                // F11 toggles borderless full-screen (works in daemon + manual mode).
                if vk == VK_F11.0 {
                    toggle_fullscreen(hwnd);
                    return LRESULT(0);
                }
                // Ctrl+A / Ctrl+C: select all / copy the CONTENT (the selection, the rendered
                // text, the info-card text, or the decoded image) — the whole point of a viewer
                // you can lift text out of. Ctrl+Shift+C copies a Markdown file's raw source.
                let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
                let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
                if ctrl && vk == 'A' as u16 {
                    if let Some(len) = selection::doc_len(hwnd) {
                        if len > 0 {
                            st.sel.set(Some((0, len)));
                            let cr = content_rect(hwnd);
                            let _ = InvalidateRect(Some(hwnd), Some(&cr), false);
                        }
                    }
                    return LRESULT(0);
                }
                if ctrl && vk == 'C' as u16 {
                    copy_content(hwnd, shift);
                    return LRESULT(0);
                }
                // Ctrl+U: view source / view rendered — the browser convention, same as the
                // toolbar's `</>` toggle. Ignored on files that only have one view.
                if ctrl && vk == 'U' as u16 {
                    toggle_source(hwnd);
                    return LRESULT(0);
                }
                // Shift+<nav key> extends the selection (plain arrows stay file navigation).
                if shift
                    && matches!(vk, v if v == VK_LEFT.0 || v == VK_RIGHT.0 || v == VK_UP.0
                        || v == VK_DOWN.0 || v == VK_HOME.0 || v == VK_END.0
                        || v == VK_PRIOR.0 || v == VK_NEXT.0)
                    && selection::extend(hwnd, vk, ctrl)
                {
                    return LRESULT(0);
                }
                // Home / End scroll a text or Markdown document to its ends.
                if !shift
                    && (vk == VK_HOME.0 || vk == VK_END.0)
                    && selection::selectable(st.kind.get())
                {
                    let to = if vk == VK_HOME.0 {
                        -st.text_scroll.get()
                    } else {
                        st.text_h.get()
                    };
                    selection::scroll_by(hwnd, to);
                    return LRESULT(0);
                }
                if st.kind.get() == ContentKind::Image && st.pdf_pages.get() > 1 {
                    // Multi-page PDF: arrows page within the document.
                    if vk == VK_NEXT.0 || vk == VK_RIGHT.0 || vk == VK_DOWN.0 {
                        goto_pdf_page(hwnd, 1);
                        return LRESULT(0);
                    }
                    if vk == VK_PRIOR.0 || vk == VK_LEFT.0 || vk == VK_UP.0 {
                        goto_pdf_page(hwnd, -1);
                        return LRESULT(0);
                    }
                } else {
                    // Otherwise ←/→ (and PgUp/PgDn) flip through the folder, QuickLook-style,
                    // without closing the popup.
                    if vk == VK_RIGHT.0 || vk == VK_NEXT.0 {
                        nav_sibling(hwnd, 1);
                        return LRESULT(0);
                    }
                    if vk == VK_LEFT.0 || vk == VK_PRIOR.0 {
                        nav_sibling(hwnd, -1);
                        return LRESULT(0);
                    }
                }
                // Esc leaves full-screen first (even when the daemon hook owns lifecycle keys).
                if vk == VK_ESCAPE.0 && st.fullscreen.get().is_some() {
                    toggle_fullscreen(hwnd);
                    return LRESULT(0);
                }
                // Only own the lifecycle keys when the daemon hook is NOT the authority.
                if st.manual && (vk == VK_ESCAPE.0 || vk == VK_SPACE.0 || vk == VK_RETURN.0) {
                    request_close(hwnd);
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_COPYDATA => {
                on_command(hwnd, lparam);
                LRESULT(1)
            }
            WM_DPICHANGED => {
                crate::win::wm_dpichanged(hwnd, lparam);
                LRESULT(0)
            }
            WM_DESTROY => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ViewerState;
                if !ptr.is_null() {
                    let tip = (*ptr).tip.get();
                    if !tip.is_invalid() {
                        let _ = DestroyWindow(tip); // owned popup; destroy before the state frees
                    }
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(ptr)); // frees RenderData (HBITMAP) + InfoCard (HICON)
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Handle a decode result: install the image (or fall back to an InfoCard on failure), then
/// size + show / resize.
unsafe fn on_render(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) {
    let boxed = Box::from_raw(lparam.0 as *mut (u64, Option<content::DecodedRgba>));
    let (gen, decoded) = *boxed;
    let st = &*state(hwnd);
    if gen != st.decode_gen.get() {
        return; // stale — the user already switched files
    }
    let _ = wparam;
    match decoded {
        Some(d) => match content::make_dib(d.w, d.h, &d.rgba, letterbox_bg(st)) {
            Some(hbmp) => {
                *st.render.borrow_mut() = Some(RenderData {
                    hbmp,
                    iw: d.w,
                    ih: d.h,
                });
                st.kind.set(ContentKind::Image);
            }
            None => fallback_card(st),
        },
        None => fallback_card(st), // decode failure / timeout → the calm card
    }
    ensure_shown(hwnd);
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// Fall back to the InfoCard for the current path (decode failed or timed out).
unsafe fn fallback_card(st: &ViewerState) {
    if let Some(p) = st.path.borrow().as_ref() {
        *st.card.borrow_mut() = Some(infocard::gather(p));
    }
    st.kind.set(ContentKind::InfoCard);
}

/// Install the decoded animation frames (build one DIB per frame) and start the frame timer.
unsafe fn on_anim(hwnd: HWND, lparam: LPARAM) {
    let boxed = Box::from_raw(lparam.0 as *mut (u64, Vec<(content::DecodedRgba, u32)>));
    let (gen, frames_in) = *boxed;
    let st = &*state(hwnd);
    if gen != st.decode_gen.get() {
        return; // stale — the user already switched files
    }
    let bg = letterbox_bg(st);
    let mut rds: Vec<RenderData> = Vec::with_capacity(frames_in.len());
    let mut delays: Vec<u32> = Vec::with_capacity(frames_in.len());
    for (d, ms) in frames_in {
        if let Some(hbmp) = content::make_dib(d.w, d.h, &d.rgba, bg) {
            rds.push(RenderData {
                hbmp,
                iw: d.w,
                ih: d.h,
            });
            delays.push(ms);
        }
    }
    if rds.len() < 2 {
        // couldn't build enough frames → fall through to a normal single-frame decode
        if let Some(p) = st.path.borrow().as_ref().cloned() {
            content::spawn_decode(hwnd, p, gen);
        }
        return;
    }
    let first = delays[0];
    *st.frames.borrow_mut() = rds;
    *st.frame_delays.borrow_mut() = delays;
    st.cur_frame.set(0);
    st.kind.set(ContentKind::Image);
    ensure_shown(hwnd);
    SetTimer(Some(hwnd), ANIM_TIMER_ID, first, None);
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// Advance to the next animation frame, re-arm the timer to that frame's delay, repaint content.
unsafe fn advance_frame(hwnd: HWND) {
    let st = &*state(hwnd);
    let n = st.frames.borrow().len();
    if n < 2 {
        let _ = KillTimer(Some(hwnd), ANIM_TIMER_ID);
        return;
    }
    let next = (st.cur_frame.get() + 1) % n;
    st.cur_frame.set(next);
    let delay = st.frame_delays.borrow().get(next).copied().unwrap_or(80);
    SetTimer(Some(hwnd), ANIM_TIMER_ID, delay, None);
    let cr = content_rect(hwnd);
    let _ = InvalidateRect(Some(hwnd), Some(&cr), false);
}

/// Handle a `WM_COPYDATA` command from the daemon (or the single-instance forwarder).
unsafe fn on_command(hwnd: HWND, lparam: LPARAM) {
    let Some((cmd, path)) = parse_command(lparam) else {
        return;
    };
    let st = &*state(hwnd);
    let in_grace = GetTickCount64().saturating_sub(st.born.get()) < SETTLE_CLOSE_MS;
    match cmd {
        CMD_SET_PATH => {
            if let Some(p) = path {
                request_load(hwnd, &p);
            }
        }
        CMD_TOGGLE => {
            if in_grace {
                return;
            }
            let same = matches!((path.as_deref(), st.path.borrow().as_deref()), (Some(a), Some(b)) if a == b);
            match path {
                Some(p) if !same => request_load(hwnd, &p),
                _ => request_close(hwnd),
            }
        }
        CMD_CLOSE if !in_grace => request_close(hwnd),
        _ => {}
    }
}

/// Run a toolbar button's action. `pub(super)` so the headless shot harness can drive a real
/// button press (`--toggle-source`) instead of only pre-setting state.
pub(super) unsafe fn do_action(hwnd: HWND, btn: Btn) {
    let st = &*state(hwnd);
    let path = st.path.borrow().clone();
    match btn {
        Btn::Toc => {
            // Slide the panel rather than snapping: freeze the CURRENT width (settled or
            // mid-animation), flip the target, and let TOC_TIMER_ID tween toward it.
            let w_full = crate::win::dpi_scale(hwnd, 220);
            let from = st
                .toc_anim
                .get()
                .unwrap_or(if st.toc_open.get() { w_full } else { 0 });
            let open = !st.toc_open.get();
            st.toc_open.set(open);
            st.toc_anim.set(Some(from));
            SetTimer(Some(hwnd), TOC_TIMER_ID, 15, None);
            let _ = sagethumbs2k_core::settings::set_preview_toc_open(open); // persist ("pin")
            let _ = InvalidateRect(Some(hwnd), None, false);
            update_tooltips(hwnd, st.tip.get());
        }
        Btn::Source => toggle_source(hwnd),
        Btn::PdfPrev => goto_pdf_page(hwnd, -1),
        Btn::PdfNext => goto_pdf_page(hwnd, 1),
        Btn::Close => request_close(hwnd),
        Btn::Pin => {
            let pin = !st.pinned.get();
            st.pinned.set(pin);
            let z = if pin { HWND_TOPMOST } else { HWND_NOTOPMOST };
            let _ = SetWindowPos(
                hwnd,
                Some(z),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
            let mut r = RECT::default();
            let _ = GetClientRect(hwnd, &mut r);
            r.bottom = cap;
            let _ = InvalidateRect(Some(hwnd), Some(&r), false);
        }
        Btn::Copy => {
            if let Some(p) = path {
                let bytes = sagethumbs2k_core::clipboard::utf16_nul_bytes(&p);
                let _ = sagethumbs2k_core::clipboard::set_clipboard(
                    sagethumbs2k_core::clipboard::CF_UNICODETEXT,
                    &bytes,
                );
            }
        }
        Btn::Info => {
            if let Some(p) = path {
                super::spawn_self(&["--image-info", &p]);
            }
        }
        Btn::Upload => {
            // Reuse the shipped keyless-host upload chain (same as the screenshot Upload
            // button + the DLL "Upload (copy link)" verb): write the path to a temp list,
            // spawn `--upload-keep` which uploads, copies the link, and toasts the result.
            // KEEPS the original (unlike `--upload`). No new deps / EXE weight.
            if let Some(p) = path {
                let mut lf = std::env::temp_dir();
                lf.push(format!("st2k_preview_upload_{}.lst", std::process::id()));
                if std::fs::write(&lf, &p).is_ok() {
                    if let Some(s) = lf.to_str() {
                        super::spawn_self(&["--upload-keep", s]);
                    }
                }
            }
        }
        Btn::Open => {
            if let Some(p) = path {
                let w = crate::win::wide(&p);
                ShellExecuteW(
                    Some(hwnd),
                    w!("open"),
                    PCWSTR(w.as_ptr()),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    SW_SHOWNORMAL,
                );
                let _ = DestroyWindow(hwnd); // Open hands off to the default app, then closes
            }
        }
        Btn::OpenWith => {
            if let Some(p) = path {
                let w = crate::win::wide(&p);
                // The shell "openas" verb shows the Open With dialog (no SHOpenWithDialog needed).
                ShellExecuteW(
                    Some(hwnd),
                    w!("openas"),
                    PCWSTR(w.as_ptr()),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    SW_SHOWNORMAL,
                );
            }
        }
    }
}

/// Flip between the RENDERED document and its raw source (toolbar button / Ctrl+U). No-op on a
/// file that has only one of the two views.
///
/// Implemented as a plain reload rather than an in-place content swap: `load` already tears down
/// whatever the rendered view owns (the WebView2 host, the markdown image cache + layout, the
/// selection and scroll state) and `request_load` routes it through the `busy` deferral, so a
/// toggle clicked while a WebView2 create is still pumping is applied after that create returns
/// instead of yanking state out from under it. The re-read is a capped text read, not a decode.
pub(super) unsafe fn toggle_source(hwnd: HWND) {
    let st = &*state(hwnd);
    if !st.src_capable.get() {
        return;
    }
    st.src_view.set(!st.src_view.get());
    // Hoist the clone into its own `let` — do NOT inline this as
    // `if let Some(p) = st.path.borrow().clone()`. On edition 2021 the `Ref` temporary in an
    // `if let` SCRUTINEE lives to the end of the whole block, so the `*st.path.borrow_mut()`
    // inside `load` would hit a BorrowMutError — and `panic=abort` turns that into the viewer
    // process dying on every click of this button. A `let` statement drops the `Ref` at the `;`.
    let path = st.path.borrow().clone();
    if let Some(p) = path {
        request_load(hwnd, &p);
    }
}

/// A plain click (nothing dragged) in the content area: an outline entry jumps to its heading;
/// a Markdown link opens it.
unsafe fn click_content(hwnd: HWND, x: i32, y: i32) {
    let st = &*state(hwnd);
    if let Some(idx) = hit_toc(hwnd, x, y) {
        // Jump to the heading AND explicitly select it — bottom sections can't scroll to the
        // pane top (max-scroll clamp), so without the selection override the click would be
        // visually dead.
        let target = st.md_toc.borrow().get(idx).map(|e| e.target);
        if let Some(target) = target {
            let _ = set_text_scroll(hwnd, target);
            st.toc_sel.set(Some(idx));
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    } else if let Some(url) = hit_link(hwnd, x, y) {
        open_preview_link(hwnd, &url);
    }
}

/// Ctrl+C: put the viewer's CONTENT on the clipboard — the selected text (else the whole
/// document) for the text/Markdown panes, the card's text for the info card, and the decoded
/// pixels (CF_DIB, same packed-DIB path as the context menu's Copy verb) for an image.
/// `raw` (Ctrl+Shift+C) copies a Markdown file's SOURCE instead of its rendered text.
/// The toolbar Copy button still copies the file PATH.
unsafe fn copy_content(hwnd: HWND, raw: bool) {
    use sagethumbs2k_core::clipboard::{set_clipboard, utf16_nul_bytes, CF_UNICODETEXT};
    let st = &*state(hwnd);
    match st.kind.get() {
        ContentKind::Markdown if raw => {
            let text = st.text.borrow();
            if let Some(t) = text.as_ref().filter(|t| !t.is_empty()) {
                let _ = set_clipboard(CF_UNICODETEXT, &utf16_nul_bytes(t));
            }
        }
        ContentKind::Text | ContentKind::Markdown => {
            if let Some(s) = selection::copy_text(hwnd) {
                let _ = set_clipboard(CF_UNICODETEXT, &utf16_nul_bytes(&s));
            }
        }
        ContentKind::InfoCard => {
            let card = st.card.borrow();
            if let Some(c) = card.as_ref() {
                let _ = set_clipboard(CF_UNICODETEXT, &utf16_nul_bytes(&c.copy_text()));
            }
        }
        ContentKind::Image => {
            // Copy what is DISPLAYED — the navigated-to PDF page / the animation frame on
            // screen at the keypress — not blindly the file's first page/frame. Decode + pack
            // off the UI thread (a RAW/HEIC decode isn't instant); the WIC tier needs COM.
            let Some(p) = st.path.borrow().clone() else {
                return;
            };
            let pdf_page = (st.pdf_pages.get() > 1).then(|| st.pdf_page.get());
            let anim_frame = {
                let frames = st.frames.borrow();
                (frames.len() > 1).then(|| st.cur_frame.get())
            };
            std::thread::spawn(move || {
                use windows::Win32::System::Com::{
                    CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED,
                };
                let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
                if !copy_shown_image(&p, pdf_page, anim_frame) {
                    // The viewer has no toast/status surface, so a failed copy is otherwise
                    // indistinguishable from the keypress not registering — say so in the log.
                    sagethumbs2k_core::safety::log(&format!("preview: Ctrl+C could not copy {p}"));
                }
                if inited {
                    unsafe { CoUninitialize() };
                }
            });
        }
        _ => {}
    }
}

/// Copy the image the viewer is SHOWING: the given PDF page / animation frame when navigated,
/// else the file's full-fidelity decode (the context menu's Copy verb path). Falls back to the
/// static decode when frame extraction fails, so Ctrl+C still yields SOMETHING.
fn copy_shown_image(path: &str, pdf_page: Option<u32>, anim_frame: Option<usize>) -> bool {
    if let Some(page) = pdf_page {
        let png = sagethumbs2k_core::decode::read_capped(path)
            .ok()
            .and_then(|b| sagethumbs2k_core::pdf::render_page_counted(&b, page, 1600));
        if let Some(img) = png.and_then(|(png, _)| image::load_from_memory(&png).ok()) {
            let rgba = img.to_rgba8();
            let (w, h) = (rgba.width() as i32, rgba.height() as i32);
            return sagethumbs2k_core::copy_rgba_to_clipboard(w, h, &rgba.into_raw()).is_ok();
        }
        return false; // page N failed to render — copying page 1 instead would be a silent lie
    }
    if let Some(frame) = anim_frame {
        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        let frames = sagethumbs2k_core::decode::read_preview_capped(path)
            .ok()
            .and_then(|b| super::anim::decode_animation(&b, &ext));
        if let Some(frames) = frames {
            if let Some((d, _)) = frames.get(frame) {
                return sagethumbs2k_core::copy_rgba_to_clipboard(d.w, d.h, &d.rgba).is_ok();
            }
        }
        // fall through: static decode (first frame) beats copying nothing
    }
    sagethumbs2k_core::copy_to_clipboard(path).is_ok()
}

// ===== Phase 4: zoom / pan / scroll =====

/// The content rectangle (below the caption), in client coords.
/// Whether `path` is a PDF (by extension).
pub(super) fn is_pdf(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false)
}

/// Navigate a multi-page PDF by `delta` pages. Keeps the current page visible until the new one
/// decodes (no Loading flash); the `decode_gen` bump fences a stale in-flight page decode.
pub(super) unsafe fn goto_pdf_page(hwnd: HWND, delta: i32) {
    let st = &*state(hwnd);
    let pages = st.pdf_pages.get();
    if pages <= 1 || st.kind.get() != ContentKind::Image {
        return;
    }
    // i64 math: `pages` is capped at ingestion, but never trust it enough to wrap an i32.
    let new = (st.pdf_page.get() as i64 + delta as i64).clamp(0, pages as i64 - 1) as u32;
    if new == st.pdf_page.get() {
        return;
    }
    st.pdf_page.set(new);
    st.zoom.set(1.0);
    st.pan.set((0, 0));
    let gen = st.decode_gen.get() + 1;
    st.decode_gen.set(gen);
    if let Some(p) = st.path.borrow().as_ref().cloned() {
        content::spawn_decode_pdf(hwnd, p, new, gen);
    }
    let _ = InvalidateRect(Some(hwnd), None, false); // update the "N / M" caption immediately
}

/// Natural pixel dims of the current image content — the first animation frame when animated
/// (all frames share a size), else the static render. `None` while still loading.
pub(super) fn image_dims(st: &ViewerState) -> Option<(i32, i32)> {
    if let Some(rd) = st.frames.borrow().first() {
        return Some((rd.iw, rd.ih));
    }
    st.render.borrow().as_ref().map(|rd| (rd.iw, rd.ih))
}

pub(super) unsafe fn content_rect(hwnd: HWND) -> RECT {
    let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
    let mut r = RECT::default();
    let _ = GetClientRect(hwnd, &mut r);
    RECT {
        left: 0,
        top: cap,
        right: r.right,
        bottom: r.bottom,
    }
}

mod scroll;
pub(super) use scroll::*;

/// Video-only: the render child's rect = content area minus the bottom scrub strip.
/// Zoom the image in/out by a wheel notch, keeping the image point under the cursor fixed.
unsafe fn zoom_at_cursor(hwnd: HWND, delta: i32, lparam: LPARAM) {
    let st = &*state(hwnd);
    let Some((iw, ih)) = image_dims(st) else {
        return;
    };
    let c = content_rect(hwnd);
    let (cw, ch) = (c.right - c.left, c.bottom - c.top);
    // WM_MOUSEWHEEL's lparam is in SCREEN coords.
    let (sx, sy) = lparam_xy(lparam);
    let mut pt = POINT { x: sx, y: sy };
    let _ = ScreenToClient(hwnd, &mut pt);
    let fit = content::fit_scale(iw, ih, cw, ch);
    let old_zoom = st.zoom.get();
    let new_zoom = (old_zoom * if delta > 0 { 1.2 } else { 1.0 / 1.2 }).clamp(1.0, 8.0);
    if (new_zoom - old_zoom).abs() < 1e-6 {
        return;
    }
    let (px, py) = st.pan.get();
    let old_scale = fit * old_zoom;
    let old_dx = c.left as f64 + (cw as f64 - iw as f64 * old_scale) / 2.0 + px as f64;
    let old_dy = c.top as f64 + (ch as f64 - ih as f64 * old_scale) / 2.0 + py as f64;
    let img_x = (pt.x as f64 - old_dx) / old_scale;
    let img_y = (pt.y as f64 - old_dy) / old_scale;
    let new_scale = fit * new_zoom;
    let new_px = (pt.x as f64
        - img_x * new_scale
        - c.left as f64
        - (cw as f64 - iw as f64 * new_scale) / 2.0)
        .round() as i32;
    let new_py = (pt.y as f64
        - img_y * new_scale
        - c.top as f64
        - (ch as f64 - ih as f64 * new_scale) / 2.0)
        .round() as i32;
    st.zoom.set(new_zoom);
    st.pan.set((new_px, new_py));
    clamp_pan(hwnd);
    let _ = InvalidateRect(Some(hwnd), Some(&c), false);
}

/// Toggle between aspect-fit and 100% (native pixels), recentering.
unsafe fn toggle_fit_100(hwnd: HWND) {
    let st = &*state(hwnd);
    let Some((iw, ih)) = image_dims(st) else {
        return;
    };
    let c = content_rect(hwnd);
    let fit = content::fit_scale(iw, ih, c.right - c.left, c.bottom - c.top);
    let full = (1.0 / fit).clamp(1.0, 8.0); // 100% == display scale 1.0
    st.zoom.set(if st.zoom.get() <= 1.01 { full } else { 1.0 });
    st.pan.set((0, 0));
    clamp_pan(hwnd);
    let _ = InvalidateRect(Some(hwnd), Some(&c), false);
}

/// Keep the (zoomed) image covering the content — clamp pan so no empty margin shows.
unsafe fn clamp_pan(hwnd: HWND) {
    let st = &*state(hwnd);
    let Some((iw, ih)) = image_dims(st) else {
        return;
    };
    let c = content_rect(hwnd);
    let (cw, ch) = (c.right - c.left, c.bottom - c.top);
    let scale = content::fit_scale(iw, ih, cw, ch) * st.zoom.get();
    let dw = (iw as f64 * scale) as i32;
    let dh = (ih as f64 * scale) as i32;
    let (maxx, maxy) = (((dw - cw) / 2).max(0), ((dh - ch) / 2).max(0));
    let (px, py) = st.pan.get();
    st.pan.set((px.clamp(-maxx, maxx), py.clamp(-maxy, maxy)));
}

fn wheel_notches(remainder: i32, delta: i32) -> (i32, i32) {
    let total = remainder.saturating_add(delta);
    (total / 120, total % 120)
}

/// Accumulate precision-wheel deltas, scrolling ~3 lines whenever they reach a full notch.
unsafe fn scroll_text(hwnd: HWND, delta: i32) {
    let st = &*state(hwnd);
    let (notches, remainder) = wheel_notches(st.wheel_remainder.get(), delta);
    st.wheel_remainder.set(remainder);
    if notches == 0 {
        return;
    }
    let step = crate::win::dpi_scale(hwnd, 40);
    let _ = scroll_text_by(hwnd, notches.saturating_mul(-step));
}

/// One outline-sidebar slide frame: move the animated width a third of the remaining distance
/// (min step so it always lands), settle + kill the timer at the target.
unsafe fn tick_toc_anim(hwnd: HWND) {
    let st = &*state(hwnd);
    let w_full = crate::win::dpi_scale(hwnd, 220);
    let target = if st.toc_open.get() { w_full } else { 0 };
    let cur = st.toc_anim.get().unwrap_or(target);
    let d = target - cur;
    let step = (d.abs() / 3).max(crate::win::dpi_scale(hwnd, 16));
    if d.abs() <= step {
        st.toc_anim.set(None); // settled
        let _ = KillTimer(Some(hwnd), TOC_TIMER_ID);
    } else {
        st.toc_anim.set(Some(cur + step * d.signum()));
    }
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// Toggle borderless full-screen (F11): fill the current monitor and hide the resize border;
/// Esc or F11 again restores the exact windowed geometry saved on entry.
unsafe fn toggle_fullscreen(hwnd: HWND) {
    let st = &*state(hwnd);
    if let Some(prev) = st.fullscreen.get() {
        let style = (GetWindowLongPtrW(hwnd, GWL_STYLE) as u32) | WS_THICKFRAME.0;
        SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize);
        let _ = SetWindowPos(
            hwnd,
            None,
            prev.left,
            prev.top,
            prev.right - prev.left,
            prev.bottom - prev.top,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        st.fullscreen.set(None);
    } else {
        let mut wr = RECT::default();
        if GetWindowRect(hwnd, &mut wr).is_err() {
            return;
        }
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: core::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return;
        }
        st.fullscreen.set(Some(wr));
        let style = (GetWindowLongPtrW(hwnd, GWL_STYLE) as u32) & !WS_THICKFRAME.0;
        SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize);
        let r = mi.rcMonitor;
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
    }
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// True if `ext` (lowercase, no dot) is something the viewer can render — used to filter the
/// folder listing for ←/→ navigation so arrows skip files nothing can preview. Must stay in sync
/// with what `loader::load` actually handles: decoded formats + text/markdown + archives + fonts.
fn is_previewable_ext(ext: &str) -> bool {
    use sagethumbs2k_core::formats;
    formats::is_known(ext)
        || formats::is_preview_text(ext)
        || formats::is_preview_markdown(ext)
        || formats::is_preview_doc(ext)
        || content::is_archive_ext(ext)
        || super::font::is_font_ext(ext)
}

/// Explorer-style filename order (`image2` before `image10`). Precompute each UTF-16 key once
/// so a large-folder O(n log n) sort does not allocate inside every comparison.
fn sort_paths_like_explorer(files: Vec<std::path::PathBuf>) -> Vec<std::path::PathBuf> {
    let mut keyed: Vec<(Vec<u16>, std::path::PathBuf)> = files
        .into_iter()
        .map(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
            (crate::win::wide(&name), p)
        })
        .collect();
    keyed.sort_by(|a, b| {
        unsafe { StrCmpLogicalW(PCWSTR(a.0.as_ptr()), PCWSTR(b.0.as_ptr())) }
            .cmp(&0)
            .then_with(|| a.1.cmp(&b.1))
    });
    keyed.into_iter().map(|(_, p)| p).collect()
}

/// Flip to the next/prev previewable file in the current file's folder (QuickLook-style folder
/// traversal, wrapping at the ends), without closing the popup. Sorted case-insensitively by
/// Explorer's logical filename order.
unsafe fn nav_sibling(hwnd: HWND, delta: i32) {
    let st = &*state(hwnd);
    let cur = match st.path.borrow().clone() {
        Some(p) => p,
        None => return,
    };
    let cur_path = std::path::Path::new(&cur);
    let Some(dir) = cur_path.parent() else {
        return;
    };
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let files: Vec<std::path::PathBuf> = rd
        .flatten()
        .filter(|e| {
            // Use the DirEntry's cached file_type (no extra per-entry `stat` syscall — a huge
            // folder would otherwise cost thousands of stats on the UI thread) and gate on the
            // extension BEFORE anything else.
            e.file_type().map(|t| t.is_file()).unwrap_or(false)
                && std::path::Path::new(&e.file_name())
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|x| is_previewable_ext(&x.to_ascii_lowercase()))
                    .unwrap_or(false)
        })
        .map(|e| e.path())
        .take(20_000) // sanity cap for a pathological folder
        .collect();
    if files.len() < 2 {
        return;
    }
    let files = sort_paths_like_explorer(files);
    let idx = files.iter().position(|p| p == cur_path).unwrap_or(0) as i32;
    let n = files.len() as i32;
    let ni = ((idx + delta) % n + n) % n; // wrap around at both ends
    let next = files[ni as usize].to_string_lossy().into_owned();
    if next != cur {
        request_load(hwnd, &next);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        scroll_from_thumb_offset, scroll_thumb_geometry, sort_paths_like_explorer,
        text_scroll_limits, wheel_notches,
    };
    use std::path::PathBuf;

    #[test]
    fn every_scroll_path_shares_the_same_limits() {
        assert_eq!(text_scroll_limits(600, 12, 2400), (576, 1824));
        assert_eq!(text_scroll_limits(600, 12, 500), (576, 0));
    }

    #[test]
    fn scrollbar_geometry_tracks_the_document_range() {
        let (thumb_h, top) = scroll_thumb_geometry(600, 576, 1824, 0, 32).unwrap();
        assert_eq!((thumb_h, top), (144, 0));

        let (_, middle) = scroll_thumb_geometry(600, 576, 1824, 912, 32).unwrap();
        let (_, bottom) = scroll_thumb_geometry(600, 576, 1824, 1824, 32).unwrap();
        assert_eq!(middle, 228);
        assert_eq!(bottom, 456);
    }

    #[test]
    fn scrollbar_drag_clamps_to_both_ends() {
        assert_eq!(scroll_from_thumb_offset(-50, 456, 1824), 0);
        assert_eq!(scroll_from_thumb_offset(228, 456, 1824), 912);
        assert_eq!(scroll_from_thumb_offset(900, 456, 1824), 1824);
    }

    #[test]
    fn precision_wheel_deltas_accumulate_without_being_lost() {
        assert_eq!(wheel_notches(0, 30), (0, 30));
        assert_eq!(wheel_notches(30, 90), (1, 0));
        assert_eq!(wheel_notches(0, -60), (0, -60));
        assert_eq!(wheel_notches(-60, -60), (-1, 0));
        assert_eq!(wheel_notches(45, -45), (0, 0));
    }

    #[test]
    fn sibling_navigation_uses_explorer_logical_order() {
        let input = ["image10.png", "image2.png", "image1.png"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        let names: Vec<String> = sort_paths_like_explorer(input)
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["image1.png", "image2.png", "image10.png"]);
    }
}
