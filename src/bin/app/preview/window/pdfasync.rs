//! Async worker-post handlers for the Markdown remote-image fetch, the classify/read load
//! step, and the continuous PDF view (session, tile, strip-tile, per-page text, page count).
//!
//! Parent-hub split (2026-09-08): moved out of `window.rs` (pure move, `pub(super)` only on
//! what `window.rs`'s dispatch still calls by name). `log_ui_stage_stall` /
//! `log_abandoned_worker` stayed in the hub: both are shared by handlers that did NOT move
//! (`on_render`, `on_destroy`) as well as these, so splitting them out here would just add a
//! re-export for no reason.

use super::*;

/// `WM_APP_MDIMG`: a fetched remote Markdown image landed, install it (stale gen / wrong
/// kind → drop).
pub(super) unsafe fn on_app_mdimg(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let boxed = Box::from_raw(lparam.0 as *mut (u64, String, Option<content::DecodedRgba>));
    let (gen, src, dec) = *boxed;
    let st = &*state(hwnd);
    if gen == st.decode_gen.get() && st.kind.get() == ContentKind::Markdown {
        let slot = match dec.and_then(|d| {
            content::make_dib(d.w, d.h, &d.rgba, crate::dark::SURFACE().0)
                .map(|hbmp| content::RenderData::opaque(hbmp, d.w, d.h))
        }) {
            Some(rd) => crate::preview::markdown::ImgSlot::Ready(rd),
            None => crate::preview::markdown::ImgSlot::Failed,
        };
        st.md_imgs.borrow_mut().insert(src, slot);
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
    LRESULT(0)
}

/// `WM_APP_LOAD_RESOLVED`: the async classify/read step landed (2026-09-05 audit, F10). A
/// stale generation (the user already switched files while this was in flight) is dropped
/// exactly like `on_render` drops a stale decode, the boxed payload is still reclaimed either
/// way so it never leaks.
pub(super) unsafe fn on_app_load_resolved(hwnd: HWND, lparam: LPARAM) {
    let boxed = Box::from_raw(lparam.0 as *mut (u64, Resolved));
    let (gen, resolved) = *boxed;
    let st = &*state(hwnd);
    if !is_load_current(gen, st.decode_gen.get()) {
        return; // stale: the user already switched files
    }
    // Clone the path BEFORE `apply_resolved`, never read `st` after it: `apply_resolved` can
    // reach `try_load_web` -> `create_web`, which PUMPS the message loop while WebView2 creates,
    // and a close arriving during that pump destroys `hwnd` synchronously (`request_close`),
    // freeing the boxed `ViewerState` this `st` points at.
    let path = st.path.borrow().clone().unwrap_or_default();
    // A `Dispatch` result for an HTML/`.url` path can reach that same pump; timing it as an
    // "apply" stall would log a false stall on every web load (hundreds of ms is normal
    // WebView2 startup, not stalled work), so it is excluded rather than timed.
    let time_apply = {
        #[cfg(feature = "html-preview")]
        {
            !(matches!(resolved, Resolved::Dispatch(_)) && is_web_route_ext(&ext_of(&path)))
        }
        #[cfg(not(feature = "html-preview"))]
        {
            true
        }
    };
    let stage_start = std::time::Instant::now();
    apply_resolved(hwnd, st, resolved);
    // `st`/`hwnd` may be dangling/destroyed now (see above), re-validate before touching either.
    if time_apply && IsWindow(Some(hwnd)).as_bool() {
        log_ui_stage_stall("apply", stage_start.elapsed(), gen, &path);
    }
}

/// `WM_APP_PDFDOC`: the opened PDF session for the continuous view landed.
pub(super) unsafe fn on_app_pdfdoc(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let boxed = Box::from_raw(lparam.0 as *mut (u64, sagethumbs2k_core::pdf::PdfSession));
    let (gen, session) = *boxed;
    let st = &*state(hwnd);
    // A session for a file we have already navigated away from is dropped here,
    // which also ends its worker thread and releases the document.
    if gen == st.decode_gen.get() && st.kind.get() == ContentKind::Image {
        let doc = crate::preview::pdfview::PdfDoc::new(session, gen);
        // Open the continuous view at the page the pager is already on, so a
        // `--pdf-page N` shot or a restored position is not silently reset to one.
        *st.pdf_doc.borrow_mut() = Some(doc);
        let page = st.pdf_page.get() as usize;
        if page > 0 {
            crate::preview::pdfview::scroll_to_page(hwnd, page);
        }
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
    LRESULT(0)
}

/// `WM_APP_PDFTILE`: one rasterized PDF page for the continuous view.
pub(super) unsafe fn on_app_pdftile(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let boxed = Box::from_raw(lparam.0 as *mut crate::preview::pdfview::TilePayload);
    let (gen, page, width, decoded) = *boxed;
    let st = &*state(hwnd);
    let bg = letterbox_bg(st);
    let mut slot = st.pdf_doc.borrow_mut();
    // Matched against the DOCUMENT's own generation, not `decode_gen`. The two are
    // equal today (the only other bump, in `goto_pdf_page`, is unreachable while the
    // continuous view is live), but a tile belongs to a document, and asking the
    // question that way means a future generation bump somewhere else cannot
    // silently stop every page from ever arriving.
    if let Some(doc) = slot.as_mut() {
        if gen == doc.gen {
            match decoded.and_then(|(w, h, rgba)| content::make_render(w, h, &rgba, bg)) {
                Some(rd) => doc.put_tile(page, width, rd),
                // The page did not rasterize. Clearing the flag is the whole point
                // of posting on failure: without it the sheet stays blank and no
                // later paint ever asks for it again.
                None => doc.clear_pending(page),
            }
            drop(slot);
            let cr = content_rect(hwnd);
            let _ = InvalidateRect(Some(hwnd), Some(&cr), false);
        }
    }
    LRESULT(0)
}

/// `WM_APP_PDFSTRIP`: one rendered page thumbnail for the side strip.
pub(super) unsafe fn on_app_pdfstrip(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let boxed = Box::from_raw(lparam.0 as *mut crate::preview::pdfview::TilePayload);
    let (gen, page, width, decoded) = *boxed;
    let st = &*state(hwnd);
    let bg = letterbox_bg(st);
    let mut slot = st.pdf_doc.borrow_mut();
    if let Some(doc) = slot.as_mut() {
        if gen == doc.gen {
            match decoded.and_then(|(w, h, rgba)| content::make_render(w, h, &rgba, bg)) {
                Some(rd) => doc.put_strip_tile(page, width, rd),
                None => doc.clear_strip_pending(page),
            }
            drop(slot);
            let sr = strip_rect(hwnd);
            let _ = InvalidateRect(Some(hwnd), Some(&sr), false);
        }
    }
    LRESULT(0)
}

/// `WM_APP_PDFTEXT`: one page's recognized text for the Ctrl+F index.
pub(super) unsafe fn on_app_pdftext(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let boxed = Box::from_raw(lparam.0 as *mut crate::preview::pdfview::TextPayload);
    let (gen, page, text) = *boxed;
    let st = &*state(hwnd);
    // Matched against the DOCUMENT's generation for the same reason WM_APP_PDFTILE is:
    // this text belongs to a document, and a page of the previous file's text landing
    // in this one's index would send Ctrl+F to a page that says something else.
    let grew = {
        let mut slot = st.pdf_doc.borrow_mut();
        match slot.as_mut() {
            Some(doc) if gen == doc.gen => doc.put_page_text(page, text.as_deref()),
            _ => false,
        }
    };
    if grew {
        // Re-run an open search over the page that just arrived. Deliberately does not
        // move the view unless the search had nothing at all before: pages land every
        // ~130 ms, and a view that jumped on each one would be unusable to read.
        crate::preview::find::on_pdf_index_progress(hwnd);
    }
    LRESULT(0)
}

/// `WM_APP_PDFINFO`: the PDF page count landed.
pub(super) unsafe fn on_app_pdfinfo(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let boxed = Box::from_raw(lparam.0 as *mut (u64, u32));
    let (gen, count) = *boxed;
    let st = &*state(hwnd);
    if gen == st.decode_gen.get() {
        // Cap the UNTRUSTED count (a crafted PDF can report > i32::MAX pages, which
        // would wrap the nav math negative and panic a clamp — panic=abort).
        st.pdf_pages.set(count.min(1_000_000));
        let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        r.bottom = cap;
        let _ = InvalidateRect(Some(hwnd), Some(&r), false); // repaint the page indicator + pager
    }
    LRESULT(0)
}
