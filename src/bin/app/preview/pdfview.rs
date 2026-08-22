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

use std::sync::Arc;

use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, DrawTextW, FillRect, InvalidateRect, SelectObject, SetBkMode,
    SetTextColor, DT_CENTER, DT_SINGLELINE, HDC, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use sagethumbs2k_core::pdf::{PageSize, PdfSession};

use super::content::{self, RenderData};
use super::window::{state, ContentKind, WM_APP_PDFTILE};

/// What a worker posts back for one rasterized page: the generation it belongs to, which page,
/// the width it was rendered at, and the pixels (`None` when the page would not rasterize).
/// Named because both the sender and the `WM_APP_PDFTILE` handler must agree on it exactly, and
/// a tuple written out twice is a tuple that eventually disagrees with itself.
pub(in crate::preview) type TilePayload = (u64, usize, i32, Option<(i32, i32, Vec<u8>)>);

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

/// Width of the page-thumbnail strip, in unscaled px. Wide enough that a page is recognisable
/// (you are looking for "the one with the table on it"), narrow enough not to be the point.
const STRIP_W: i32 = 116;

/// Below this client width the strip is not shown at all. A narrow popup has no room to give up
/// a sixth of itself, and a reader would rather have the page.
const STRIP_MIN_CLIENT: i32 = 520;

/// How wide the thumbnail strip is for THIS window, or 0 when it should not appear.
///
/// Consulted by `content_rect`, so the strip takes its slice once and paint, scroll clamping and
/// every hit test inherit the narrower content area automatically - the same integration the
/// find bar already uses. Returning 0 keeps every non-PDF window byte-identical.
pub(in crate::preview) unsafe fn strip_width(hwnd: HWND) -> i32 {
    let st = super::window::state(hwnd);
    if st.is_null() {
        return 0;
    }
    let st = &*st;
    if st.kind.get() != ContentKind::Image {
        return 0;
    }
    // `try_borrow`, not `borrow`: content_rect is called from inside paint, which already holds
    // this cell mutably. A panic there takes the window down with panic=abort.
    let Ok(slot) = st.pdf_doc.try_borrow() else {
        return 0;
    };
    let Some(doc) = slot.as_ref() else {
        return 0;
    };
    if doc.page_count() < 2 || !sagethumbs2k_core::settings::preview_pdf_strip() {
        return 0;
    }
    let mut r = windows::Win32::Foundation::RECT::default();
    let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut r);
    if r.right < crate::win::dpi_scale(hwnd, STRIP_MIN_CLIENT) {
        return 0;
    }
    crate::win::dpi_scale(hwnd, STRIP_W)
}

/// The width every page is laid out and rasterized at: the content area less a small margin,
/// so a sheet is not flush against the window edge.
///
/// ONE definition, called by paint and by both scroll paths. They used to disagree: the scroll
/// paths read `tile_width`, which is 0 until the first paint has run, and a key pressed in that
/// window laid the document out at one pixel wide and scrolled to a nonsense position. The
/// document arrives asynchronously, so "a key press before the first paint" is not a corner
/// case, it is what happens when someone holds Down while a PDF opens.
pub(in crate::preview) unsafe fn render_width(hwnd: HWND) -> i32 {
    let r = super::window::content_rect(hwnd);
    let margin = crate::win::dpi_scale(hwnd, 8);
    ((r.right - r.left) - 2 * margin).max(16)
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
    /// Bumped when the document is replaced, so a tile from the previous file is dropped.
    pub(in crate::preview) gen: u64,
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
            gen,
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
        if self.tiles.len() <= MAX_LIVE_PAGES {
            return;
        }
        let keep_lo = first.saturating_sub(PREFETCH);
        let keep_hi = (last + PREFETCH).min(self.tiles.len());
        for (i, t) in self.tiles.iter_mut().enumerate() {
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
    let session = Arc::clone(&doc.session);
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
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
        // Always post, even on failure, so `pending` is cleared and the page can be retried
        // rather than staying blank forever behind a flag nobody resets.
        let payload: Box<TilePayload> = Box::new((gen, page, width, decoded));
        let raw = Box::into_raw(payload);
        if PostMessageW(Some(hwnd), WM_APP_PDFTILE, WPARAM(0), LPARAM(raw as isize)).is_err() {
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
    let width = doc.zoomed_width(render_width(hwnd));
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

/// Vertical pitch of one strip entry: the thumbnail plus its caption and gap.
fn strip_pitch(hwnd: HWND, thumb_h: i32) -> i32 {
    thumb_h + crate::win::dpi_scale(hwnd, 22)
}

/// Which page a click at `y` inside the strip lands on, or `None` for the empty tail.
///
/// Pure so the arithmetic is testable: an off-by-one here sends the reader to the wrong page,
/// which is the kind of bug that is obvious in use and invisible in a screenshot.
pub(in crate::preview) fn strip_page_at(
    y_in_strip: i32,
    pitch: i32,
    top_page: usize,
    pages: usize,
) -> Option<usize> {
    if pitch <= 0 || pages == 0 || y_in_strip < 0 {
        return None;
    }
    let idx = top_page.checked_add((y_in_strip / pitch) as usize)?;
    (idx < pages).then_some(idx)
}

/// Paint the page-thumbnail strip. Returns false when there is no strip for this window.
pub(in crate::preview) unsafe fn paint_strip(hwnd: HWND, hdc: HDC, bg: u32, sheet: u32) -> bool {
    let rc = super::window::strip_rect(hwnd);
    let (sw, sh) = (rc.right - rc.left, rc.bottom - rc.top);
    if sw <= 0 || sh <= 0 {
        return false;
    }
    let st = &*state(hwnd);
    let mut slot = st.pdf_doc.borrow_mut();
    let Some(doc) = slot.as_mut() else {
        return false;
    };
    let pages = doc.page_count();
    if pages == 0 {
        return false;
    }

    let brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(bg));
    FillRect(hdc, &rc, brush);
    let _ = DeleteObject(brush.into());

    let pad = crate::win::dpi_scale(hwnd, 8);
    let tw = (sw - 2 * pad).max(8);
    if tw != doc.strip_width {
        doc.strip_width = tw;
        for t in doc.strip_tiles.iter_mut() {
            *t = None;
        }
        for p in doc.strip_pending.iter_mut() {
            *p = false;
        }
    }
    // Every page keeps its own aspect, so the pitch follows the FIRST page rather than assuming
    // a uniform document; a mixed-orientation file still lays out evenly enough to click.
    let first_sz = doc.session.size(0);
    let th =
        ((f64::from(first_sz.h) / f64::from(first_sz.w.max(1.0))) * f64::from(tw)).round() as i32;
    let th = th.clamp(8, 1 << 14);
    let pitch = strip_pitch(hwnd, th);

    // Keep the current page on screen without yanking the strip on every scroll.
    let cur = st.pdf_page.get() as usize;
    let visible = (sh / pitch).max(1) as usize;
    if cur < doc.strip_top {
        doc.strip_top = cur;
    } else if cur >= doc.strip_top + visible {
        doc.strip_top = cur + 1 - visible;
    }
    doc.strip_top = doc.strip_top.min(pages.saturating_sub(1));

    let sheet_brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(sheet));
    let sel_brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(
        crate::dark::ACCENT().0,
    ));
    let fonts = crate::win::gui_font_for(hwnd);
    let oldf = SelectObject(hdc, fonts.into());
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(
        hdc,
        windows::Win32::Foundation::COLORREF(crate::dark::HEADER_TEXT().0),
    );

    let (gen, width_now) = (doc.gen, doc.strip_width);
    let mut wanted: Vec<usize> = Vec::new();
    for slotn in 0..=visible {
        let page = doc.strip_top + slotn;
        if page >= pages {
            break;
        }
        let y = rc.top + pad + slotn as i32 * pitch;
        if y >= rc.bottom {
            break;
        }
        let cell = RECT {
            left: rc.left + pad,
            top: y,
            right: rc.left + pad + tw,
            bottom: y + th,
        };
        if page == cur {
            // A 2 px accent frame around the page being read.
            let f = crate::win::dpi_scale(hwnd, 2);
            let outer = RECT {
                left: cell.left - f,
                top: cell.top - f,
                right: cell.right + f,
                bottom: cell.bottom + f,
            };
            FillRect(hdc, &outer, sel_brush);
        }
        match doc.strip_tiles.get(page).and_then(|t| t.as_ref()) {
            Some(rd) => content::blit_exact(hdc, &cell, rd),
            None => {
                FillRect(hdc, &cell, sheet_brush);
                wanted.push(page);
            }
        }
        let label = crate::win::wide(&format!("{}", page + 1));
        let mut lr = RECT {
            left: cell.left,
            top: cell.bottom,
            right: cell.right,
            bottom: cell.bottom + crate::win::dpi_scale(hwnd, 18),
        };
        let _ = DrawTextW(hdc, &mut label.clone(), &mut lr, DT_CENTER | DT_SINGLELINE);
    }
    SelectObject(hdc, oldf);
    let _ = DeleteObject(sheet_brush.into());
    let _ = DeleteObject(sel_brush.into());

    for page in wanted {
        request_strip_tile(hwnd, doc, page, width_now, gen);
    }
    true
}

/// Ask a worker for one strip thumbnail.
unsafe fn request_strip_tile(hwnd: HWND, doc: &mut PdfDoc, page: usize, width: i32, gen: u64) {
    if page >= doc.strip_tiles.len() || doc.strip_tiles[page].is_some() || doc.strip_pending[page] {
        return;
    }
    doc.strip_pending[page] = true;
    let session = Arc::clone(&doc.session);
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
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
        let payload: Box<TilePayload> = Box::new((gen, page, width, decoded));
        let raw = Box::into_raw(payload);
        if PostMessageW(
            Some(hwnd),
            super::window::WM_APP_PDFSTRIP,
            WPARAM(0),
            LPARAM(raw as isize),
        )
        .is_err()
        {
            drop(Box::from_raw(raw));
        }
    });
}

/// A click in the strip: jump to that page. Returns whether it was handled.
pub(in crate::preview) unsafe fn strip_click(hwnd: HWND, x: i32, y: i32) -> bool {
    let rc = super::window::strip_rect(hwnd);
    if rc.right <= rc.left || x < rc.left || x >= rc.right || y < rc.top || y >= rc.bottom {
        return false;
    }
    let (pitch, top, pages) = {
        let st = &*state(hwnd);
        let slot = st.pdf_doc.borrow();
        let Some(doc) = slot.as_ref() else {
            return false;
        };
        let pad = crate::win::dpi_scale(hwnd, 8);
        let tw = ((rc.right - rc.left) - 2 * pad).max(8);
        let sz = doc.session.size(0);
        let th = (((f64::from(sz.h) / f64::from(sz.w.max(1.0))) * f64::from(tw)).round() as i32)
            .clamp(8, 1 << 14);
        (strip_pitch(hwnd, th), doc.strip_top, doc.page_count())
    };
    let pad = crate::win::dpi_scale(hwnd, 8);
    match strip_page_at(y - rc.top - pad, pitch, top, pages) {
        Some(page) => {
            scroll_to_page(hwnd, page);
            true
        }
        None => false,
    }
}

/// Move the view by `delta` laid-out pixels. Returns whether anything moved, so a caller at the
/// end of the document can let the key fall through instead of swallowing it.
pub(in crate::preview) unsafe fn scroll_by(hwnd: HWND, delta: i32) -> bool {
    let st = &*state(hwnd);
    let ch = {
        let r = super::window::content_rect(hwnd);
        r.bottom - r.top
    };
    let base = render_width(hwnd);
    let mut slot = st.pdf_doc.borrow_mut();
    let Some(doc) = slot.as_mut() else {
        return false;
    };
    let width = doc.zoomed_width(base);
    let gap = crate::win::dpi_scale(hwnd, PAGE_GAP);
    let l = doc.layout_at(width, gap);
    let want = doc
        .scroll
        .saturating_add(delta)
        .clamp(0, max_scroll(l.total, ch));
    if want == doc.scroll {
        return false;
    }
    doc.scroll = want;
    let page = page_at(&l, want, ch) as u32;
    drop(slot);
    // Keep the caption and every `st.pdf_page` consumer in step with what is on screen.
    if st.pdf_page.get() != page {
        st.pdf_page.set(page);
    }
    let _ = InvalidateRect(Some(hwnd), None, false);
    true
}

/// Put the top of `page` at the top of the viewport. Used by the toolbar's pager buttons and
/// by PgUp/PgDn, which mean "the next page", not "a viewport of pixels".
pub(in crate::preview) unsafe fn scroll_to_page(hwnd: HWND, page: usize) -> bool {
    let st = &*state(hwnd);
    let ch = {
        let r = super::window::content_rect(hwnd);
        r.bottom - r.top
    };
    let base = render_width(hwnd);
    let mut slot = st.pdf_doc.borrow_mut();
    let Some(doc) = slot.as_mut() else {
        return false;
    };
    let width = doc.zoomed_width(base);
    let gap = crate::win::dpi_scale(hwnd, PAGE_GAP);
    let l = doc.layout_at(width, gap);
    if l.tops.is_empty() {
        return false;
    }
    let page = page.min(l.tops.len() - 1);
    let want = l.tops[page].clamp(0, max_scroll(l.total, ch));
    doc.scroll = want;
    drop(slot);
    st.pdf_page.set(page as u32);
    let _ = InvalidateRect(Some(hwnd), None, false);
    true
}

/// Ctrl+wheel: magnify, and RE-RENDER at the new size rather than stretching the old bitmap.
/// Returns whether anything changed, so the caller can fall through when already at a limit.
pub(in crate::preview) unsafe fn zoom_by(hwnd: HWND, notches: f64) -> bool {
    let st = &*state(hwnd);
    let mut slot = st.pdf_doc.borrow_mut();
    let Some(doc) = slot.as_mut() else {
        return false;
    };
    // Geometric, so each notch feels the same at 1x and at 3x. Linear steps crawl when zoomed
    // in and jump when zoomed out.
    let want = doc.zoom * 1.25_f64.powf(notches);
    if !doc.set_zoom(want) {
        return false;
    }
    drop(slot);
    let _ = InvalidateRect(Some(hwnd), None, false);
    true
}

/// Shift+wheel: slide a zoomed page sideways. A page that still fits cannot pan, so this is
/// inert at 1x and the wheel keeps its ordinary meaning there.
pub(in crate::preview) unsafe fn pan_by(hwnd: HWND, delta: i32) -> bool {
    let st = &*state(hwnd);
    let cw = {
        let r = super::window::content_rect(hwnd);
        r.right - r.left
    };
    let base = render_width(hwnd);
    let mut slot = st.pdf_doc.borrow_mut();
    let Some(doc) = slot.as_mut() else {
        return false;
    };
    let width = doc.zoomed_width(base);
    let want = doc
        .pan_x
        .saturating_add(delta)
        .clamp(0, (width - cw).max(0));
    if want == doc.pan_x {
        return false;
    }
    doc.pan_x = want;
    drop(slot);
    let _ = InvalidateRect(Some(hwnd), None, false);
    true
}

/// Is the continuous view live for the current file?
pub(in crate::preview) unsafe fn active(hwnd: HWND) -> bool {
    let st = &*state(hwnd);
    st.kind.get() == ContentKind::Image && st.pdf_doc.borrow().is_some()
}

/// One viewport of scrolling, the amount PgUp/PgDn move when a page is taller than the window.
pub(in crate::preview) unsafe fn viewport_step(hwnd: HWND) -> i32 {
    let r = super::window::content_rect(hwnd);
    ((r.bottom - r.top) - crate::win::dpi_scale(hwnd, 24)).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn letter(n: usize) -> Vec<PageSize> {
        (0..n)
            .map(|_| PageSize {
                w: 816.0,
                h: 1056.0,
            })
            .collect()
    }

    #[test]
    fn pages_stack_with_a_gap_between_them_and_none_after_the_last() {
        let l = layout(&letter(3), 800, 10);
        // 816x1056 at 800 wide is 1035 tall.
        assert_eq!(l.heights, vec![1035, 1035, 1035]);
        assert_eq!(l.tops, vec![0, 1045, 2090]);
        assert_eq!(
            l.total, 3125,
            "the document ends at the last page's bottom, not a gap past it"
        );
    }

    /// Mixed page sizes are the case a "every page is the same height" shortcut gets wrong, and
    /// a PDF is perfectly entitled to mix them (a landscape table in a portrait report).
    #[test]
    fn a_document_that_mixes_page_sizes_lays_out_each_page_on_its_own_aspect() {
        let sizes = vec![
            PageSize {
                w: 816.0,
                h: 1056.0,
            }, // portrait letter
            PageSize {
                w: 1056.0,
                h: 816.0,
            }, // the same sheet, landscape
        ];
        let l = layout(&sizes, 800, 10);
        assert_eq!(l.heights, vec![1035, 618]);
        assert_eq!(l.tops, vec![0, 1045]);
    }

    #[test]
    fn a_document_shorter_than_the_window_cannot_scroll() {
        let l = layout(&letter(1), 400, 10);
        assert_eq!(max_scroll(l.total, l.total + 200), 0);
        assert_eq!(max_scroll(l.total, 100), l.total - 100);
    }

    #[test]
    fn the_visible_range_covers_every_page_the_viewport_touches() {
        let l = layout(&letter(5), 800, 10); // pages 1035 tall, tops 0/1045/2090/3135/4180
        assert_eq!(visible_range(&l, 0, 500), (0, 1));
        // Straddling the boundary must include BOTH pages, or the second one paints as a hole.
        assert_eq!(visible_range(&l, 1000, 200), (0, 2));
        assert_eq!(visible_range(&l, 2100, 900), (2, 3));
        assert_eq!(
            visible_range(&l, 0, 10_000),
            (0, 5),
            "a window taller than the document shows all of it"
        );
    }

    /// A scroll position outside the document must still name a page. Returning an empty range
    /// would paint nothing, which is indistinguishable from a broken document.
    #[test]
    fn a_scroll_past_the_end_still_shows_the_last_page() {
        let l = layout(&letter(3), 800, 10);
        assert_eq!(visible_range(&l, 99_999, 400), (2, 3));
    }

    /// The caption must follow what FILLS the window, not what happens to touch its top edge.
    #[test]
    fn the_named_page_is_the_one_covering_most_of_the_view() {
        let l = layout(&letter(3), 800, 10);
        assert_eq!(page_at(&l, 0, 600), 0);
        // The case the rule exists for: the TOP EDGE is still inside page one, with five
        // pixels of it left, while page two fills the rest of the window. Naming page one
        // here (what "the page at the top edge" would do) is the bug.
        assert_eq!(
            page_at(&l, 1030, 600),
            1,
            "five pixels of page one against 585 of page two is page two"
        );
        // And the other way: a short window entirely inside page one, close to its bottom.
        assert_eq!(
            page_at(&l, 1000, 40),
            0,
            "wholly inside page one, however near the end of it"
        );
        assert_eq!(page_at(&l, 2090, 400), 2);
    }

    /// Degenerate inputs reach this code through a corrupt or unusual document, and a panic in
    /// a paint handler takes the window down.
    #[test]
    fn layout_survives_absurd_page_sizes() {
        let sizes = vec![
            PageSize { w: 0.0, h: 0.0 },
            PageSize { w: 1.0, h: 1e9 },
            PageSize { w: 1e9, h: 1.0 },
        ];
        let l = layout(&sizes, 800, 10);
        assert_eq!(l.heights.len(), 3);
        assert!(l.heights.iter().all(|&h| h >= 1), "no page is zero-height");
        assert!(l.total > 0);
        let _ = visible_range(&l, 0, 500);
        let _ = page_at(&l, 0, 500);
    }

    fn doc_zoom(z: f64) -> f64 {
        z.clamp(super::ZOOM_MIN, super::ZOOM_MAX)
    }

    /// Zoom multiplies the width pages are RENDERED at, which is the whole mechanism: the tile
    /// is rasterized bigger rather than a fit-width bitmap being stretched. If this ever
    /// returned the base width, zoom would silently go back to being a blur.
    #[test]
    fn zoom_scales_the_width_pages_are_rasterized_at() {
        let w = |z: f64| ((1000.0_f64 * doc_zoom(z)).round() as i32).clamp(16, 1 << 15);
        assert_eq!(w(1.0), 1000);
        assert_eq!(w(1.5), 1500);
        assert_eq!(w(4.0), 4000);
    }

    /// The floor is fit-width. Below it the page is smaller than the pane and there is nothing
    /// to see that fit-width does not already show, so zooming out past it is empty margin.
    /// The ceiling keeps a rasterized page inside sane memory.
    #[test]
    fn zoom_is_clamped_at_both_ends() {
        assert_eq!(doc_zoom(0.1), super::ZOOM_MIN);
        assert_eq!(doc_zoom(0.99), super::ZOOM_MIN);
        assert_eq!(doc_zoom(99.0), super::ZOOM_MAX);
        assert_eq!(doc_zoom(2.0), 2.0);
    }

    /// Clicking a thumbnail must land on the page you pointed at. An off-by-one here sends the
    /// reader somewhere else, which is glaring in use and invisible in a screenshot.
    #[test]
    fn a_click_in_the_strip_picks_the_thumbnail_under_it() {
        // pitch 100, showing from page 0, 4 pages.
        assert_eq!(super::strip_page_at(0, 100, 0, 4), Some(0));
        assert_eq!(super::strip_page_at(99, 100, 0, 4), Some(0));
        assert_eq!(super::strip_page_at(100, 100, 0, 4), Some(1));
        assert_eq!(super::strip_page_at(350, 100, 0, 4), Some(3));
    }

    /// A scrolled strip is offset by its first visible page, and the empty tail below the last
    /// thumbnail is not a page at all.
    #[test]
    fn a_scrolled_strip_offsets_and_the_tail_is_not_a_page() {
        assert_eq!(super::strip_page_at(0, 100, 7, 20), Some(7));
        assert_eq!(super::strip_page_at(250, 100, 7, 20), Some(9));
        assert_eq!(
            super::strip_page_at(400, 100, 4, 6),
            None,
            "past the last page"
        );
        assert_eq!(super::strip_page_at(-5, 100, 0, 4), None, "above the first");
    }

    /// Degenerate inputs reach this from a resize mid-paint; a panic in a click handler takes
    /// the window down.
    #[test]
    fn strip_hit_testing_survives_nonsense() {
        assert_eq!(super::strip_page_at(10, 0, 0, 4), None);
        assert_eq!(super::strip_page_at(10, -3, 0, 4), None);
        assert_eq!(super::strip_page_at(10, 100, 0, 0), None);
    }

    #[test]
    fn an_empty_document_never_panics() {
        let l = layout(&[], 800, 10);
        assert_eq!(l.total, 0);
        assert_eq!(visible_range(&l, 0, 500), (0, 0));
        assert_eq!(page_at(&l, 0, 500), 0);
    }
}
