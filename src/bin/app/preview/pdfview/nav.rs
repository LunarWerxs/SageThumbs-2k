//! Scrolling, zooming and panning the live document.

use super::*;

/// Borrow the live document of `hwnd` and hand `f` the document plus the width pages are
/// rendered at, releasing the borrow before the caller continues.
///
/// `None` when the window holds no document, or when `f` itself gives up: the "nothing moved"
/// every scroll and pan entry point reports as `false`.
pub(super) unsafe fn with_live_doc<T>(
    hwnd: HWND,
    st: &ViewerState,
    f: impl FnOnce(&mut PdfDoc, i32) -> Option<T>,
) -> Option<T> {
    let base = render_width(hwnd);
    let mut slot = st.pdf_doc.borrow_mut();
    let doc = slot.as_mut()?;
    let width = doc.zoomed_width(base);
    f(doc, width)
}

/// [`with_live_doc`] for the vertical scroll paths, which also need the viewport height and the
/// document's layout. The viewport is measured before the borrow, the order every one of them
/// already used.
pub(super) unsafe fn with_scroll_doc<T>(
    hwnd: HWND,
    st: &ViewerState,
    f: impl FnOnce(&mut PdfDoc, i32, &Layout) -> Option<T>,
) -> Option<T> {
    let ch = {
        let r = super::super::window::content_rect(hwnd);
        r.bottom - r.top
    };
    with_live_doc(hwnd, st, |doc, width| {
        let gap = crate::win::dpi_scale(hwnd, PAGE_GAP);
        let l = doc.layout_at(width, gap);
        f(doc, ch, &l)
    })
}

/// Move the view by `delta` laid-out pixels. Returns whether anything moved, so a caller at the
/// end of the document can let the key fall through instead of swallowing it.
pub(in crate::preview) unsafe fn scroll_by(hwnd: HWND, delta: i32) -> bool {
    let st = &*state(hwnd);
    let Some(page) = with_scroll_doc(hwnd, st, |doc, ch, l| {
        let want = doc
            .scroll
            .saturating_add(delta)
            .clamp(0, max_scroll(l.total, ch));
        if want == doc.scroll {
            return None;
        }
        doc.scroll = want;
        Some(page_at(l, want, ch) as u32)
    }) else {
        return false;
    };
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
    let Some(page) = with_scroll_doc(hwnd, st, |doc, ch, l| {
        if l.tops.is_empty() {
            return None;
        }
        let page = page.min(l.tops.len() - 1);
        let want = l.tops[page].clamp(0, max_scroll(l.total, ch));
        doc.scroll = want;
        Some(page)
    }) else {
        return false;
    };
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
        let r = super::super::window::content_rect(hwnd);
        r.right - r.left
    };
    let moved = with_live_doc(hwnd, st, |doc, width| {
        let want = doc
            .pan_x
            .saturating_add(delta)
            .clamp(0, (width - cw).max(0));
        if want == doc.pan_x {
            return None;
        }
        doc.pan_x = want;
        Some(())
    });
    if moved.is_none() {
        return false;
    }
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
    let r = super::super::window::content_rect(hwnd);
    ((r.bottom - r.top) - crate::win::dpi_scale(hwnd, 24)).max(1)
}
