//! Continuous scrolling for multi-page PDFs in the Quick preview.
//!
//! Before this, a PDF was ONE page rendered to a bitmap, and reaching page two meant a key
//! press that threw that bitmap away and rendered another. That is a page-turner, not a
//! document viewer, and it is the specific thing a user comparing us to macOS Quick Look
//! notices first.
//!
//! # Why this rides ON TOP of `ContentKind::Image` instead of becoming its own kind
//!
//! A PDF is still `ContentKind::Image` and `st.render` still holds the CURRENT page. Twenty
//! seven places branch on that kind: Convert, Resize, Copy, OCR, Image-info, the zoom, the
//! window sizing, the toolbar's button visibility. Introducing `ContentKind::Pdf` would mean
//! auditing every one of them to keep behaviour that is already correct, and the failure mode
//! of missing one is a feature silently disappearing for PDFs only. So the scroll view is an
//! OVERLAY: when a document has more than one page and the session opened, paint and input
//! consult this module; everything else keeps reading `st.render` and cannot tell.
//!
//! # Layout is known before anything is rasterized
//!
//! [`sagethumbs2k_core::pdf::PdfSession`] reports every page's size on open, without drawing
//! any of them. That is what makes the scrollbar honest from the first frame: the document's
//! full height is known immediately, so the thumb never resizes and the view never jumps as
//! pages arrive. Pages rasterize lazily as they scroll into view, and a page still in flight
//! draws as a plain sheet rather than a hole.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
mod index;
mod nav;
mod strip;
pub(in crate::preview) use index::{
    index_note, index_snapshot, page_for_offset, start_indexing, IndexSnapshot, PdfIndex,
};
pub(in crate::preview) use nav::{
    active, pan_by, scroll_by, scroll_to_page, viewport_step, zoom_by,
};
#[cfg(test)]
pub(in crate::preview) use strip::strip_page_at;
pub(in crate::preview) use strip::{paint_strip, strip_click, strip_width};

use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, DrawTextW, FillRect, InvalidateRect, SelectObject, SetBkMode,
    SetTextColor, DT_CENTER, DT_SINGLELINE, HDC, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use sagethumbs2k_core::pdf::{PageSize, PdfSession};

use super::content::{self, RenderData};
use super::window::{state, ContentKind, ViewerState, WM_APP_PDFTILE};

/// What a worker posts back for one rasterized page: the generation it belongs to, which page,
/// the width it was rendered at, and the pixels (`None` when the page would not rasterize).
/// Named because both the sender and the `WM_APP_PDFTILE` handler must agree on it exactly, and
/// a tuple written out twice is a tuple that eventually disagrees with itself.
pub(in crate::preview) type TilePayload = (u64, usize, i32, Option<(i32, i32, Vec<u8>)>);

/// What the text indexer posts back for one page: the generation, the page, and its recognized
/// text. `None` means the recognizer FAILED on that page, which is a different thing from a page
/// that genuinely holds no text and is counted separately (see [`PdfIndex::failed`]).
pub(in crate::preview) type TextPayload = (u64, usize, Option<String>);

/// Gap between pages, in unscaled pixels. Enough to read as a break between sheets without
/// wasting a scroll's worth of empty space on a small window.
const PAGE_GAP: i32 = 10;

/// How many pages beyond the visible range to rasterize ahead. One is deliberate: a flick
/// scroll wants the next sheet ready, and rendering the whole document would spend the user's
/// battery on pages they will never look at.
const PREFETCH: usize = 1;

/// Hard ceiling on how many rendered page bitmaps are held at once. Each is a full DIB, so a
/// long document scrolled end to end would otherwise grow without limit. Pages outside the
/// window are dropped and re-rendered if scrolled back to, which is cheap against an open
/// session and invisible next to the cost of keeping them.
const MAX_LIVE_PAGES: usize = 12;

/// Same ceiling as `MAX_LIVE_PAGES`, for the strip's own thumbnail cache. Kept as a separate
/// constant (even though the value matches today) because the two caches serve different
/// windows — the strip and the reader rarely show the same pages — and tying them together
/// would make a future change to one silently change the other.
const MAX_LIVE_STRIP_TILES: usize = 12;

/// The width every page is laid out and rasterized at: the content area less a small margin,
/// so a sheet is not flush against the window edge.
///
/// ONE definition, called by paint and by both scroll paths. They used to disagree: the scroll
/// paths read `tile_width`, which is 0 until the first paint has run, and a key pressed in that
/// window laid the document out at one pixel wide and scrolled to a nonsense position. The
/// document arrives asynchronously, so "a key press before the first paint" is not a corner
/// case, it is what happens when someone holds Down while a PDF opens.
pub(in crate::preview) unsafe fn render_width(hwnd: HWND) -> i32 {
    render_width_in(hwnd, &super::window::content_rect(hwnd))
}

/// [`render_width`] for a content rect the caller already holds.
///
/// This split exists for one caller and one trap: [`paint`] runs with the document cell
/// mutably borrowed, and `content_rect` consults [`strip_width`], whose `try_borrow` FAILS
/// under that borrow and reports no strip. So paint asking `render_width(hwnd)` mid-borrow
/// got a width ~116 px wider than every scroll path computed, the two layouts disagreed, and
/// every jump-to-page landed ~300 px short of the page it named - systematically, and only
/// while the strip was visible. Paint must derive the width from the rect it was HANDED
/// (computed before the borrow), and sharing this one function with `render_width` is what
/// keeps the two from drifting apart again.
pub(in crate::preview) unsafe fn render_width_in(
    hwnd: HWND,
    content: &windows::Win32::Foundation::RECT,
) -> i32 {
    let margin = crate::win::dpi_scale(hwnd, 8);
    ((content.right - content.left) - 2 * margin).max(16)
}

/// Zoom limits. The floor is fit-width: below that the page is smaller than the pane and there
/// is nothing to see that fit-width does not already show, so "zoom out" past it is not a
/// feature, it is empty margin. The ceiling keeps a rendered page inside sane memory - at 4x a
/// letter page is roughly 4900 px wide, which is already past what any display shows at once.
const ZOOM_MIN: f64 = 1.0;
const ZOOM_MAX: f64 = 4.0;

/// The open document behind the continuous view.
pub(in crate::preview) struct PdfDoc {
    session: Arc<PdfSession>,
    /// Page sizes as the document declares them, in DIPs. The ratio is all layout needs.
    sizes: Vec<PageSize>,
    /// Rendered sheets, indexed by page. `None` = not rendered (or evicted).
    tiles: Vec<Option<RenderData>>,
    /// A render already in flight for this page, so a slow page is not requested on every
    /// paint while it is still being drawn.
    pending: Vec<bool>,
    /// The pixel width every tile was rendered at. A resize that changes this invalidates all
    /// of them, because a stretched sheet is exactly the blurry-page complaint this replaces.
    tile_width: i32,
    /// Scroll position in laid-out pixels from the top of page one.
    pub(in crate::preview) scroll: i32,
    /// Magnification over fit-width. 1.0 = the page exactly fills the content area.
    ///
    /// Pages are RE-RENDERED at the zoomed width rather than the fit-width bitmap being
    /// stretched, which is the whole point: a PDF is text, and a stretched bitmap of text is
    /// the blurry mess this exists to avoid. Changing it invalidates every tile, which is
    /// correct and cheap against an already-open session.
    pub(in crate::preview) zoom: f64,
    /// Horizontal offset in laid-out pixels, for when zoom makes a page wider than the pane.
    /// Always 0 while the page still fits, so an unzoomed document behaves exactly as before.
    pub(in crate::preview) pan_x: i32,
    /// Thumbnails for the side strip, indexed by page. Deliberately a SECOND cache rather than
    /// a smaller entry in `tiles`: these are ~116 px wide against the page tiles' ~1200, they
    /// are wanted for a different set of pages (whatever the strip shows, not what the reader
    /// is looking at), and mixing the two would have one eviction policy fighting two jobs.
    strip_tiles: Vec<Option<RenderData>>,
    strip_pending: Vec<bool>,
    /// The width strip thumbnails were rendered at; a DPI or layout change invalidates them.
    strip_width: i32,
    /// First page shown in the strip, so a long document can scroll its own contact sheet.
    pub(in crate::preview) strip_top: usize,
    /// The document's searchable text, read in the background when someone presses Ctrl+F.
    index: PdfIndex,
    /// Set when this document is dropped, so a text index still grinding through a two hundred
    /// page file stops instead of burning a minute of CPU on a file the reader has left, and so
    /// an in-flight tile/strip render (`spawn_render`) skips its work and its post rather than
    /// waking a window whose `PdfDoc` no longer exists. Shared with every such worker, which
    /// checks it before starting and again before posting.
    cancel: Arc<AtomicBool>,
    /// Bumped when the document is replaced, so a tile from the previous file is dropped.
    pub(in crate::preview) gen: u64,
}

impl Drop for PdfDoc {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Where every page sits in the scrolled document, at a given render width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::preview) struct Layout {
    /// Top edge of each page, in laid-out pixels.
    pub tops: Vec<i32>,
    /// Height of each page at this width.
    pub heights: Vec<i32>,
    /// Full document height, including the gaps between pages but not after the last one.
    pub total: i32,
}

/// Lay pages out in a single column at `width` px.
///
/// Pure, and separated from everything Win32 because this is where continuous scrolling is
/// actually right or wrong: an off-by-one in a page top is a view that drifts a few pixels
/// further out of true with every page, which nobody notices until page forty.
pub(in crate::preview) fn layout(sizes: &[PageSize], width: i32, gap: i32) -> Layout {
    let width = width.max(1);
    let mut tops = Vec::with_capacity(sizes.len());
    let mut heights = Vec::with_capacity(sizes.len());
    let mut y = 0i32;
    for (i, s) in sizes.iter().enumerate() {
        let h = ((f64::from(s.h) / f64::from(s.w.max(1.0))) * f64::from(width)).round() as i32;
        let h = h.clamp(1, 1 << 20);
        if i > 0 {
            y = y.saturating_add(gap);
        }
        tops.push(y);
        heights.push(h);
        y = y.saturating_add(h);
    }
    Layout {
        tops,
        heights,
        total: y.max(0),
    }
}

/// The furthest the view may scroll: never past the last page's bottom, and never at all when
/// the whole document already fits.
pub(in crate::preview) fn max_scroll(total: i32, viewport_h: i32) -> i32 {
    (total - viewport_h.max(0)).max(0)
}

/// Which pages intersect the viewport, as an inclusive-exclusive range. Empty only when there
/// are no pages at all: a scroll position past the end still yields the last page, because
/// clamping is the caller's job and returning nothing would paint a blank window.
pub(in crate::preview) fn visible_range(
    l: &Layout,
    scroll: i32,
    viewport_h: i32,
) -> (usize, usize) {
    if l.tops.is_empty() {
        return (0, 0);
    }
    let top = scroll;
    let bottom = scroll.saturating_add(viewport_h.max(1));
    let mut first = l.tops.len() - 1;
    let mut last = 0usize;
    let mut any = false;
    for i in 0..l.tops.len() {
        let (a, b) = (l.tops[i], l.tops[i] + l.heights[i]);
        if b > top && a < bottom {
            if !any {
                first = i;
                any = true;
            }
            last = i;
        }
    }
    if !any {
        // Scrolled clean past the end (or before the start): show the nearest page rather than
        // nothing, so a stale scroll can never present an empty document.
        let i = if top >= l.total { l.tops.len() - 1 } else { 0 };
        return (i, i + 1);
    }
    (first, last + 1)
}

/// The page the caption should name: the one covering the most of the viewport.
///
/// NOT "the page at the top edge". Scrolling one pixel past a page boundary would then renumber
/// the caption while the previous page still fills the window, which reads as a bug.
pub(in crate::preview) fn page_at(l: &Layout, scroll: i32, viewport_h: i32) -> usize {
    if l.tops.is_empty() {
        return 0;
    }
    let top = scroll;
    let bottom = scroll.saturating_add(viewport_h.max(1));
    let mut best = 0usize;
    let mut best_cover = -1i32;
    for i in 0..l.tops.len() {
        let (a, b) = (l.tops[i], l.tops[i] + l.heights[i]);
        let cover = b.min(bottom) - a.max(top);
        if cover > best_cover {
            best_cover = cover;
            best = i;
        }
    }
    best
}

impl PdfDoc {
    pub(in crate::preview) fn new(session: PdfSession, gen: u64) -> Self {
        let sizes = session.sizes().to_vec();
        let n = sizes.len();
        Self {
            session: Arc::new(session),
            sizes,
            tiles: (0..n).map(|_| None).collect(),
            pending: vec![false; n],
            tile_width: 0,
            scroll: 0,
            zoom: 1.0,
            pan_x: 0,
            strip_tiles: (0..n).map(|_| None).collect(),
            strip_pending: vec![false; n],
            strip_width: 0,
            strip_top: 0,
            index: PdfIndex::default(),
            cancel: Arc::new(AtomicBool::new(false)),
            gen,
        }
    }

    /// Install one page's recognized text. Returns whether the index moved, so the caller only
    /// re-runs an open search when there is something new to search.
    pub(in crate::preview) fn put_page_text(&mut self, page: usize, text: Option<&str>) -> bool {
        self.index.push(page, text)
    }

    fn snapshot(&self) -> IndexSnapshot {
        IndexSnapshot {
            hay: self.index.hay.clone(),
            starts: self.index.starts.clone(),
            done: self.index.done(),
            total: self.index.total,
            failed: self.index.failed,
            pages: self.sizes.len(),
        }
    }

    pub(in crate::preview) fn page_count(&self) -> usize {
        self.sizes.len()
    }

    pub(in crate::preview) fn layout_at(&self, width: i32, gap: i32) -> Layout {
        layout(&self.sizes, width, gap)
    }

    /// The width pages are laid out and rasterized at: fit-width times the zoom.
    pub(in crate::preview) fn zoomed_width(&self, base: i32) -> i32 {
        ((f64::from(base.max(1)) * self.zoom).round() as i32).clamp(16, 1 << 15)
    }

    /// Change magnification. Returns whether it moved, so a caller at a limit can leave the
    /// wheel alone rather than swallowing it. Tiles are dropped because they were rendered for
    /// the old width and stretching them is exactly what zoom is meant to stop.
    pub(in crate::preview) fn set_zoom(&mut self, want: f64) -> bool {
        let z = want.clamp(ZOOM_MIN, ZOOM_MAX);
        if (z - self.zoom).abs() < 0.001 {
            return false;
        }
        self.zoom = z;
        self.invalidate_tiles();
        true
    }

    /// Drop every tile. Called when the render width changes, because a tile rendered for a
    /// different width would be blitted stretched, and "the pages went blurry when I resized
    /// the window" is the same complaint in a different coat.
    fn invalidate_tiles(&mut self) {
        for t in self.tiles.iter_mut() {
            *t = None;
        }
        for p in self.pending.iter_mut() {
            *p = false;
        }
    }

    /// Keep memory bounded: drop tiles far from the pages currently on screen.
    fn evict_outside(&mut self, first: usize, last: usize) {
        let Some((keep_lo, keep_hi)) =
            eviction_keep_range(first, last, self.tiles.len(), MAX_LIVE_PAGES)
        else {
            return;
        };
        for (i, t) in self.tiles.iter_mut().enumerate() {
            if i < keep_lo || i >= keep_hi {
                *t = None;
            }
        }
    }

    /// Keep strip-thumbnail memory bounded the same way `evict_outside` bounds the main view:
    /// drop thumbnails far from the strip's currently visible window.
    fn evict_strip_outside(&mut self, first: usize, last: usize) {
        let Some((keep_lo, keep_hi)) =
            eviction_keep_range(first, last, self.strip_tiles.len(), MAX_LIVE_STRIP_TILES)
        else {
            return;
        };
        for (i, t) in self.strip_tiles.iter_mut().enumerate() {
            if i < keep_lo || i >= keep_hi {
                *t = None;
            }
        }
    }

    /// Install a strip thumbnail that arrived from a worker.
    pub(in crate::preview) fn put_strip_tile(&mut self, page: usize, width: i32, rd: RenderData) {
        if page >= self.strip_tiles.len() || width != self.strip_width {
            return;
        }
        self.strip_pending[page] = false;
        self.strip_tiles[page] = Some(rd);
    }

    pub(in crate::preview) fn clear_strip_pending(&mut self, page: usize) {
        if let Some(p) = self.strip_pending.get_mut(page) {
            *p = false;
        }
    }

    /// Install a tile that arrived from a worker.
    pub(in crate::preview) fn put_tile(&mut self, page: usize, width: i32, rd: RenderData) {
        if page >= self.tiles.len() || width != self.tile_width {
            return; // a tile for a width we have since left; dropping it is correct
        }
        self.pending[page] = false;
        self.tiles[page] = Some(rd);
    }

    pub(in crate::preview) fn clear_pending(&mut self, page: usize) {
        if let Some(p) = self.pending.get_mut(page) {
            *p = false;
        }
    }
}

/// The `[keep_lo, keep_hi)` slice of a `len`-long tile cache to keep, given the wanted
/// `[first, last)` range plus a `PREFETCH` margin — `None` when the cache is at or under `cap`
/// and eviction is not worth doing. Pulled out of `evict_outside`/`evict_strip_outside` so the
/// range arithmetic is testable without a live `PdfDoc`/`PdfSession`.
fn eviction_keep_range(
    first: usize,
    last: usize,
    len: usize,
    cap: usize,
) -> Option<(usize, usize)> {
    if len <= cap {
        return None;
    }
    let keep_lo = first.saturating_sub(PREFETCH);
    let keep_hi = (last + PREFETCH).min(len);
    Some((keep_lo, keep_hi))
}

/// Ask a worker for page `page` at `width`, unless one is already in flight.
///
/// The session serialises renders on its own thread, so several requests queue rather than
/// fight; what this guards is issuing the SAME page repeatedly from every paint while the
/// first one is still being drawn.
unsafe fn request_tile(hwnd: HWND, doc: &mut PdfDoc, page: usize, width: i32, gen: u64) {
    if page >= doc.tiles.len() || doc.tiles[page].is_some() || doc.pending[page] {
        return;
    }
    doc.pending[page] = true;
    spawn_render(
        hwnd,
        Arc::clone(&doc.session),
        Arc::clone(&doc.cancel),
        page,
        width,
        gen,
        WM_APP_PDFTILE,
    );
}

/// Render page `page` at `width` on a worker and post the result as `msg` (`WM_APP_PDFTILE` or
/// `WM_APP_PDFSTRIP`), shared by `request_tile` and `request_strip_tile` — the two previously
/// duplicated the whole render body.
///
/// `cancel` is the document's own drop flag: checked before the render starts (so a worker
/// still queued behind a slow page skips the work entirely once the document is gone) and again
/// before posting (so a worker that was already rendering does not post into a window whose
/// `PdfDoc` no longer exists). A render that finishes before the document is dropped is always
/// posted, success or failure, so `pending`/`strip_pending` is cleared and the page can be
/// retried rather than staying blank forever behind a flag nobody resets.
unsafe fn spawn_render(
    hwnd: HWND,
    session: Arc<PdfSession>,
    cancel: Arc<AtomicBool>,
    page: usize,
    width: i32,
    gen: u64,
    msg: u32,
) {
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);
        let decoded = session
            .render_to_width(page, width.max(1) as u32)
            .and_then(|png| {
                image::load_from_memory(&png).ok().map(|img| {
                    let rgba = img.to_rgba8();
                    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
                    (w, h, rgba.into_raw())
                })
            });
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let payload: Box<TilePayload> = Box::new((gen, page, width, decoded));
        let raw = Box::into_raw(payload);
        if PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(raw as isize)).is_err() {
            drop(Box::from_raw(raw));
        }
    });
}

/// Open the document for continuous scrolling, on a worker.
///
/// Deliberately SEPARATE from `content::spawn_decode_pdf`, and deliberately second. Page one
/// must appear as fast as it always did; parsing a three-hundred-page file to learn how tall it
/// is can take a moment, and making first paint wait on that would trade a real improvement for
/// a visible regression. Until this lands the viewer is exactly the single-page pager it was.
pub(in crate::preview) unsafe fn spawn_open(hwnd: HWND, path: String, gen: u64) {
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);
        let Ok(bytes) = sagethumbs2k_core::decode::read_capped(&path) else {
            return;
        };
        let Some(session) = PdfSession::open(&bytes) else {
            return; // encrypted, malformed, or past the page cap: stay a pager
        };
        if session.page_count() < 2 {
            return; // nothing to scroll through
        }
        let payload: Box<(u64, PdfSession)> = Box::new((gen, session));
        let raw = Box::into_raw(payload);
        if PostMessageW(
            Some(hwnd),
            super::window::WM_APP_PDFDOC,
            WPARAM(0),
            LPARAM(raw as isize),
        )
        .is_err()
        {
            drop(Box::from_raw(raw));
        }
    });
}

/// Paint the continuous view. Returns false when there is nothing to paint with, so the caller
/// can fall back to the ordinary single-image path.
pub(in crate::preview) unsafe fn paint(
    hwnd: HWND,
    hdc: HDC,
    rc: &RECT,
    bg: u32,
    sheet: u32,
) -> bool {
    let st = &*state(hwnd);
    let mut slot = st.pdf_doc.borrow_mut();
    let Some(doc) = slot.as_mut() else {
        return false;
    };
    let cw = rc.right - rc.left;
    let ch = rc.bottom - rc.top;
    if cw <= 0 || ch <= 0 || doc.page_count() == 0 {
        return false;
    }

    // Fit-width times the zoom. At 1.0 the page exactly fills the content area, which is the
    // mode continuous scrolling means: the reader controls the vertical, the window controls
    // the horizontal. Zoomed in, the page is WIDER than the pane and `pan_x` slides it.
    //
    // From `rc`, NOT `render_width(hwnd)`: the document cell is mutably borrowed here, which
    // makes `strip_width`'s try_borrow fail and report no strip, so the hwnd route measures a
    // WIDER content area than the one this function was handed - see `render_width_in`.
    let width = doc.zoomed_width(render_width_in(hwnd, rc));
    if width != doc.tile_width {
        doc.tile_width = width;
        doc.invalidate_tiles();
    }
    let gap = crate::win::dpi_scale(hwnd, PAGE_GAP);
    let l = doc.layout_at(width, gap);
    doc.scroll = doc.scroll.clamp(0, max_scroll(l.total, ch));
    // A page narrower than the pane centres and cannot pan; a wider one pans within its
    // overhang. Clamped here, in paint, because that is the only place that knows both the
    // page width and the pane width for certain.
    doc.pan_x = doc.pan_x.clamp(0, (width - cw).max(0));

    let brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(bg));
    FillRect(hdc, rc, brush);
    let _ = DeleteObject(brush.into());

    let (first, last) = visible_range(&l, doc.scroll, ch);
    let x = if width <= cw {
        rc.left + (cw - width) / 2
    } else {
        rc.left - doc.pan_x
    };
    let sheet_brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(sheet));
    for i in first..last {
        let y = rc.top + l.tops[i] - doc.scroll;
        let page_rc = RECT {
            left: x,
            top: y,
            right: x + width,
            bottom: y + l.heights[i],
        };
        match doc.tiles.get(i).and_then(|t| t.as_ref()) {
            Some(rd) => content::blit_exact(hdc, &page_rc, rd),
            // A sheet, not a hole. A page still rasterizing reads as "loading" rather than
            // "broken", and the layout does not move when the real one lands.
            None => {
                FillRect(hdc, &page_rc, sheet_brush);
            }
        }
    }
    let _ = DeleteObject(sheet_brush.into());

    // Rasterize what is on screen plus a little ahead, then let go of the rest.
    let want_lo = first.saturating_sub(PREFETCH);
    let want_hi = (last + PREFETCH).min(doc.page_count());
    doc.evict_outside(first, last);
    let (gen, width_now) = (doc.gen, doc.tile_width);
    for i in want_lo..want_hi {
        request_tile(hwnd, doc, i, width_now, gen);
    }
    true
}

#[cfg(test)]
mod tests;
