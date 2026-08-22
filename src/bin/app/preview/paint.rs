//! All viewer painting: content arms, text/code, toolbar glyphs.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreatePen, DeleteDC,
    DeleteObject, DrawTextW, EndPaint, FillRect, GetStockObject, LineTo, MoveToEx, SelectObject,
    SetBkMode, SetDCBrushColor, SetTextColor, DC_BRUSH, DRAW_TEXT_FORMAT, DT_CENTER,
    DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, HBRUSH, HDC, HFONT,
    HGDIOBJ, PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::content::{self};
use super::selection::sel_range;
use super::toolbar::button_rects;
use super::transport::{draw_scrub_strip, scrub_rect, video_rect};
use super::window::{
    clamp_text_scroll, state, text_scrollbar, Btn, ContentKind, ViewerState, BTNS, CAPTION_H, PAD,
};
use super::{highlight, infocard};

// ===== Painting =====

/// `DrawTextW` that no-ops on an EMPTY buffer. windows-rs passes the slice's length as GDI's
/// `cchText`, and a zero-length (dangling) one faults — which `panic=abort` turns into the whole
/// viewer dying. Reached for real: a Markdown heading with no text (`# ` alone, or one holding
/// only an image) yields an empty outline label. Use this for any caller-supplied string.
pub(super) unsafe fn draw_text(hdc: HDC, text: &mut [u16], rc: &mut RECT, fmt: DRAW_TEXT_FORMAT) {
    if text.is_empty() {
        return;
    }
    DrawTextW(hdc, text, rc, fmt);
}

/// Fill `rc` with a flat `color` using GDI's per-thread stock DC brush — no
/// `CreateSolidBrush`/`DeleteObject` pair for what is just a flat fill. This viewer paints
/// on every scroll notch and hover, so the 8 sites in this file that used to allocate +
/// delete a brush per call are the hot path, not a one-off. Mirrors
/// `settings_dlg::helpers::fill` (private to that module — this is a same-shaped copy
/// local to this one, not a call to it).
unsafe fn fill(hdc: HDC, rc: &RECT, color: u32) {
    SetDCBrushColor(hdc, COLORREF(color));
    FillRect(hdc, rc, HBRUSH(GetStockObject(DC_BRUSH).0));
}

pub(super) unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    if !hdc.is_invalid() {
        // Double-buffer: render the whole client into a CACHED off-screen bitmap (allocated once
        // per client size, not reallocated on every paint — see `ensure_back_buffer`), then blit
        // it once. Painting straight to the window DC drew the content-bg fill and then the
        // text/lines separately on-screen, which FLASHED on every scroll notch. One BitBlt = no
        // flash. (BeginPaint's DC is clipped to the invalid region, so the blit only touches what
        // changed.)
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let w = (rc.right - rc.left).max(1);
        let h = (rc.bottom - rc.top).max(1);
        let st = &*state(hwnd);
        match ensure_back_buffer(st, hdc, w, h) {
            Some(mem) => {
                paint_into(hwnd, mem);
                let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
            }
            // Allocation failed (e.g. out of GDI handles) — paint directly; correct, just flickers.
            None => paint_into(hwnd, hdc),
        }
    }
    let _ = EndPaint(hwnd, &ps);
}

/// Whether the cached back-buffer bitmap (`cached` = its last-built `(w, h)`, `None` if never
/// built or just freed by a resize) must be (re)allocated to paint a client of `wanted` size.
/// This is the actual defect being fixed: every `WM_PAINT` used to `CreateCompatibleBitmap` a
/// fresh full-client bitmap (tens of MB at 4K) and delete it before returning — on every scroll
/// notch and hover, not just on resize. Split out from the real GDI calls in
/// [`ensure_back_buffer`] so the reuse/recreate decision is testable without a live HDC.
pub(super) fn back_buffer_needs_alloc(cached: Option<(i32, i32)>, wanted: (i32, i32)) -> bool {
    cached != Some(wanted)
}

/// Get (creating or resizing as needed) the cached off-screen DC that [`paint`] double-buffers
/// into. Reused across repaints at the same client size; `WM_SIZE` (via [`free_back_buffer`])
/// frees the stale one so this allocates fresh here on the next paint at the new size. Returns
/// `None` only if the GDI calls themselves fail, in which case the caller paints straight to the
/// window DC.
unsafe fn ensure_back_buffer(st: &ViewerState, hdc: HDC, w: i32, h: i32) -> Option<HDC> {
    let have = !st.back_dc.get().is_invalid() && !st.back_bmp.get().is_invalid();
    let cached = have.then(|| st.back_size.get());
    if !back_buffer_needs_alloc(cached, (w, h)) {
        return Some(st.back_dc.get());
    }
    free_back_buffer(st);
    let mem = CreateCompatibleDC(Some(hdc));
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    if mem.is_invalid() || bmp.is_invalid() {
        if !bmp.is_invalid() {
            let _ = DeleteObject(bmp.into());
        }
        if !mem.is_invalid() {
            let _ = DeleteDC(mem);
        }
        return None;
    }
    let stock = SelectObject(mem, bmp.into()); // the DC's original 1x1 bitmap — restored before free
    st.back_dc.set(mem);
    st.back_bmp.set(bmp);
    st.back_stock.set(stock);
    st.back_size.set((w, h));
    Some(mem)
}

/// Release the cached `WM_PAINT` double-buffer's GDI handles, if any. Called on `WM_SIZE` (the
/// buffer is now the wrong size) and `WM_DESTROY` (final cleanup) — see `ViewerState::back_dc`.
pub(super) unsafe fn free_back_buffer(st: &ViewerState) {
    let mem = st.back_dc.get();
    let bmp = st.back_bmp.get();
    if !mem.is_invalid() && !bmp.is_invalid() {
        // Deselect our bitmap back to the DC's original stock bitmap FIRST — GDI leaks a bitmap
        // silently (DeleteObject becomes a no-op) if it is deleted while still selected into a DC.
        SelectObject(mem, st.back_stock.get());
        let _ = DeleteObject(bmp.into());
    }
    if !mem.is_invalid() {
        let _ = DeleteDC(mem);
    }
    st.back_dc.set(HDC::default());
    st.back_bmp.set(Default::default());
    st.back_stock.set(Default::default());
    st.back_size.set((0, 0));
}

pub(super) unsafe fn paint_into(hwnd: HWND, hdc: HDC) {
    let st = &*state(hwnd);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
    let caption_rc = RECT {
        left: 0,
        top: 0,
        right: rc.right,
        bottom: cap,
    };
    // Same rect `content_rect` computes, kept in step with it: the find bar, when open, takes a
    // strip off the top and everything below lays out inside what is left.
    let content_rc = super::window::content_rect(hwnd);

    let content_bg = crate::dark::SURFACE().0;
    let cap_bg = crate::dark::DARK_BG().0;
    let text = crate::dark::DARK_TEXT().0;
    let subtle = crate::dark::HEADER_TEXT().0;

    // Content.
    match st.kind.get() {
        ContentKind::Image => {
            // Checkerboard behind a transparent image, from the SAME setting the classic menu tile
            // uses, so a white-on-transparent logo doesn't read as an empty pane. `paint_image`
            // ignores it for an opaque bitmap, so this is a no-op for photos. The cell is
            // DPI-scaled and larger than the menu tile's, because this pane is full size.
            let checker = sagethumbs2k_core::settings::preview_checker()
                .then(|| crate::win::dpi_scale(hwnd, 12));
            // The fit view is drawn from a codec-scaled decode. If this zoom (or a resize) has
            // outgrown it, ask for the real pixels; they arrive asynchronously and repaint.
            super::window::ensure_full_for_zoom(hwnd, &content_rc);
            // A multi-page PDF whose session opened scrolls continuously instead of showing
            // one page at a time. Returns false for everything else (and for a PDF whose
            // session never landed), which falls through to the single-image path below,
            // unchanged. The caption is painted by the shared code after this match either
            // way, so the page indicator and buttons behave identically in both modes.
            let scrolled_pdf = super::pdfview::paint(
                hwnd,
                hdc,
                &content_rc,
                content_bg,
                crate::dark::BTN_FACE().0,
            );
            let frames = st.frames.borrow();
            if scrolled_pdf {
                // already drawn
            } else if let Some(rd) = frames.get(st.cur_frame.get()) {
                content::paint_image(
                    hdc,
                    &content_rc,
                    rd,
                    content_bg,
                    st.zoom.get(),
                    st.pan.get(),
                    checker,
                );
            } else if let Some(rd) = st.render.borrow().as_ref() {
                content::paint_image(
                    hdc,
                    &content_rc,
                    rd,
                    content_bg,
                    st.zoom.get(),
                    st.pan.get(),
                    checker,
                );
            } else {
                paint_message(hwnd, hdc, &content_rc, content_bg, subtle, "Loading…");
            }
        }
        ContentKind::InfoCard => {
            if let Some(card) = st.card.borrow().as_ref() {
                infocard::paint(hwnd, hdc, &content_rc, card, content_bg, text, subtle);
            } else {
                paint_message(hwnd, hdc, &content_rc, content_bg, subtle, "");
            }
        }
        ContentKind::Text => {
            if let Some(t) = st.text.borrow().as_ref() {
                let lang = cached_text_lang(hwnd, st.decode_gen.get(), || {
                    let ext = st
                        .path
                        .borrow()
                        .as_deref()
                        .and_then(|p| std::path::Path::new(p).extension().and_then(|e| e.to_str()))
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    let mut lang = highlight::lang_from_ext(&ext);
                    if matches!(lang, highlight::Lang::Plain) {
                        // No usable extension (Makefile, Dockerfile, .bashrc, a bare shebang
                        // script): fall back to the file NAME and the first line's `#!` before
                        // giving up on colouring it. Only reached when the extension told us
                        // nothing, so a real `.txt` is never second-guessed.
                        let name = st
                            .path
                            .borrow()
                            .as_deref()
                            .and_then(|p| {
                                std::path::Path::new(p)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .map(str::to_owned)
                            })
                            .unwrap_or_default();
                        lang = highlight::lang_from_name_or_shebang(&name, t);
                    }
                    lang
                });
                let th = paint_text(
                    hwnd,
                    hdc,
                    &content_rc,
                    t,
                    lang,
                    content_bg,
                    text,
                    st.text_scroll.get(),
                    sel_range(st),
                );
                st.text_h.set(th); // remember for the wheel handler's scroll clamp
                let _ = clamp_text_scroll(hwnd);
            } else {
                paint_message(hwnd, hdc, &content_rc, content_bg, subtle, "");
            }
        }
        ContentKind::Markdown => {
            if let Some(t) = st.text.borrow().as_ref() {
                let cols = super::markdown::MdColors {
                    bg: content_bg,
                    fg: text,
                    muted: subtle,
                    accent: crate::dark::ACCENT_TEXT().0,
                    code_bg: crate::dark::DARK_BG().0,
                    border: crate::dark::BORDER().0,
                    sel: crate::dark::SEL_BG().0,
                };
                // Outline (ToC) sidebar: reserve a left strip and shift the document right when the
                // sidebar is open AND the document actually has headings (flag cached at load).
                // While the open/close slide runs, `toc_anim` carries the mid-tween width.
                let w_full = crate::win::dpi_scale(hwnd, 220);
                let settled = if st.toc_open.get() { w_full } else { 0 };
                let sidebar_w = if st.md_has_headings.get() {
                    st.toc_anim.get().unwrap_or(settled).clamp(0, w_full)
                } else {
                    0
                };
                let show_toc = sidebar_w > 0;
                let md_rc = RECT {
                    left: content_rc.left + sidebar_w,
                    ..content_rc
                };
                let scroll = st.text_scroll.get();
                // The markdown file's folder — local image srcs resolve against it.
                let doc_dir = st
                    .path
                    .borrow()
                    .as_deref()
                    .and_then(|p| std::path::Path::new(p).parent().map(|d| d.to_path_buf()));
                let mut links = st.md_links.borrow_mut();
                let mut toc = st.md_toc.borrow_mut();
                let mut imgs = st.md_imgs.borrow_mut();
                let mut layout = st.md_layout.borrow_mut();
                let mut hits = st.md_hits.borrow_mut();
                let mut sel = super::markdown::MdSel {
                    range: sel_range(st),
                    hits: &mut hits,
                };
                let th = super::markdown::render(
                    hwnd,
                    hdc,
                    &md_rc,
                    t,
                    scroll,
                    &cols,
                    &mut links,
                    &mut toc,
                    &mut imgs,
                    doc_dir.as_deref(),
                    st.decode_gen.get(),
                    st.md_remote_ok.get(),
                    &mut layout,
                    &mut sel,
                );
                drop(hits);
                drop(layout);
                st.text_h.set(th);
                let _ = clamp_text_scroll(hwnd);
                drop(links);
                drop(imgs);
                if show_toc {
                    let side_rc = RECT {
                        right: content_rc.left + sidebar_w,
                        ..content_rc
                    };
                    let mut hits = st.toc_hits.borrow_mut();
                    paint_toc(
                        hwnd,
                        hdc,
                        &side_rc,
                        &toc,
                        scroll,
                        st.toc_sel.get(),
                        &mut hits,
                    );
                } else {
                    st.toc_hits.borrow_mut().clear();
                }
            } else {
                st.md_links.borrow_mut().clear();
                st.toc_hits.borrow_mut().clear();
                st.md_hits.borrow_mut().clear();
                paint_message(hwnd, hdc, &content_rc, content_bg, subtle, "");
            }
        }
        ContentKind::Video => {
            // For VIDEO the render child covers this area, so the black is only visible in the
            // brief pre-first-frame window. For AUDIO there is no picture and the child is hidden
            // (see `video::create`), so this IS the visible surface: paint the track's embedded
            // cover art aspect-fit on black, or plain black when it has none. Either way the
            // transport strip draws in the bottom band.
            let vr = video_rect(hwnd);
            match st.art.borrow().as_ref() {
                Some(art) => content::paint_image(hdc, &vr, art, 0x0000_0000, 1.0, (0, 0), None),
                None => fill(hdc, &vr, 0x0000_0000),
            }
            if let Some(v) = st.video.borrow().as_ref() {
                draw_scrub_strip(hwnd, hdc, &scrub_rect(hwnd), v, text, subtle);
            }
        }
        ContentKind::Loading => {
            paint_message(hwnd, hdc, &content_rc, content_bg, subtle, "Loading…")
        }
        // The WebView2 child window renders over the content area; just fill behind it.
        ContentKind::Html => fill(hdc, &content_rc, content_bg),
    }

    // Scroll-position thumb for the text + markdown panes (they have no OS scrollbar). Drawn on top
    // of the content, only when it's taller than the viewport, so you can see where you are.
    if matches!(st.kind.get(), ContentKind::Text | ContentKind::Markdown) {
        paint_scroll_thumb(hwnd, hdc);
    }

    // Find bar, between the caption and the content (a no-op when closed).
    super::find::paint(hwnd, hdc, text, subtle);

    // Caption strip.
    fill(hdc, &caption_rc, cap_bg);
    // Hairline under the caption.
    let pen = CreatePen(PS_SOLID, 1, COLORREF(crate::dark::BORDER().0));
    let old = SelectObject(hdc, HGDIOBJ(pen.0));
    let _ = MoveToEx(hdc, 0, cap - 1, None);
    let _ = LineTo(hdc, rc.right, cap - 1);
    SelectObject(hdc, old);
    let _ = DeleteObject(HGDIOBJ(pen.0));

    // Title (file name), left-aligned in the caption.
    let buttons = button_rects(hwnd);
    let title_right = buttons
        .iter()
        .map(|(_, r)| r.left)
        .min()
        .unwrap_or(rc.right)
        - crate::win::dpi_scale(hwnd, PAD);
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(text));
    let tf = crate::win::gui_font_for(hwnd);
    let oldf = SelectObject(hdc, tf.into());
    // PDF page indicator "N / M", right-aligned before the buttons — same visibility rule as
    // the pager buttons (multi-page PDF showing as an image; not on the InfoCard fallback).
    let pdf_lbl = if st.kind.get() == ContentKind::Image && st.pdf_pages.get() > 1 {
        Some(format!(
            "{} / {}",
            st.pdf_page.get() + 1,
            st.pdf_pages.get()
        ))
    } else {
        None
    };
    let label_w = if pdf_lbl.is_some() {
        crate::win::dpi_scale(hwnd, 72)
    } else {
        0
    };
    let mut title = st
        .path
        .borrow()
        .as_ref()
        .and_then(|p| {
            std::path::Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
        .encode_utf16()
        .collect::<Vec<u16>>();
    let mut trc = RECT {
        left: crate::win::dpi_scale(hwnd, PAD + 4),
        top: 0,
        right: title_right - label_w,
        bottom: cap,
    };
    draw_text(
        hdc,
        &mut title,
        &mut trc,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
    );
    if let Some(lbl) = pdf_lbl {
        SetTextColor(hdc, COLORREF(subtle));
        let mut w: Vec<u16> = lbl.encode_utf16().collect();
        let mut lr = RECT {
            left: title_right - label_w,
            top: 0,
            right: title_right,
            bottom: cap,
        };
        DrawTextW(
            hdc,
            &mut w,
            &mut lr,
            DT_RIGHT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
    }
    SelectObject(hdc, oldf);

    // Toolbar glyphs. `buttons` is laid out right-to-left, but `st.hot` is a BTNS index (what
    // `hit_button` returns), so resolve each drawn button back to its BTNS index to match — else
    // the highlight mirrors (hover right, light left). One shared Segoe Fluent Icons font for the
    // whole toolbar (crisp ClearType native glyphs, like the screenshot tool).
    let icon = icon_font(hwnd);
    for (b, r) in buttons.iter() {
        let hot = st.hot.get() == BTNS.iter().position(|&bb| bb == *b);
        draw_button(
            hwnd,
            hdc,
            *b,
            r,
            hot,
            st.pinned.get(),
            st.toc_open.get(),
            st.src_view.get(),
            icon,
        );
    }
    let _ = DeleteObject(icon.into());
}

thread_local! {
    /// Per-window cached Text-pane syntax language, keyed by `(hwnd, ViewerState::decode_gen)`.
    /// `decode_gen` bumps on every (re)load, so a file switch in the SAME window (arrow-nav,
    /// daemon reuse) invalidates the cache instead of relighting a NEW file with the PREVIOUS
    /// one's language forever. `lang_from_ext`/`lang_from_name_or_shebang` used to re-run on
    /// every `WM_PAINT` (every scroll notch, every hover redraw) even though the answer cannot
    /// change until the next load — this makes it a once-per-load cost instead.
    ///
    /// Lives here rather than on `ViewerState` itself (its natural home) because that struct is
    /// out of scope for this change; a stray entry for a since-destroyed window costs a couple
    /// of bytes, not a GDI handle, so no destroy hook is needed to keep this bounded in practice.
    static TEXT_LANG_CACHE: std::cell::RefCell<std::collections::HashMap<isize, (u64, highlight::Lang)>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// The Text pane's resolved syntax language for the window's CURRENT load: computed once via
/// `compute` and reused by every repaint until `gen` (the load generation) changes.
fn cached_text_lang(
    hwnd: HWND,
    gen: u64,
    compute: impl FnOnce() -> highlight::Lang,
) -> highlight::Lang {
    TEXT_LANG_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let key = hwnd.0 as isize;
        if let Some((cached_gen, lang)) = c.get(&key) {
            if *cached_gen == gen {
                return *lang;
            }
        }
        let lang = compute();
        c.insert(key, (gen, lang));
        lang
    })
}

/// Centered single-line message (e.g. "Loading…") in `rc`.
pub(super) unsafe fn paint_message(
    hwnd: HWND,
    hdc: HDC,
    rc: &RECT,
    bg: u32,
    color: u32,
    text: &str,
) {
    fill(hdc, rc, bg);
    if text.is_empty() {
        return;
    }
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(color));
    let f = crate::win::gui_font_for(hwnd);
    let old = SelectObject(hdc, f.into());
    let mut w: Vec<u16> = text.encode_utf16().collect();
    let mut r = *rc;
    DrawTextW(
        hdc,
        &mut w,
        &mut r,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
    SelectObject(hdc, old);
}

/// Draw the Markdown outline (table-of-contents) sidebar into `rc`: a "CONTENTS" header + one row
/// per heading (indented by level, deeper levels muted, the current section accent-highlighted),
/// each recorded in `hits` as `(row_rect, target_scroll)` for click-to-jump. Overflowing entries
/// are clipped (no sidebar scroll in v1). Uses the cached UI font (must not be deleted).
#[allow(clippy::too_many_arguments)] // owner-draw helper: many positional draw params by nature
unsafe fn paint_toc(
    hwnd: HWND,
    hdc: HDC,
    rc: &RECT,
    toc: &[super::markdown::TocEntry],
    scroll: i32,
    sel: Option<usize>,
    hits: &mut Vec<(RECT, usize)>,
) {
    hits.clear();
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let bg = crate::dark::DARK_BG().0;
    let fg = crate::dark::DARK_TEXT().0;
    let muted = crate::dark::HEADER_TEXT().0;
    let accent = crate::dark::ACCENT().0;

    fill(hdc, rc, bg);
    // right-edge separator
    let pen = CreatePen(PS_SOLID, 1, COLORREF(crate::dark::BORDER().0));
    let op = SelectObject(hdc, HGDIOBJ(pen.0));
    let _ = MoveToEx(hdc, rc.right - 1, rc.top, None);
    let _ = LineTo(hdc, rc.right - 1, rc.bottom);
    SelectObject(hdc, op);
    let _ = DeleteObject(HGDIOBJ(pen.0));

    let f = crate::win::gui_font_for(hwnd);
    let old = SelectObject(hdc, f.into());
    SetBkMode(hdc, TRANSPARENT);
    let pad = sc(14);
    let row_h = sc(22);
    let mut y = rc.top + pad;

    SetTextColor(hdc, COLORREF(muted));
    let mut hdr: Vec<u16> = "CONTENTS".encode_utf16().collect();
    let mut hr = RECT {
        left: rc.left + pad,
        top: y,
        right: rc.right - pad,
        bottom: y + row_h,
    };
    DrawTextW(
        hdc,
        &mut hdr,
        &mut hr,
        DT_LEFT | DT_SINGLELINE | DT_NOPREFIX,
    );
    y += row_h + sc(4);

    // The "current" section: an explicitly-clicked entry wins (bottom sections can't scroll to
    // the pane top, so the click must still visibly select); otherwise the last heading at or
    // above the scroll position.
    let cur = sel
        .filter(|i| *i < toc.len())
        .or_else(|| toc.iter().rposition(|e| e.target <= scroll + sc(4)));
    for (i, e) in toc.iter().enumerate() {
        if y + row_h > rc.bottom {
            break; // clip overflow (no sidebar scroll in v1)
        }
        let indent = pad + (e.level.saturating_sub(1) as i32) * sc(12);
        let color = if Some(i) == cur {
            accent
        } else if e.level >= 3 {
            muted
        } else {
            fg
        };
        SetTextColor(hdc, COLORREF(color));
        let mut label: Vec<u16> = e.text.encode_utf16().collect();
        let mut r = RECT {
            left: rc.left + indent,
            top: y,
            right: rc.right - sc(8),
            bottom: y + row_h,
        };
        draw_text(
            hdc,
            &mut label,
            &mut r,
            DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS,
        );
        hits.push((
            RECT {
                left: rc.left,
                top: y,
                right: rc.right,
                bottom: y + row_h,
            },
            i,
        ));
        y += row_h;
    }
    SelectObject(hdc, old);
}

/// Paint `text` as monospaced, top-anchored content — the text/code fallback path.
/// Rendered line-per-line with SCROLL CULLING (`highlight::paint_lines`): only the lines inside
/// the viewport are drawn, so it scrolls smoothly no matter how big the file is. Long lines clip
/// at the pane edge (editor-style) rather than word-wrapping. This replaced a plain-text branch
/// that ran Windows' word-wrap layout over the ENTIRE file, twice, on every repaint — which made
/// big files (e.g. a 45 KB `bun.lock`) jerk when scrolled. Returns the total content height.
/// `sel` (normalized raw byte range) paints the mouse/Ctrl+A selection highlight.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn paint_text(
    hwnd: HWND,
    hdc: HDC,
    rc: &RECT,
    text: &str,
    lang: highlight::Lang,
    bg: u32,
    fg: u32,
    scroll: i32,
    sel: Option<(usize, usize)>,
) -> i32 {
    fill(hdc, rc, bg);
    let m = crate::win::dpi_scale(hwnd, 12);
    SetBkMode(hdc, TRANSPARENT);
    let font = mono_font(hwnd);
    let width = (rc.right - rc.left - 2 * m).max(1);
    // Plain text draws every run in `fg` (no keywords), so this covers both plain and code.
    let text_h = highlight::paint_lines(
        hdc,
        text,
        lang,
        rc.left + m,
        rc.top + m - scroll,
        width,
        rc.top,
        rc.bottom,
        font,
        fg,
        sel,
        None,
    );
    let _ = DeleteObject(font.into());
    text_h
}

/// A thin scroll-position indicator on the right edge of `content_rc`. The text/markdown panes
/// have no OS scrollbar, so without this you can't tell where you are or whether a wheel notch
/// registered. Sized/positioned from the same (scroll, text_h, visible) math as `scroll_text`, so
/// it tracks the real position; hidden when everything already fits. The shared geometry helper
/// is also used by the window's mouse hit-testing, making this painted thumb draggable.
unsafe fn paint_scroll_thumb(hwnd: HWND, hdc: HDC) {
    let Some(sb) = text_scrollbar(hwnd) else {
        return;
    };
    let st = &*state(hwnd);
    let color = if st.scroll_drag.get().is_some() || st.scroll_page_press.get() {
        crate::dark::ACCENT().0
    } else if st.scroll_hot.get() {
        crate::dark::HEADER_TEXT().0
    } else {
        crate::dark::BORDER_STRONG().0
    };
    fill(hdc, &sb.thumb, color);
}

/// A ~13px Consolas monospace font for the text preview (Consolas ships on every Win10/11;
/// the face name drives the monospace look, so pitch-and-family is left at its default).
pub(super) unsafe fn mono_font(hwnd: HWND) -> HFONT {
    use windows::Win32::Graphics::Gdi::{
        CreateFontW, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_QUALITY, OUT_DEFAULT_PRECIS,
    };
    let h = crate::win::dpi_scale(hwnd, 13);
    let face = crate::win::wide("Consolas");
    CreateFontW(
        -h,
        0,
        0,
        0,
        400, // FW_NORMAL
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        DEFAULT_QUALITY,
        Default::default(), // DEFAULT_PITCH | FF_DONTCARE — face name gives monospace
        PCWSTR(face.as_ptr()),
    )
}

/// An icon-font handle at toolbar size (crisp, ClearType-AA glyphs instead of hand-drawn GDI
/// lines). The FACE is whichever icon font this machine actually has -
/// `crate::win::icon_font_face` - because `Segoe Fluent Icons` is Windows 11 only and its
/// absence is silent: GDI substitutes a text font and every glyph becomes an empty box, which
/// is precisely what Windows 10 users saw (issue #21). Caller owns + deletes it.
pub(super) unsafe fn icon_font(hwnd: HWND) -> HFONT {
    use windows::Win32::Graphics::Gdi::{
        CreateFontIndirectW, CLEARTYPE_QUALITY, DEFAULT_CHARSET, LOGFONTW,
    };
    let mut lf = LOGFONTW {
        lfHeight: -crate::win::dpi_scale(hwnd, 15),
        lfWeight: 400,
        lfQuality: CLEARTYPE_QUALITY,
        lfCharSet: DEFAULT_CHARSET,
        ..Default::default()
    };
    let face = crate::win::wide(crate::win::icon_font_face());
    for (i, c) in face.iter().take(lf.lfFaceName.len() - 1).enumerate() {
        lf.lfFaceName[i] = *c;
    }
    CreateFontIndirectW(&lf)
}

/// The Segoe Fluent Icons codepoint for each toolbar button.
pub(super) fn btn_glyph(btn: Btn, pinned: bool) -> u16 {
    match btn {
        Btn::Toc => 0xE8FD,           // BulletedList (outline)
        Btn::MdImages => 0xEB9F,      // Picture (web images on/off)
        Btn::Source => 0xE943,        // Code (`</>`) — view source
        Btn::PdfPrev => 0xE76B,       // ChevronLeft
        Btn::PdfNext => 0xE76C,       // ChevronRight
        Btn::Pin if pinned => 0xE840, // Pinned (filled)
        Btn::Pin => 0xE718,           // Pin
        Btn::Copy => 0xE8C8,          // Copy
        // Never reached: `draw_button` short-circuits Ocr to the vector mark above. Kept so
        // this match stays exhaustive (and harmless if someone routes it back through a font).
        Btn::Ocr => 0xE8D2,      // Font ("A")
        Btn::Info => 0xE946,     // Info
        Btn::Upload => 0xE898,   // Upload (up-arrow to line)
        Btn::Open => 0xE8A7,     // OpenInNewWindow
        Btn::OpenWith => 0xE7AC, // OpenWith
        Btn::Close => 0xE711,    // Cancel (X)
    }
}

/// Draw one toolbar button: the hover pill, then its Segoe Fluent icon glyph, in the accent
/// colour when hovered (or when Pin / the outline toggle is active), else the normal text colour.
#[allow(clippy::too_many_arguments)] // owner-draw helper: many positional draw params by nature
pub(super) unsafe fn draw_button(
    hwnd: HWND,
    hdc: HDC,
    btn: Btn,
    r: &RECT,
    hot: bool,
    pinned: bool,
    toc_open: bool,
    src_view: bool,
    icon: HFONT,
) {
    // Hover background pill.
    if hot {
        let pad = crate::win::dpi_scale(hwnd, 3);
        let pr = RECT {
            left: r.left + pad,
            top: r.top + pad,
            right: r.right - pad,
            bottom: r.bottom - pad,
        };
        fill(hdc, &pr, crate::dark::BTN_FACE_HOT().0);
    }
    let active = (matches!(btn, Btn::Pin) && pinned)
        || (matches!(btn, Btn::Toc) && toc_open)
        || (matches!(btn, Btn::Source) && src_view);
    let color = if hot || active {
        crate::dark::ACCENT().0
    } else {
        crate::dark::DARK_TEXT().0
    };
    // OCR has no icon-font glyph — it's the shared vector mark, so it matches the same
    // button in the screenshot editor's action bar exactly.
    if matches!(btn, Btn::Ocr) {
        crate::gdip::ocr_glyph(hdc, *r, COLORREF(color));
        return;
    }
    let old = SelectObject(hdc, icon.into());
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(color));
    let mut buf = [btn_glyph(btn, pinned)];
    let mut rr = *r;
    DrawTextW(
        hdc,
        &mut buf,
        &mut rr,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
    SelectObject(hdc, old);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// Fabricate a distinct-but-harmless `HWND` for cache-key tests: [`cached_text_lang`] only
    /// ever uses `.0` as a hashmap key and never dereferences it, so an arbitrary value is safe.
    fn fake_hwnd(n: isize) -> HWND {
        HWND(n as *mut std::ffi::c_void)
    }

    /// The whole point of the fix (paint.rs used to re-derive the Text pane's syntax language
    /// on every `WM_PAINT` — every scroll notch, every hover redraw): a repaint at the SAME
    /// load generation must reuse the cached language, not recompute it, and a NEW load (the
    /// generation bumping, e.g. arrow-nav to the next file in the same window) must invalidate
    /// the cache rather than keep serving the previous file's language forever.
    #[test]
    fn cached_text_lang_recomputes_only_when_the_load_generation_changes() {
        let hwnd = fake_hwnd(0x1111);
        let calls = Cell::new(0);
        let compute_rust = || {
            calls.set(calls.get() + 1);
            highlight::Lang::Rust
        };

        assert!(cached_text_lang(hwnd, 1, compute_rust) == highlight::Lang::Rust);
        assert_eq!(calls.get(), 1);

        // Same generation, second repaint: must NOT recompute.
        assert!(cached_text_lang(hwnd, 1, compute_rust) == highlight::Lang::Rust);
        assert_eq!(
            calls.get(),
            1,
            "must not recompute while decode_gen is unchanged"
        );

        // New generation: must invalidate and recompute, and pick up the NEW answer.
        let compute_py = || {
            calls.set(calls.get() + 1);
            highlight::Lang::Py
        };
        assert!(cached_text_lang(hwnd, 2, compute_py) == highlight::Lang::Py);
        assert_eq!(calls.get(), 2);
    }

    /// Two different windows must not share a cache slot — an arbitrary HWND collision would
    /// paint one preview window's file in another window's language.
    #[test]
    fn cached_text_lang_keys_are_per_window() {
        let a = fake_hwnd(0x2222);
        let b = fake_hwnd(0x3333);
        assert!(cached_text_lang(a, 1, || highlight::Lang::Rust) == highlight::Lang::Rust);
        assert!(cached_text_lang(b, 1, || highlight::Lang::Py) == highlight::Lang::Py);
        // `a`'s entry must still read back Rust, not have been clobbered by `b`'s insert.
        assert!(cached_text_lang(a, 1, || highlight::Lang::Py) == highlight::Lang::Rust);
    }

    /// A091: WM_PAINT used to `CreateCompatibleBitmap` a fresh full-client back buffer (tens of
    /// MB at 4K) and delete it on EVERY repaint, instead of caching one sized to the client and
    /// only rebuilding it when that size actually changes (WM_SIZE). Without the cache, this
    /// predicate would need to return `true` unconditionally (every paint reallocates); with it,
    /// a same-size repaint must reuse the buffer and only a real size change forces a rebuild.
    #[test]
    fn back_buffer_reused_across_repaints_reallocated_on_resize() {
        // Nothing cached yet (first paint, or just freed by WM_SIZE/WM_DESTROY) — must allocate.
        assert!(back_buffer_needs_alloc(None, (800, 600)));
        // Same client size as what's cached (a scroll notch, a hover redraw) — must NOT
        // reallocate. This is the fix: the old code had no such check at all.
        assert!(!back_buffer_needs_alloc(Some((800, 600)), (800, 600)));
        // The client size actually changed (WM_SIZE) — the cached bitmap no longer matches the
        // window and MUST be rebuilt, or the next paint would blit a stale-size buffer.
        assert!(back_buffer_needs_alloc(Some((800, 600)), (1024, 768)));
    }
}
