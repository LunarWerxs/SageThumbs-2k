//! Clicking the content area, and copying what is shown to the clipboard.
//!
//! Split out of `window.rs` 2026-07-31 (pure move).

use super::*;

/// A plain click (nothing dragged) in the content area: an outline entry jumps to its heading;
/// a Markdown link opens it.
pub(in crate::preview) unsafe fn click_content(hwnd: HWND, x: i32, y: i32) {
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
pub(in crate::preview) unsafe fn copy_content(hwnd: HWND, raw: bool) {
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
pub(in crate::preview) fn copy_shown_image(
    path: &str,
    pdf_page: Option<u32>,
    anim_frame: Option<usize>,
) -> bool {
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
            .and_then(|b| crate::preview::anim::decode_animation(&b, &ext));
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
pub(in crate::preview) fn is_pdf(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false)
}

/// Navigate a multi-page PDF by `delta` pages. Keeps the current page visible until the new one
/// decodes (no Loading flash); the `decode_gen` bump fences a stale in-flight page decode.
pub(in crate::preview) unsafe fn goto_pdf_page(hwnd: HWND, delta: i32) {
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
    // Same fence as a file switch: paging a PDF abandons the page still being rendered.
    content::begin_generation(gen);
    if let Some(p) = st.path.borrow().as_ref().cloned() {
        content::spawn_decode_pdf(hwnd, p, new, gen);
    }
    let _ = InvalidateRect(Some(hwnd), None, false); // update the "N / M" caption immediately
}

/// Natural pixel dims of the current image content — the first animation frame when animated
/// (all frames share a size), else the static render. `None` while still loading.
pub(in crate::preview) fn image_dims(st: &ViewerState) -> Option<(i32, i32)> {
    if let Some(rd) = st.frames.borrow().first() {
        return Some((rd.iw, rd.ih));
    }
    st.render.borrow().as_ref().map(|rd| (rd.iw, rd.ih))
}

pub(in crate::preview) unsafe fn content_rect(hwnd: HWND) -> RECT {
    let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
    let mut r = RECT::default();
    let _ = GetClientRect(hwnd, &mut r);
    // The find bar, when open, eats the top of the content area. Taking it off HERE is the entire
    // integration: paint, scroll clamping, selection hit-testing and the video/webview child rects
    // all derive from this one rect, so none of them need to know the bar exists.
    let find_h = if (*state(hwnd)).find.borrow().open {
        crate::win::dpi_scale(hwnd, crate::preview::find::FIND_H)
    } else {
        0
    };
    RECT {
        left: 0,
        top: cap + find_h,
        right: r.right,
        bottom: r.bottom,
    }
}
