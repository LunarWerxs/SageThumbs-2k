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
            // Captured on the UI thread, at the keypress — NOT read from `st` again inside the
            // worker, which runs on its own thread and must never touch `ViewerState`'s
            // `Cell`/`RefCell` fields without the UI thread's synchronization. `copy_shown_image`
            // re-checks this against the live generation right before the clipboard write, so a
            // fast file-switch during the decode drops the copy instead of putting the file just
            // left on the clipboard under the still-held Ctrl+C.
            let gen = st.decode_gen.get();
            std::thread::spawn(move || {
                use windows::Win32::System::Com::{
                    CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED,
                };
                let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
                if !copy_shown_image(&p, pdf_page, anim_frame, gen) {
                    // The viewer has no toast/status surface, so a failed copy is otherwise
                    // indistinguishable from the keypress not registering — say so in the log.
                    // (Also reached when the copy was dropped as stale, not just a real decode
                    // failure — both cases end in "nothing landed on the clipboard".)
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

/// Build the RGBA pixels the viewer is currently SHOWING: the given PDF page / animation frame
/// when navigated. `None` for anything else (the caller decides what to fall back to) — this
/// deliberately does NOT cover the "neither navigated" case, since that one differs between the
/// two callers (clipboard falls back to the file's own full-fidelity decode; a save has nothing
/// sensible to fall back to when the toolbar button is only shown while one of the two applies).
fn navigated_shown_image_rgba(
    path: &str,
    pdf_page: Option<u32>,
    anim_frame: Option<usize>,
) -> Option<(i32, i32, Vec<u8>)> {
    if let Some(page) = pdf_page {
        let png = sagethumbs2k_core::decode::read_capped(path)
            .ok()
            .and_then(|b| sagethumbs2k_core::pdf::render_page_counted(&b, page, 1600));
        let img = png.and_then(|(png, _)| image::load_from_memory(&png).ok())?;
        let rgba = img.to_rgba8();
        return Some((rgba.width() as i32, rgba.height() as i32, rgba.into_raw()));
    }
    if let Some(frame) = anim_frame {
        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        let frames = sagethumbs2k_core::decode::read_preview_capped(path)
            .ok()
            .and_then(|b| crate::preview::anim::decode_animation(&b, &ext))?;
        let (d, _) = frames.get(frame)?;
        return Some((d.w, d.h, d.rgba.clone()));
    }
    None
}

/// Copy the image the viewer is SHOWING: the given PDF page / animation frame when navigated,
/// else the file's full-fidelity decode (the context menu's Copy verb path). Falls back to the
/// static decode when frame extraction fails, so Ctrl+C still yields SOMETHING.
///
/// `gen` is the `decode_gen` captured at the moment this copy was requested — checked against
/// the live generation right before EVERY clipboard write, never earlier: the decode above can
/// take a while (a RAW/HEIC decode, a PDF page render), and the user may have already switched
/// files by the time it finishes. Landing the wrong image on the clipboard silently is worse
/// than a dropped Ctrl+C, so a stale generation drops the copy instead.
pub(in crate::preview) fn copy_shown_image(
    path: &str,
    pdf_page: Option<u32>,
    anim_frame: Option<usize>,
    gen: u64,
) -> bool {
    if let Some((w, h, rgba)) = navigated_shown_image_rgba(path, pdf_page, anim_frame) {
        return content::generation_current(gen)
            && sagethumbs2k_core::copy_rgba_to_clipboard(w, h, &rgba).is_ok();
    }
    if pdf_page.is_some() {
        return false; // page N failed to render — copying page 1 instead would be a silent lie
    }
    // Either not navigated at all, or an animation frame that failed to extract — either way
    // the file's own full-fidelity decode (first frame, for an animation) beats nothing.
    content::generation_current(gen) && sagethumbs2k_core::copy_to_clipboard(path).is_ok()
}

/// Save the image the viewer is currently SHOWING (the navigated-to PDF page / animation frame)
/// as a PNG at `dest`. `false` if nothing could be decoded, or the write itself failed. Unlike
/// [`copy_shown_image`] there is no "not navigated" fallback: the toolbar's `SavePage` button
/// only shows while one of the two applies (see `window::btn_visible`).
pub(in crate::preview) fn save_shown_image(
    path: &str,
    pdf_page: Option<u32>,
    anim_frame: Option<usize>,
    dest: &str,
) -> bool {
    let Some((w, h, rgba)) = navigated_shown_image_rgba(path, pdf_page, anim_frame) else {
        return false;
    };
    let Some(img) = image::RgbaImage::from_raw(w as u32, h as u32, rgba) else {
        return false;
    };
    img.save(dest).is_ok()
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
    // When the document is scrolling continuously, "next page" is a place to scroll TO, not a
    // different bitmap to render. Routing the toolbar's pager through the same scroll keeps the
    // two in step: clicking the arrow and dragging the wheel end up at the same position, and
    // the caption is derived from the scroll either way rather than from a second counter that
    // could disagree with it.
    if crate::preview::pdfview::active(hwnd) {
        let cur = st.pdf_page.get() as i64;
        let want = (cur + i64::from(delta)).clamp(0, i64::from(pages) - 1) as usize;
        crate::preview::pdfview::scroll_to_page(hwnd, want);
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
    // The PDF page-thumbnail strip takes a slice off the RIGHT, on the same principle as the
    // find bar above: subtract it HERE and paint, scroll clamping, hit-testing and the child
    // window rects all inherit the narrower content area without knowing the strip exists.
    // Zero for everything that is not a multi-page PDF, so every other window is unchanged.
    let strip = crate::preview::pdfview::strip_width(hwnd);
    RECT {
        left: 0,
        top: cap + find_h,
        right: (r.right - strip).max(0),
        bottom: r.bottom,
    }
}

/// The strip's own rect: the slice `content_rect` gave up, or an empty rect when there is none.
pub(in crate::preview) unsafe fn strip_rect(hwnd: HWND) -> RECT {
    let strip = crate::preview::pdfview::strip_width(hwnd);
    let content = content_rect(hwnd);
    let mut r = RECT::default();
    let _ = GetClientRect(hwnd, &mut r);
    RECT {
        left: r.right - strip,
        top: content.top,
        right: r.right,
        bottom: content.bottom,
    }
}
