//! The page strip down the side: its width, its thumbnails and a click on one.

use super::*;

/// Width of the page-thumbnail strip, in unscaled px. Wide enough that a page is recognisable
/// (you are looking for "the one with the table on it"), narrow enough not to be the point.
pub(super) const STRIP_W: i32 = 116;

/// Below this client width the strip is not shown at all. A narrow popup has no room to give up
/// a sixth of itself, and a reader would rather have the page.
pub(super) const STRIP_MIN_CLIENT: i32 = 520;

/// How wide the thumbnail strip is for THIS window, or 0 when it should not appear.
///
/// Consulted by `content_rect`, so the strip takes its slice once and paint, scroll clamping and
/// every hit test inherit the narrower content area automatically - the same integration the
/// find bar already uses. Returning 0 keeps every non-PDF window byte-identical.
pub(in crate::preview) unsafe fn strip_width(hwnd: HWND) -> i32 {
    let st = super::super::window::state(hwnd);
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

/// Vertical pitch of one strip entry: the thumbnail plus its caption and gap.
pub(super) fn strip_pitch(hwnd: HWND, thumb_h: i32) -> i32 {
    thumb_h + crate::win::dpi_scale(hwnd, 22)
}

/// Strip thumbnail height for a `tw`-wide cell, from page one's aspect ratio (every page keeps
/// its own aspect, so this follows the FIRST page rather than assuming a uniform document; a
/// mixed-orientation file still lays out evenly enough to click). Shared by `paint_strip` and
/// `strip_click`, which previously computed this formula separately and could disagree.
pub(super) fn strip_thumb_height(doc: &PdfDoc, tw: i32) -> i32 {
    let sz = doc.session.size(0);
    let th = ((f64::from(sz.h) / f64::from(sz.w.max(1.0))) * f64::from(tw)).round() as i32;
    th.clamp(8, 1 << 14)
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
    let rc = super::super::window::strip_rect(hwnd);
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
    let th = strip_thumb_height(doc, tw);
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

    doc.evict_strip_outside(doc.strip_top, (doc.strip_top + visible + 1).min(pages));
    for page in wanted {
        request_strip_tile(hwnd, doc, page, width_now, gen);
    }
    true
}

/// Ask a worker for one strip thumbnail.
pub(super) unsafe fn request_strip_tile(
    hwnd: HWND,
    doc: &mut PdfDoc,
    page: usize,
    width: i32,
    gen: u64,
) {
    if page >= doc.strip_tiles.len() || doc.strip_tiles[page].is_some() || doc.strip_pending[page] {
        return;
    }
    doc.strip_pending[page] = true;
    spawn_render(
        hwnd,
        Arc::clone(&doc.session),
        Arc::clone(&doc.cancel),
        page,
        width,
        gen,
        super::super::window::WM_APP_PDFSTRIP,
    );
}

/// A click in the strip: jump to that page. Returns whether it was handled.
pub(in crate::preview) unsafe fn strip_click(hwnd: HWND, x: i32, y: i32) -> bool {
    let rc = super::super::window::strip_rect(hwnd);
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
        let th = strip_thumb_height(doc, tw);
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
