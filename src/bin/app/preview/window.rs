//! The viewer window: a borderless, resizable, dark/DPI-aware popup with a slim custom
//! caption + toolbar. Owns the wndproc, all painting, hit-testing (drag/resize), sizing,
//! key handling, the toolbar actions, and the `WM_COPYDATA` command handling. Content
//! painting is delegated to [`super::content`] (images) and [`super::infocard`].

use std::cell::{Cell, RefCell};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, InvalidateRect, MonitorFromWindow, ScreenToClient, HBITMAP, HDC, HGDIOBJ,
    MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
    VK_CONTROL, VK_DOWN, VK_END, VK_ESCAPE, VK_F11, VK_HOME, VK_LEFT, VK_NEXT, VK_OEM_MINUS,
    VK_OEM_PLUS, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_UP,
};
use windows::Win32::UI::Shell::{ShellExecuteW, StrCmpLogicalW};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::content::{self, RenderData};
use super::selection::{self, SelHit};
use super::{infocard, parse_command, CMD_CLOSE, CMD_SET_PATH, CMD_TOGGLE, VIEWER_CLASS};
use super::{loader::*, paint::*, toolbar::*, transport::*};

/// Decode result posted from the worker (`WM_APP + 1`); LPARAM = `Box<(gen, Option<SharedRgba>)>`.
pub(super) const WM_APP_RENDER: u32 = WM_APP + 1;
/// Animated-image frames posted from the worker (`WM_APP + 7`);
/// LPARAM = `Box<(gen, Vec<(DecodedRgba, delay_ms)>)>`.
pub(super) const WM_APP_ANIM: u32 = WM_APP + 7;
/// PDF page count posted from the worker (`WM_APP + 8`); LPARAM = `Box<(gen, page_count)>`.
pub(super) const WM_APP_PDFINFO: u32 = WM_APP + 8;
/// One rasterized PDF page for the continuous view (`WM_APP + 10`);
/// LPARAM = `Box<(gen, page, width, Option<(w, h, rgba)>)>`. Posted even on FAILURE, so the
/// page's in-flight flag is cleared and it can be retried rather than staying blank forever.
pub(super) const WM_APP_PDFTILE: u32 = WM_APP + 10;
/// The opened PDF session for the continuous view (`WM_APP + 11`);
/// LPARAM = `Box<(gen, PdfSession)>`. Arrives AFTER the first page is already on screen, so
/// opening a long document never delays first paint; the view simply becomes scrollable when
/// it lands, and stays a single-page pager if it never does.
pub(super) const WM_APP_PDFDOC: u32 = WM_APP + 11;
/// One rendered page THUMBNAIL for the side strip (`WM_APP + 12`); same payload shape as
/// `WM_APP_PDFTILE`, separate message because it feeds the strip's own cache at its own width.
pub(super) const WM_APP_PDFSTRIP: u32 = WM_APP + 12;
/// One page's recognized TEXT for the Ctrl+F index (`WM_APP + 13`);
/// LPARAM = `Box<(gen, page, Option<String>)>`. `None` is a recognizer failure, not an empty
/// page, and the two are counted apart so an unavailable OCR engine is never reported as a
/// document with nothing in it.
pub(super) const WM_APP_PDFTEXT: u32 = WM_APP + 13;
/// A fetched remote markdown image (`WM_APP + 9`); LPARAM = `Box<(gen, src, Option<DecodedRgba>)>`.
pub(super) const WM_APP_MDIMG: u32 = WM_APP + 9;
/// Follow-selection switch posted from the poll thread (`WM_APP + 2`); LPARAM = `Box<String>` path.
pub(super) const WM_APP_SWITCH: u32 = WM_APP + 2;
/// The async classify/read step landed (`WM_APP + 14`); LPARAM = `Box<(gen, Resolved)>`
/// (2026-09-05 audit, F10). Posted by [`super::loader::spawn_prepare_load`] for the work that
/// used to run synchronously in `load()` before the window could even show: the extension
/// sniff, archive listing, DB/mail markdown and the text/markdown read.
pub(super) const WM_APP_LOAD_RESOLVED: u32 = WM_APP + 14;
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
/// The narrowest a toolbar cell is allowed to get when the caption cannot fit the visible set at
/// [`BTN_W`] (see `toolbar::button_rects`). Wide enough to still hold a ~14 px glyph with a
/// pixel of air either side, so a crowded caption reads as tight rather than as broken.
pub(super) const MIN_BTN_W: i32 = 22;
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
    /// Load the web-hosted images this Markdown document references, instead of showing them as
    /// labelled chips. Only appears when the document ACTUALLY references some (see
    /// [`btn_visible`]), which is the only moment the choice means anything: you are looking at
    /// the chips and wondering whether you can see the real thing. It used to be a checkbox in
    /// Settings, where nobody thought to look for it.
    MdImages,
    /// "View source" toggle: swap a RENDERED document (Markdown, a CSV/TSV/notebook table, a
    /// WebView2 HTML page, an SVG) for its raw text, and back. Only shown when the current file
    /// actually has both views (see [`btn_visible`] / `loader::source_capable`).
    Source,
    PdfPrev,
    PdfNext,
    /// Flip THIS window between the light and dark skin, without touching the app-wide Theme
    /// setting. Requested for the case the setting cannot serve: a dark photograph or a bright
    /// scan reads better against the opposite background from the one you normally want.
    /// Session-only, like [`Btn::Source`] — a fresh preview opens in the configured theme.
    Theme,
    Pin,
    Copy,
    /// Save the currently-shown PDF page / animation frame / video frame as a standalone PNG
    /// (Ctrl+S). Only shown while one of those applies (see [`btn_visible`]) — a plain image
    /// with no navigation has nothing beyond what [`Btn::Copy`] already puts on the clipboard,
    /// and there is no "save the whole file" fallback for this one (unlike Copy's). The video
    /// case grabs the frame at the CURRENT playback position (`video::save_current_frame`), not
    /// the settings default every other Media Foundation tier grabs — saving a different frame
    /// from the one on screen would be a bug, not a feature.
    SavePage,
    /// Read the text out of the picture you're looking at (OCR) and put it on the clipboard.
    /// Only shown for image-ish content (see [`btn_visible`]) — there is nothing to recognize
    /// in a text or Markdown pane, where you can already select the words.
    Ocr,
    Info,
    Upload,
    Open,
    OpenWith,
    /// Open Settings on its **Quick preview** page — the options that govern this window are
    /// otherwise several clicks away in a program you reach from a right-click menu.
    Settings,
    /// Print the CURRENTLY SHOWN content bitmap (Ctrl+P) — the same navigated-to PDF page /
    /// animation frame [`Btn::SavePage`] would write to disk, or the plain decode for a static
    /// image — through the standard Windows print dialog, fit to page and centred. Image-only
    /// (see [`btn_visible`]), the same gate [`Btn::Ocr`] uses: there is nothing to rasterize in
    /// a text/Markdown/HTML pane or a video frame that printing would add over the OS's own
    /// "print from" path for that document type.
    Print,
    Close,
}

/// All buttons, in left-to-right caption order (rightmost drawn is Close).
pub(super) const BTNS: [Btn; 17] = [
    Btn::Toc,
    Btn::MdImages,
    Btn::Source,
    Btn::PdfPrev,
    Btn::PdfNext,
    // Theme sits with the other VIEW controls, left of the file actions.
    Btn::Theme,
    Btn::Pin,
    Btn::Copy,
    Btn::SavePage,
    Btn::Ocr,
    Btn::Info,
    Btn::Upload,
    Btn::OpenWith,
    Btn::Open,
    // Settings then Close: the gear is an app action, and Close stays hard right where a
    // window's close button belongs.
    Btn::Settings,
    // Appended here (immediately left of Close) deliberately: every OTHER button keeps its
    // `--hot` index (see main.rs / DEVELOPMENT_GOTCHAS), and only `Btn::Close` shifts, by one.
    Btn::Print,
    Btn::Close,
];

thread_local! {
    /// GDI+ token for this window's lifetime — started in `WM_CREATE`, shut down in
    /// `WM_DESTROY`, mirroring `settings_dlg`. GDI+ must be live on the thread before the
    /// caption's anti-aliased OCR mark can draw; without it `GdipCreateFromHDC` fails and
    /// [`crate::gdip::with_aa`] silently draws NOTHING (an empty button, which is exactly
    /// how this was first noticed).
    static GDIP_TOKEN: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

/// Whether `Btn::SavePage` should show, given the state it depends on — split out as a pure
/// function (mirrors `toolbar::cell_width`'s reason for existing) so the video case is testable
/// without spinning up a real `ViewerState`/window, which needs a live HWND to construct.
///
/// Same navigated-or-not condition `clipboard::navigated_shown_image_rgba` uses for PDF/anim: a
/// multi-page PDF actually paged, or an animation actually holding more than one frame. Video is
/// a THIRD case with no "navigated" concept of its own: a live player is enough (there's always
/// a current frame to grab), which is also why it's gated on the player actually existing rather
/// than `ContentKind::Video` alone — a video that fell back to an info card (no Media Foundation,
/// a codec it refused) has no frame behind the button.
fn savepage_visible(
    kind: ContentKind,
    pdf_pages: u32,
    frame_count: usize,
    has_video: bool,
) -> bool {
    (kind == ContentKind::Image && (pdf_pages > 1 || frame_count > 1))
        || (kind == ContentKind::Video && has_video)
}

/// Whether a toolbar button is currently shown (PDF pager only for multi-page PDFs; the outline
/// toggle only for Markdown that has headings; the source toggle only for files that HAVE a
/// rendered view to toggle away from).
pub(super) fn btn_visible(st: &ViewerState, b: Btn) -> bool {
    match b {
        Btn::PdfPrev | Btn::PdfNext => {
            st.kind.get() == ContentKind::Image && st.pdf_pages.get() > 1
        }
        Btn::Toc => st.kind.get() == ContentKind::Markdown && st.md_has_headings.get(),
        Btn::MdImages => st.kind.get() == ContentKind::Markdown && st.md_has_remote.get(),
        Btn::Source => st.src_capable.get(),
        Btn::SavePage => savepage_visible(
            st.kind.get(),
            st.pdf_pages.get(),
            st.frames.borrow().len(),
            st.video.borrow().is_some(),
        ),
        // OCR needs pixels to read. A text/Markdown/HTML pane already has selectable words
        // (Ctrl+C), and an InfoCard is our own chrome, so the button would be a no-op there.
        // Print shares the same gate: it prints the decoded bitmap, and a text/Markdown/HTML
        // pane or a video frame has no such bitmap to print (see `Btn::Print`).
        Btn::Ocr | Btn::Print => st.kind.get() == ContentKind::Image,
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
    /// Cover art for an AUDIO file, painted as the backdrop behind the transport strip. Audio
    /// rides the same Media Foundation engine as video (`ContentKind::Video`) but has no picture
    /// of its own, so without this the content area is a flat black rectangle. Kept separate from
    /// `render` because the kind stays Video: this is a backdrop, not the content, so it must not
    /// pick up the Image path's zoom, pan, or arrow-key behaviour.
    pub(super) art: RefCell<Option<RenderData>>,
    /// Animated-image frames (empty for static content). When non-empty the Image path shows
    /// `frames[cur_frame]` and cycles them on `ANIM_TIMER_ID`.
    pub(super) frames: RefCell<Vec<RenderData>>,
    pub(super) frame_delays: RefCell<Vec<u32>>,
    pub(super) cur_frame: Cell<usize>,
    /// PDF page navigation: current 0-based page + total page count (0 = not a multi-page PDF).
    pub(super) pdf_page: Cell<u32>,
    pub(super) pdf_pages: Cell<u32>,
    /// The open document behind CONTINUOUS scrolling, when this file is a multi-page PDF whose
    /// session opened. `None` for everything else, and for a PDF whose session failed, which is
    /// what makes the fallback to single-page paging automatic rather than a separate mode
    /// somebody has to remember to select. See `preview::pdfview`.
    pub(super) pdf_doc: RefCell<Option<super::pdfview::PdfDoc>>,
    pub(super) card: RefCell<Option<infocard::InfoCard>>,
    pub(super) text: RefCell<Option<String>>,
    pub(super) video: RefCell<Option<super::video::VideoPlayer>>,
    /// The playing clip's REAL pixel size (rotation applied), once Media Foundation has read its
    /// metadata; `None` before that and for audio. `loader::client_size` sizes the window from it,
    /// so a portrait clip gets a portrait window instead of the placeholder 16:9 shell.
    pub(super) video_dims: Cell<Option<(i32, i32)>>,
    /// `settings::preview_arrow_nav`, read once per load like the other behaviour toggles so a
    /// mid-preview Settings save cannot change what a key does between two presses.
    pub(super) arrow_nav: Cell<bool>,
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
    /// Keyboard focus on the caption toolbar / transport strip (Tab-reachable, mouse-only
    /// before this). `None` = focus is in the content pane, same as before this existed.
    /// `toolbar::live_focus` treats this as stale (i.e. as `None`) once `focus_gen` no longer
    /// matches `decode_gen` — see that function's doc comment for why a load path never has to
    /// remember to clear it.
    pub(super) focus: Cell<Option<super::toolbar::FocusTarget>>,
    /// The `decode_gen` that was current when `focus` was last SET (not merely repainted).
    /// Meaningless while `focus` is `None`.
    pub(super) focus_gen: Cell<u64>,
    /// The caption toolbar's tooltip control (one RECT tool per button); `HWND::default()` if none.
    pub(super) tip: Cell<HWND>,
    /// The rects currently registered with `tip`, in tool-id order (see `toolbar::tool_rects`).
    /// The paint path compares against this and only re-points the tooltip control when the
    /// toolbar layout actually moved — see `toolbar::update_tooltips` for the bug this shape
    /// exists to make impossible.
    pub(super) tip_rects: RefCell<Vec<RECT>>,
    /// The tooltip TEXTS currently registered with `tip`, in the same tool-id order.
    /// Three of the bar's tips are chosen from state that changes while the window is open
    /// (theme, pin, view-source, mute, repeat), and comctl32 keeps its own copy of whatever was
    /// last sent — so without this the tip would still describe the state the user just left.
    /// See `toolbar::tooltip_text_changed` for the bug that shipped.
    pub(super) tip_texts: RefCell<Vec<&'static str>>,
    /// Whether the 500 ms follow-selection poll thread has been started (daemon mode only).
    pub(super) poll_started: Cell<bool>,
    // ----- Phase 4 viewer polish -----
    /// Image zoom RELATIVE TO FIT: 1.0 = aspect-fit (the default). Wheel + double-click drive it.
    pub(super) zoom: Cell<f64>,
    /// A full-resolution decode has been asked for and has not landed yet.
    ///
    /// The fit view is served by a codec-scaled decode that holds only display-sized pixels
    /// (see `content::spawn_decode`), and a zoom past what it holds triggers a real one. That
    /// check runs on every paint, so without this latch a single zoom would spawn a decode per
    /// repaint. Cleared when a full decode installs, and reset per file by `loader::load`.
    pub(super) full_pending: Cell<bool>,
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
    /// Whether the current Markdown document references any web-hosted image. Computed ONCE at
    /// load (same streaming parse as `md_has_headings`), and the only thing that decides whether
    /// the web-images toolbar button appears at all.
    pub(super) md_has_remote: Cell<bool>,
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
    /// A user drag-resize of the frame is in progress (or just finished, pending the save). Set by
    /// `WM_SIZING` — which fires ONLY for a real frame drag, never for our own `SetWindowPos` —
    /// and consumed by `WM_EXITSIZEMOVE`, which persists the new size. That pairing is what makes
    /// "the size I dragged" stick without a plain window MOVE also pinning a size the user never
    /// chose. While it is set, `loader::client_size` yields to the live drag.
    pub(super) user_sized: Cell<bool>,
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
    /// In-document find (Ctrl+F). The query outlives both closing the bar and switching files, so
    /// the same search can be carried through a folder with ←/→ (see [`super::find`]).
    pub(super) find: RefCell<super::find::FindState>,
    /// The live WebView2 host for `ContentKind::Html` (feature `html-preview`); `None` otherwise.
    #[cfg(feature = "html-preview")]
    pub(super) webview: RefCell<Option<super::webview::WebViewHost>>,
    /// Cached `WM_PAINT` double-buffer: a memory DC + bitmap sized to the client area, reused
    /// across repaints instead of a fresh `CreateCompatibleBitmap` (tens of MB at 4K) per frame.
    /// `back_stock` is the DC's original 1x1 stock bitmap, swapped back in before deleting
    /// `back_bmp` — GDI silently leaks a bitmap deleted while still selected into a DC. Default
    /// (all-zero/invalid) until the first paint allocates it. Invalidated (freed) on `WM_SIZE` —
    /// the client size just changed under it — and freed for good on `WM_DESTROY`; see
    /// `paint::free_back_buffer`.
    pub(super) back_dc: Cell<HDC>,
    pub(super) back_bmp: Cell<HBITMAP>,
    pub(super) back_stock: Cell<HGDIOBJ>,
    pub(super) back_size: Cell<(i32, i32)>,
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
        art: RefCell::new(None),
        frames: RefCell::new(Vec::new()),
        frame_delays: RefCell::new(Vec::new()),
        cur_frame: Cell::new(0),
        pdf_page: Cell::new(0),
        pdf_pages: Cell::new(0),
        pdf_doc: RefCell::new(None),
        card: RefCell::new(None),
        text: RefCell::new(None),
        video: RefCell::new(None),
        video_dims: Cell::new(None),
        arrow_nav: Cell::new(sagethumbs2k_core::settings::preview_arrow_nav()),
        find: super::find::new_state(),
        hinst,
        pinned: Cell::new(pinned),
        open_front: Cell::new(open_front),
        born: Cell::new(GetTickCount64()),
        shown: Cell::new(false),
        decode_gen: Cell::new(0),
        hot: Cell::new(None),
        focus: Cell::new(None),
        focus_gen: Cell::new(0),
        tip: Cell::new(HWND::default()),
        tip_rects: RefCell::new(Vec::new()),
        tip_texts: RefCell::new(Vec::new()),
        poll_started: Cell::new(false),
        zoom: Cell::new(1.0),
        full_pending: Cell::new(false),
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
        md_has_remote: Cell::new(false),
        md_imgs: RefCell::new(super::markdown::ImgCache::new()),
        md_layout: RefCell::new(super::markdown::MdLayout::default()),
        md_remote_ok: Cell::new(false),
        // The headless shot can open straight into source view (`--shot --window preview --source`).
        src_view: Cell::new(shot.map(|o| o.source).unwrap_or(false)),
        src_capable: Cell::new(false),
        user_sized: Cell::new(false),
        fullscreen: Cell::new(None),
        busy: Cell::new(false),
        pending_close: Cell::new(false),
        pending_path: RefCell::new(None),
        #[cfg(feature = "html-preview")]
        webview: RefCell::new(None),
        back_dc: Cell::new(HDC::default()),
        back_bmp: Cell::new(HBITMAP::default()),
        back_stock: Cell::new(HGDIOBJ::default()),
        back_size: Cell::new((0, 0)),
    });
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(st) as isize);
    // GDI+ for this viewer's lifetime (torn down in WM_DESTROY). It goes HERE, not in a
    // WM_CREATE arm: `wndproc` hands every message that arrives before GWLP_USERDATA is set
    // straight to DefWindowProc, so a WM_CREATE arm would be dead code — and the only symptom
    // is that `gdip::with_aa` silently draws nothing, leaving an empty toolbar button.
    GDIP_TOKEN.with(|t| t.set(crate::gdip::startup()));
    if dark {
        crate::dark::dark_titlebar(hwnd);
    }

    if let Some(opts) = shot {
        load_sync(hwnd, initial_path.as_deref(), opts);
    } else {
        (*state(hwnd)).tip.set(create_tooltips(hwnd, hinst));
        // Armed BEFORE `load`, not after (2026-09-05 audit, F10): `load` now shows the window
        // itself before it reads anything, but this fallback stays first regardless, so a
        // future change to `load` can never again leave the show-fallback waiting behind a
        // synchronous read.
        SetTimer(Some(hwnd), SHOW_TIMER_ID, 120, None);
        if let Some(p) = initial_path {
            load(hwnd, &p);
        }
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

/// Window geometry/paint messages: hit-testing, sizing constraints, paint/print, and
/// resize-drag bookkeeping. `None` when `msg` isn't one of these; the caller tries the next
/// category.
unsafe fn on_geometry_msg(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    Some(match msg {
        WM_NCHITTEST => on_nchittest(hwnd, wparam, lparam),
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
        WM_SIZE => on_size(hwnd),
        WM_SIZING => {
            // A real frame drag (never our own SetWindowPos) — flag it so WM_EXITSIZEMOVE
            // knows this was a RESIZE and not just a move, and remembers the size.
            (*state(hwnd)).user_sized.set(true);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_EXITSIZEMOVE => {
            remember_size(hwnd);
            LRESULT(0)
        }
        WM_NCLBUTTONDBLCLK => {
            // Double-click the caption = forget the dragged size and fit this file again.
            // DefWindowProc would send SC_MAXIMIZE, which this WS_POPUP window can't honour
            // anyway, so nothing is being taken away.
            if wparam.0 as u32 == HTCAPTION {
                forget_size(hwnd);
                return Some(LRESULT(0));
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DPICHANGED => {
            crate::win::wm_dpichanged(hwnd, lparam);
            LRESULT(0)
        }
        _ => return None,
    })
}

/// App-defined async/custom messages (`WM_APP_*`, plus the video player's own registered
/// message) and the tick timer: decode/render completions posted from worker threads, and
/// periodic UI upkeep. `None` when `msg` isn't one of these.
unsafe fn on_app_msg(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    Some(match msg {
        WM_TIMER => on_timer(hwnd, wparam),
        WM_APP_RENDER => {
            on_render(hwnd, wparam, lparam);
            LRESULT(0)
        }
        WM_APP_ANIM => {
            on_anim(hwnd, lparam);
            LRESULT(0)
        }
        WM_APP_MDIMG => on_app_mdimg(hwnd, lparam),
        WM_APP_PDFDOC => on_app_pdfdoc(hwnd, lparam),
        WM_APP_PDFTILE => on_app_pdftile(hwnd, lparam),
        WM_APP_PDFSTRIP => on_app_pdfstrip(hwnd, lparam),
        WM_APP_PDFTEXT => on_app_pdftext(hwnd, lparam),
        WM_APP_PDFINFO => on_app_pdfinfo(hwnd, lparam),
        m if m == super::video::WM_APP_VIDEO => {
            on_video_event(hwnd, wparam.0 as u32);
            LRESULT(0)
        }
        WM_APP_SWITCH => on_app_switch(hwnd, lparam),
        WM_APP_LOAD_RESOLVED => {
            on_app_load_resolved(hwnd, lparam);
            LRESULT(0)
        }
        _ => return None,
    })
}

/// Mouse input over the content pane: movement, buttons, wheel, cursor. `None` when `msg`
/// isn't one of these.
unsafe fn on_mouse_msg(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    Some(match msg {
        WM_MOUSEMOVE => on_mousemove(hwnd, lparam),
        WM_MOUSELEAVE => on_mouseleave(hwnd),
        WM_LBUTTONDOWN => on_lbuttondown(hwnd, lparam),
        WM_LBUTTONUP => on_lbuttonup(hwnd, lparam),
        WM_CAPTURECHANGED => on_capturechanged(hwnd),
        WM_SETCURSOR => on_setcursor(hwnd, wparam, lparam),
        WM_LBUTTONDBLCLK => on_lbuttondblclk(hwnd, lparam),
        WM_MOUSEWHEEL => on_mousewheel(hwnd, wparam, lparam),
        _ => return None,
    })
}

/// Keyboard input, activation, and the remaining lifecycle/IPC messages. `None` when `msg`
/// isn't one of these.
unsafe fn on_key_and_lifecycle_msg(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    Some(match msg {
        WM_ACTIVATE => on_activate(hwnd, wparam),
        WM_KEYDOWN => on_keydown(hwnd, wparam, lparam),
        WM_CHAR => {
            // Only the find bar consumes typed characters; everything else falls through so
            // nothing else in the viewer changes behaviour.
            if super::find::on_char(hwnd, wparam.0 as u32) {
                return Some(LRESULT(0));
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_COPYDATA => {
            on_command(hwnd, lparam);
            LRESULT(1)
        }
        WM_DESTROY => on_destroy(hwnd),
        _ => return None,
    })
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
        if let Some(r) = on_geometry_msg(hwnd, msg, wparam, lparam) {
            return r;
        }
        if let Some(r) = on_app_msg(hwnd, msg, wparam, lparam) {
            return r;
        }
        if let Some(r) = on_mouse_msg(hwnd, msg, wparam, lparam) {
            return r;
        }
        if let Some(r) = on_key_and_lifecycle_msg(hwnd, msg, wparam, lparam) {
            return r;
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

/// `WM_NCHITTEST`: native thick frame handles resize; make the caption strip draggable.
unsafe fn on_nchittest(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let hit = DefWindowProcW(hwnd, WM_NCHITTEST, wparam, lparam);
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

/// `WM_TIMER`: the show-fallback, scrub-strip, animation-frame and outline-slide ticks.
unsafe fn on_timer(hwnd: HWND, wparam: WPARAM) -> LRESULT {
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

/// Log one debug line when a UI-thread pipeline stage (`apply_resolved`, the render post-back
/// handler `on_render`) took longer than [`sagethumbs2k_core::safety::PREVIEW_UI_STAGE_BUDGET`]:
/// see that constant's doc comment for the whole responsiveness contract this is part of
/// (audit E02, 2026-09-07). Diagnostic only: the stage has already run to completion by the
/// time this is called, nothing is aborted or retried. A stage that PUMPS the message loop
/// (WebView2 creation) is not a stall by definition: callers must exclude that route from
/// timing rather than rely on this to filter it out.
///
/// Takes the path as an owned `&str`, never a `&ViewerState`: a stage that can pump/destroy the
/// window may have freed the state by the time this runs, so the caller must capture whatever it
/// needs from `st` before making that call, not after.
fn log_ui_stage_stall(stage: &str, elapsed: std::time::Duration, gen: u64, path: &str) {
    if let Some(line) = sagethumbs2k_core::safety::stage_stall_report(
        stage,
        elapsed,
        sagethumbs2k_core::safety::PREVIEW_UI_STAGE_BUDGET,
        gen,
        path,
    ) {
        sagethumbs2k_core::safety::log_debug(&line);
    }
}

/// Rate-limited debug line for an abandoned decode/prepare worker, shared by
/// `loader::abandon_pending_prepare` and `content::abandoned_logged` (audit E02, 2026-09-07:
/// abandoned work must stay OBSERVABLE, not just bounded). A held arrow key can abandon a worker
/// on every repeat (the mash bench's exact case, see `content::bench_abandoned_count`, which
/// stays an EXACT, un-rate-limited counter: only this log line is throttled), and logging every
/// single one would flood the diagnostics log for no extra signal, so this logs at most once per
/// `ABANDON_LOG_WINDOW`. `context` names which worker gave up, for the log line only.
pub(super) fn log_abandoned_worker(context: &str) {
    const ABANDON_LOG_WINDOW: std::time::Duration = std::time::Duration::from_millis(250);
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    let now = std::time::Instant::now();
    // A short, explicit lock scope, never held across the `log_debug` call below (see
    // docs/DEVELOPMENT_GOTCHAS.md, "`if let Some(x) = *MUTEX.lock()` holds the lock for the
    // whole body": this copies the decision out and drops the guard before doing anything else).
    let should_log = {
        let mut last = LAST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let due = match *last {
            Some(prev) => now.duration_since(prev) >= ABANDON_LOG_WINDOW,
            None => true,
        };
        if due {
            *last = Some(now);
        }
        due
    };
    if !should_log {
        return;
    }
    sagethumbs2k_core::safety::log_debugf!(
        "preview {context}: abandoned a worker ({} of {} abandoned-worker budget slots live)",
        sagethumbs2k_core::safety::abandoned_workers(),
        sagethumbs2k_core::safety::MAX_ABANDONED_WORKERS,
    );
}

/// `WM_SIZE`: free the stale-sized back-buffer, re-place child windows, clamp scroll.
unsafe fn on_size(hwnd: HWND) -> LRESULT {
    let st = &*state(hwnd);
    // The cached back-buffer bitmap was sized to the OLD client rect; keeping it
    // would blit stale-size content (or a mismatched BitBlt) on the very next paint.
    // Free it now so `paint::ensure_back_buffer` allocates fresh at the new size.
    free_back_buffer(st);
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
    let _ = InvalidateRect(Some(hwnd), None, false);
    LRESULT(0)
}

/// `WM_APP_SWITCH`: the follow-selection poll saw a new selection.
unsafe fn on_app_switch(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    // The follow-selection poll saw a new selection: switch to it (unless it's
    // already what we're showing).
    let path = *Box::from_raw(lparam.0 as *mut String);
    let st = &*state(hwnd);
    if st.path.borrow().as_deref() != Some(path.as_str()) {
        request_load(hwnd, &path);
    }
    LRESULT(0)
}

/// `WM_ACTIVATE`: close-on-focus-loss (opt-in setting; never when pinned; not during the
/// open grace so a just-shown, never-activated window can't self-close).
unsafe fn on_activate(hwnd: HWND, wparam: WPARAM) -> LRESULT {
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

/// `WM_DESTROY`: tear down GDI+, the tooltip control, the back buffer, and free `ViewerState`.
unsafe fn on_destroy(hwnd: HWND) -> LRESULT {
    let tok = GDIP_TOKEN.with(|t| t.replace(0));
    if tok != 0 {
        crate::gdip::shutdown(tok);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ViewerState;
    if !ptr.is_null() {
        // Close is a CANCEL, not silence (audit E02, 2026-09-07): a decode/prepare worker still
        // running for this window must be told nobody is waiting, the same way switching files
        // already tells it via `reset_viewer_state`. Bumping the generation trips
        // `content::abandoned`'s check for any in-flight decode worker; giving up the pending
        // prepare ticket counts it against the abandoned-worker budget like any other
        // abandonment. Without this a worker orphaned by closing the window was neither told
        // nor counted: it just kept running unseen until it finished or hit its own budget.
        let next_gen = (*ptr).decode_gen.get() + 1;
        (*ptr).decode_gen.set(next_gen);
        content::begin_generation(next_gen);
        abandon_pending_prepare();
        let tip = (*ptr).tip.get();
        if !tip.is_invalid() {
            let _ = DestroyWindow(tip); // owned popup; destroy before the state frees
        }
        free_back_buffer(&*ptr); // release the cached WM_PAINT double-buffer GDI handles
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(Box::from_raw(ptr)); // frees RenderData (HBITMAP) + InfoCard (HICON)
    }
    PostQuitMessage(0);
    LRESULT(0)
}

/// React to a Media Foundation engine event. The player itself only ever REPORTS (it is borrowed
/// while it runs), so the two events that need the viewer to act are handled here.
unsafe fn on_video_event(hwnd: HWND, event: u32) {
    let st = &*state(hwnd);
    let (what, dims) = {
        let vb = st.video.borrow();
        match vb.as_ref() {
            Some(p) => {
                let what = p.on_event(event); // CANPLAY -> autoplay, etc.
                let dims = if what == super::video::VideoEvent::Metadata {
                    p.native_size()
                } else {
                    None
                };
                (what, dims)
            }
            None => return,
        }
    };
    match what {
        super::video::VideoEvent::Metadata => {
            // The clip's REAL size (rotation applied) is finally known. Until now the window was a
            // placeholder 16:9 shell, so a portrait phone clip sat letterboxed inside it. Re-size to
            // the true aspect, but never when the user has already dragged their own size or gone
            // full-screen: `client_size` honours both, so simply re-running it is the whole check.
            if dims.is_none() || st.fullscreen.get().is_some() {
                return;
            }
            st.video_dims.set(dims);
            let (cw, ch) = client_size(hwnd);
            place(hwnd, cw, ch, None);
        }
        super::video::VideoEvent::Error => {
            // The source opened but cannot actually be decoded, so nothing will ever appear on the
            // render child. Drop the engine (its Drop destroys the child window) and fall back to
            // the still-frame path, which is the same fallback `loader::load` uses when the engine
            // refuses the file outright.
            let _ = KillTimer(Some(hwnd), SCRUB_TIMER_ID);
            *st.video.borrow_mut() = None;
            st.video_dims.set(None);
            if let Some(p) = st.path.borrow().as_ref().cloned() {
                st.kind.set(ContentKind::Loading);
                content::spawn_decode(hwnd, p, st.decode_gen.get());
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        super::video::VideoEvent::None => {}
    }
}

/// Handle a decode result: install the image (or fall back to an InfoCard on failure), then
/// size + show / resize.
unsafe fn on_render(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) {
    // MUST stay `content::SharedRgba` — this is a hand-written cast back from a raw pointer,
    // so a type that disagrees with what `content::post_render` boxed is UB the compiler
    // cannot see. Naming the shared alias (rather than spelling the type out) is what keeps
    // the two ends in step.
    let boxed = Box::from_raw(lparam.0 as *mut (u64, Option<content::SharedRgba>));
    let (gen, decoded) = *boxed;
    let st = &*state(hwnd);
    if gen != st.decode_gen.get() {
        return; // stale — the user already switched files
    }
    let _ = wparam;
    // Cloned up front, same discipline as `on_app_load_resolved`: nothing here currently pumps
    // the message loop, but the stage timer must never read `st` after a call that might.
    let path = st.path.borrow().clone().unwrap_or_default();
    let stage_start = std::time::Instant::now();
    // A decode landing while the kind is STILL Video is the audio cover art (loader only asks for
    // one in that case, and the video fallback path sets Loading before it asks). It is a backdrop,
    // not the content: install it into `art`, leave the kind alone, and never fall back to the card
    // for it — a track with no embedded picture just keeps the plain dark surface.
    if st.kind.get() == ContentKind::Video {
        if let Some(d) = decoded {
            if let Some(hbmp) = content::make_dib(d.w, d.h, &d.rgba, letterbox_bg(st)) {
                *st.art.borrow_mut() = Some(RenderData::opaque(hbmp, d.w, d.h));
            }
        }
        let _ = InvalidateRect(Some(hwnd), None, false);
    } else {
        match decoded {
            Some(d) => match content::make_render_for(&d, letterbox_bg(st)) {
                Some(rd) => {
                    // A full-resolution decode landing clears any pending request for one,
                    // whether this IS that decode or the user simply navigated to a small image.
                    if d.is_full() {
                        st.full_pending.set(false);
                    }
                    *st.render.borrow_mut() = Some(rd);
                    st.kind.set(ContentKind::Image);
                }
                // A successful DECODE that then fails to become a DIB (e.g. CreateDIBSection
                // under memory pressure) must not orphan a valid image already on screen,
                // mirrors the None-decode guard just below rather than falling to InfoCard.
                None if st.render.borrow().is_some() => st.full_pending.set(false),
                None => fallback_card(st),
            },
            // A failed decode must never REPLACE a picture that is already on screen. That only
            // became reachable once the fit view started being served by a scaled decode: a
            // subsequent full-resolution fetch can fail (a file deleted mid-zoom, a format the
            // scaled path opened and the buffered one refuses) and swapping the visible image
            // for an error card would be a plain downgrade. With nothing installed yet, the card
            // is still the right answer.
            None if st.render.borrow().is_some() => st.full_pending.set(false),
            None => fallback_card(st), // decode failure / timeout → the calm card
        }
        ensure_shown(hwnd);
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
    log_ui_stage_stall("render", stage_start.elapsed(), gen, &path);
}

/// Fetch the real pixels if the zoom has outgrown the codec-scaled ones the fit view is served
/// from. A no-op — one comparison — for a full-resolution render and for any un-zoomed image.
///
/// Called from the paint path rather than from the zoom handlers, so a window resize, a
/// full-screen toggle and a wheel notch are all covered by the same check instead of three that
/// have to be kept in step. `full_pending` is what stops the repaint that follows from asking
/// again before the first answer arrives.
pub(super) unsafe fn ensure_full_for_zoom(hwnd: HWND, rc: &RECT) {
    let st = &*state(hwnd);
    if st.full_pending.get() || st.kind.get() != ContentKind::Image {
        return;
    }
    let wanted = st
        .render
        .borrow()
        .as_ref()
        .is_some_and(|rd| content::wants_full_resolution(rd, rc, st.zoom.get()));
    if !wanted {
        return;
    }
    let Some(path) = st.path.borrow().as_ref().cloned() else {
        return;
    };
    st.full_pending.set(true);
    content::spawn_decode_full(hwnd, path, st.decode_gen.get());
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
        if let Some(rd) = content::make_render(d.w, d.h, &d.rgba, bg) {
            rds.push(rd);
            delays.push(ms);
        }
    }
    if rds.len() < 2 {
        // couldn't build enough frames → fall through to a normal single-frame decode
        if let Some(p) = st.path.borrow().as_ref().cloned() {
            // `spawn_decode_full`, NOT `spawn_decode`. `spawn_decode` re-detects the animated
            // extension and re-runs the frame decode, which yields the same frame list that has
            // just failed to become bitmaps - so it posts `WM_APP_ANIM` again, lands back here,
            // and retries forever (a fresh thread and a full re-read every cycle) with the
            // window stuck on "Loading". `spawn_decode_full` skips the animation branch
            // entirely, which is exactly the single-frame fallback this arm promises.
            content::spawn_decode_full(hwnd, p, gen);
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

mod clipboard;
mod command;
pub(super) mod navigate;
mod zoom;
pub(in crate::preview) use clipboard::*;
pub(in crate::preview) use command::*;
pub(in crate::preview) use navigate::*;
pub(in crate::preview) use zoom::*;
mod scroll;
pub(super) use scroll::*;
// G154 split (2026-09-08): mouse/hit-testing, the PDF/Markdown async message handlers, and
// keyboard dispatch moved out here. Unlike the five children above, nothing outside this file
// calls into any of them by name, so they stay a plain private glob import (no
// `pub(in crate::preview)`) — `pub(super)` in each child is enough to reach this file, and
// `window.rs`'s own `tests` module (a descendant of this module) sees the re-imported names
// the same way it already saw them when they lived here directly.
mod keys;
mod mouse;
mod pdfasync;
use keys::*;
use mouse::*;
use pdfasync::*;

#[cfg(test)]
mod tests {
    use super::{
        nav_key_action, pdf_wheel_action, savepage_visible, scroll_from_thumb_offset,
        scroll_thumb_geometry, sort_paths_like_explorer, text_scroll_limits, wheel_notches,
        ContentKind, NavKey, WheelAction,
    };
    use std::path::PathBuf;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        VK_DOWN, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RIGHT, VK_UP,
    };

    /// `Btn::SavePage` gains a THIRD way to show, on top of the pre-existing PDF/animation
    /// cases: a live video player. Without this arm the button stayed hidden for every video,
    /// which is the whole defect this queue item exists to fix.
    #[test]
    fn savepage_shows_for_a_video_with_a_live_player() {
        assert!(savepage_visible(ContentKind::Video, 0, 0, true));
        // No player yet (fell back to an info card) — nothing to grab a frame from.
        assert!(!savepage_visible(ContentKind::Video, 0, 0, false));
    }

    /// The pre-existing PDF/animation gates must survive the video arm being added alongside
    /// them — a video-only regression check would miss a change that broke these instead.
    #[test]
    fn savepage_keeps_its_pdf_and_animation_gates() {
        assert!(savepage_visible(ContentKind::Image, 3, 0, false)); // multi-page PDF
        assert!(savepage_visible(ContentKind::Image, 0, 4, false)); // animation, >1 frame
        assert!(!savepage_visible(ContentKind::Image, 1, 1, false)); // neither paged nor animated
        assert!(!savepage_visible(ContentKind::Text, 3, 4, false)); // wrong content kind entirely
    }

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

    /// The wheel over a PDF. 2.3.1 shipped with this doing nothing at all: the routing sat
    /// inside the wndproc, fell through to the ordinary image zoom, and that drives state the
    /// tiled PDF paint never reads. Every test I had drove the keyboard or called the scroll
    /// function directly, so all of them passed against a build where rolling the wheel on a
    /// PDF was inert while the release notes promised it scrolled.
    #[test]
    fn a_bare_wheel_over_a_pdf_scrolls_the_document() {
        assert_eq!(pdf_wheel_action(false, false), WheelAction::Scroll);
    }

    /// The modifiers, and the precedence between them. Ctrl wins over Shift, so a hand resting
    /// on both gets the magnifier rather than a sideways jolt.
    #[test]
    fn ctrl_magnifies_and_shift_slides_sideways() {
        assert_eq!(pdf_wheel_action(true, false), WheelAction::Zoom);
        assert_eq!(pdf_wheel_action(false, true), WheelAction::Pan);
        assert_eq!(
            pdf_wheel_action(true, true),
            WheelAction::Zoom,
            "Ctrl beats Shift; both held must not pan"
        );
    }

    /// The regression this whole split exists for. A multi-page PDF used to swallow ←/→ for
    /// paging, and since `goto_pdf_page` clamps at both ends there was then NO key at all that
    /// reached the next file: the popup was a dead end until you closed it. Asserted for BOTH
    /// states of the flag, because the bug was precisely that one state behaved differently.
    #[test]
    fn left_right_always_move_between_files_even_on_a_multipage_pdf() {
        for multipage_pdf in [false, true] {
            assert_eq!(
                nav_key_action(multipage_pdf, VK_RIGHT.0),
                Some(NavKey::File(1)),
                "→ must reach the next FILE (multipage_pdf = {multipage_pdf})"
            );
            assert_eq!(
                nav_key_action(multipage_pdf, VK_LEFT.0),
                Some(NavKey::File(-1)),
                "← must reach the previous FILE (multipage_pdf = {multipage_pdf})"
            );
        }
    }

    /// Paging still has to work, or the fix above would just have deleted the feature.
    #[test]
    fn a_multipage_pdf_pages_on_up_down_and_pgup_pgdn() {
        assert_eq!(nav_key_action(true, VK_DOWN.0), Some(NavKey::Page(1)));
        assert_eq!(nav_key_action(true, VK_NEXT.0), Some(NavKey::Page(1)));
        assert_eq!(nav_key_action(true, VK_UP.0), Some(NavKey::Page(-1)));
        assert_eq!(nav_key_action(true, VK_PRIOR.0), Some(NavKey::Page(-1)));
    }

    /// Off a multi-page PDF nothing changed: PgUp/PgDn keep flipping files, and ↑/↓ stay
    /// unclaimed so scrolling and the default handling still get them.
    #[test]
    fn ordinary_content_keeps_its_previous_key_meanings() {
        assert_eq!(nav_key_action(false, VK_NEXT.0), Some(NavKey::File(1)));
        assert_eq!(nav_key_action(false, VK_PRIOR.0), Some(NavKey::File(-1)));
        assert_eq!(nav_key_action(false, VK_UP.0), None);
        assert_eq!(nav_key_action(false, VK_DOWN.0), None);
    }

    /// Whatever the content, there is always a way out of the current file. A future edit that
    /// claims ←/→ for anything else fails here rather than shipping another dead end.
    #[test]
    fn no_content_state_can_trap_the_keyboard() {
        for multipage_pdf in [false, true] {
            let escapes = [VK_RIGHT.0, VK_LEFT.0, VK_NEXT.0, VK_PRIOR.0]
                .into_iter()
                .filter(|&vk| matches!(nav_key_action(multipage_pdf, vk), Some(NavKey::File(_))))
                .count();
            assert!(
                escapes >= 2,
                "multipage_pdf = {multipage_pdf}: only {escapes} keys still reach another file"
            );
        }
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
