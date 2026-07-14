//! All viewer painting: content arms, text/code, toolbar glyphs.


use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    COLORREF, HWND, RECT,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreatePen, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, LineTo, MoveToEx, SelectObject, SetBkMode,
    SetTextColor, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX,
    DT_RIGHT, DT_SINGLELINE, DT_VCENTER, HDC, HFONT, HGDIOBJ, PAINTSTRUCT,
    PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::content::{self};
use super::{highlight, infocard};
use super::window::{ContentKind, Btn, BTNS, state, CAPTION_H, PAD};
use super::toolbar::button_rects; use super::transport::{scrub_rect, video_rect, draw_scrub_strip};

// ===== Painting =====

pub(super) unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    if !hdc.is_invalid() {
        // Double-buffer: render the whole client into an off-screen bitmap, then blit it once.
        // Painting straight to the window DC drew the content-bg fill and then the text/lines
        // separately on-screen, which FLASHED on every scroll notch. One BitBlt = no flash.
        // (BeginPaint's DC is clipped to the invalid region, so the blit only touches what changed.)
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let w = (rc.right - rc.left).max(1);
        let h = (rc.bottom - rc.top).max(1);
        let mem = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, w, h);
        if !mem.is_invalid() && !bmp.is_invalid() {
            let old = SelectObject(mem, bmp.into());
            paint_into(hwnd, mem);
            let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
            SelectObject(mem, old);
        } else {
            paint_into(hwnd, hdc); // buffer alloc failed — paint directly (correct, just flickers)
        }
        if !bmp.is_invalid() {
            let _ = DeleteObject(bmp.into());
        }
        if !mem.is_invalid() {
            let _ = DeleteDC(mem);
        }
    }
    let _ = EndPaint(hwnd, &ps);
}

pub(super) unsafe fn paint_into(hwnd: HWND, hdc: HDC) {
    let st = &*state(hwnd);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
    let caption_rc = RECT { left: 0, top: 0, right: rc.right, bottom: cap };
    let content_rc = RECT { left: 0, top: cap, right: rc.right, bottom: rc.bottom };

    let content_bg = crate::dark::SURFACE().0;
    let cap_bg = crate::dark::DARK_BG().0;
    let text = crate::dark::DARK_TEXT().0;
    let subtle = crate::dark::HEADER_TEXT().0;

    // Content.
    match st.kind.get() {
        ContentKind::Image => {
            let frames = st.frames.borrow();
            if let Some(rd) = frames.get(st.cur_frame.get()) {
                content::paint_image(hdc, &content_rc, rd, content_bg, st.zoom.get(), st.pan.get());
            } else if let Some(rd) = st.render.borrow().as_ref() {
                content::paint_image(hdc, &content_rc, rd, content_bg, st.zoom.get(), st.pan.get());
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
                let ext = st
                    .path
                    .borrow()
                    .as_deref()
                    .and_then(|p| std::path::Path::new(p).extension().and_then(|e| e.to_str()))
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let lang = highlight::lang_from_ext(&ext);
                let th = paint_text(hwnd, hdc, &content_rc, t, lang, content_bg, text, st.text_scroll.get());
                st.text_h.set(th); // remember for the wheel handler's scroll clamp
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
                let md_rc = RECT { left: content_rc.left + sidebar_w, ..content_rc };
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
                let th = super::markdown::render(
                    hwnd, hdc, &md_rc, t, scroll, &cols, &mut links, &mut toc, &mut imgs,
                    doc_dir.as_deref(), st.decode_gen.get(), st.md_remote_ok.get(), &mut layout,
                );
                drop(layout);
                st.text_h.set(th);
                drop(links);
                drop(imgs);
                if show_toc {
                    let side_rc = RECT { right: content_rc.left + sidebar_w, ..content_rc };
                    let mut hits = st.toc_hits.borrow_mut();
                    paint_toc(hwnd, hdc, &side_rc, &toc, scroll, st.toc_sel.get(), &mut hits);
                } else {
                    st.toc_hits.borrow_mut().clear();
                }
            } else {
                st.md_links.borrow_mut().clear();
                st.toc_hits.borrow_mut().clear();
                paint_message(hwnd, hdc, &content_rc, content_bg, subtle, "");
            }
        }
        ContentKind::Video => {
            // The video render child covers the video area; paint black behind it (brief
            // pre-first-frame) then draw the transport strip in the bottom band.
            let vr = video_rect(hwnd);
            let brush = CreateSolidBrush(COLORREF(0x0000_0000));
            FillRect(hdc, &vr, brush);
            let _ = DeleteObject(brush.into());
            if let Some(v) = st.video.borrow().as_ref() {
                draw_scrub_strip(hwnd, hdc, &scrub_rect(hwnd), v, text, subtle);
            }
        }
        ContentKind::Loading => paint_message(hwnd, hdc, &content_rc, content_bg, subtle, "Loading…"),
        ContentKind::Html => {
            // The WebView2 child window renders over the content area; just fill behind it.
            let brush = CreateSolidBrush(COLORREF(content_bg));
            FillRect(hdc, &content_rc, brush);
            let _ = DeleteObject(brush.into());
        }
    }

    // Scroll-position thumb for the text + markdown panes (they have no OS scrollbar). Drawn on top
    // of the content, only when it's taller than the viewport, so you can see where you are.
    if matches!(st.kind.get(), ContentKind::Text | ContentKind::Markdown) {
        paint_scroll_thumb(hwnd, hdc, &content_rc, st.text_scroll.get(), st.text_h.get());
    }

    // Caption strip.
    let brush = CreateSolidBrush(COLORREF(cap_bg));
    FillRect(hdc, &caption_rc, brush);
    let _ = DeleteObject(brush.into());
    // Hairline under the caption.
    let pen = CreatePen(PS_SOLID, 1, COLORREF(crate::dark::BORDER().0));
    let old = SelectObject(hdc, HGDIOBJ(pen.0));
    let _ = MoveToEx(hdc, 0, cap - 1, None);
    let _ = LineTo(hdc, rc.right, cap - 1);
    SelectObject(hdc, old);
    let _ = DeleteObject(HGDIOBJ(pen.0));

    // Title (file name), left-aligned in the caption.
    let buttons = button_rects(hwnd);
    let title_right = buttons.iter().map(|(_, r)| r.left).min().unwrap_or(rc.right) - crate::win::dpi_scale(hwnd, PAD);
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(text));
    let tf = crate::win::gui_font_for(hwnd);
    let oldf = SelectObject(hdc, tf.into());
    // PDF page indicator "N / M", right-aligned before the buttons — same visibility rule as
    // the pager buttons (multi-page PDF showing as an image; not on the InfoCard fallback).
    let pdf_lbl = if st.kind.get() == ContentKind::Image && st.pdf_pages.get() > 1 {
        Some(format!("{} / {}", st.pdf_page.get() + 1, st.pdf_pages.get()))
    } else {
        None
    };
    let label_w = if pdf_lbl.is_some() { crate::win::dpi_scale(hwnd, 72) } else { 0 };
    let mut title = st
        .path
        .borrow()
        .as_ref()
        .and_then(|p| std::path::Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default()
        .encode_utf16()
        .collect::<Vec<u16>>();
    let mut trc = RECT {
        left: crate::win::dpi_scale(hwnd, PAD + 4),
        top: 0,
        right: title_right - label_w,
        bottom: cap,
    };
    if !title.is_empty() {
        DrawTextW(hdc, &mut title, &mut trc, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
    }
    if let Some(lbl) = pdf_lbl {
        SetTextColor(hdc, COLORREF(subtle));
        let mut w: Vec<u16> = lbl.encode_utf16().collect();
        let mut lr = RECT { left: title_right - label_w, top: 0, right: title_right, bottom: cap };
        DrawTextW(hdc, &mut w, &mut lr, DT_RIGHT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
    }
    SelectObject(hdc, oldf);

    // Toolbar glyphs. `buttons` is laid out right-to-left, but `st.hot` is a BTNS index (what
    // `hit_button` returns), so resolve each drawn button back to its BTNS index to match — else
    // the highlight mirrors (hover right, light left). One shared Segoe Fluent Icons font for the
    // whole toolbar (crisp ClearType native glyphs, like the screenshot tool).
    let icon = icon_font(hwnd);
    for (b, r) in buttons.iter() {
        let hot = st.hot.get() == BTNS.iter().position(|&bb| bb == *b);
        draw_button(hwnd, hdc, *b, r, hot, st.pinned.get(), st.toc_open.get(), icon);
    }
    let _ = DeleteObject(icon.into());
}

/// Centered single-line message (e.g. "Loading…") in `rc`.
pub(super) unsafe fn paint_message(hwnd: HWND, hdc: HDC, rc: &RECT, bg: u32, color: u32, text: &str) {
    let brush = CreateSolidBrush(COLORREF(bg));
    FillRect(hdc, rc, brush);
    let _ = DeleteObject(brush.into());
    if text.is_empty() {
        return;
    }
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(color));
    let f = crate::win::gui_font_for(hwnd);
    let old = SelectObject(hdc, f.into());
    let mut w: Vec<u16> = text.encode_utf16().collect();
    let mut r = *rc;
    DrawTextW(hdc, &mut w, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
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

    let brush = CreateSolidBrush(COLORREF(bg));
    FillRect(hdc, rc, brush);
    let _ = DeleteObject(brush.into());
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
    let mut hr = RECT { left: rc.left + pad, top: y, right: rc.right - pad, bottom: y + row_h };
    DrawTextW(hdc, &mut hdr, &mut hr, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX);
    y += row_h + sc(4);

    // The "current" section: an explicitly-clicked entry wins (bottom sections can't scroll to
    // the pane top, so the click must still visibly select); otherwise the last heading at or
    // above the scroll position.
    let cur = sel.filter(|i| *i < toc.len()).or_else(|| toc.iter().rposition(|e| e.target <= scroll + sc(4)));
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
        let mut r = RECT { left: rc.left + indent, top: y, right: rc.right - sc(8), bottom: y + row_h };
        DrawTextW(hdc, &mut label, &mut r, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);
        hits.push((RECT { left: rc.left, top: y, right: rc.right, bottom: y + row_h }, i));
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
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn paint_text(hwnd: HWND, hdc: HDC, rc: &RECT, text: &str, lang: highlight::Lang, bg: u32, fg: u32, scroll: i32) -> i32 {
    let brush = CreateSolidBrush(COLORREF(bg));
    FillRect(hdc, rc, brush);
    let _ = DeleteObject(brush.into());
    let m = crate::win::dpi_scale(hwnd, 12);
    SetBkMode(hdc, TRANSPARENT);
    let font = mono_font(hwnd);
    let width = (rc.right - rc.left - 2 * m).max(1);
    // Plain text draws every run in `fg` (no keywords), so this covers both plain and code.
    let text_h = highlight::paint_lines(hdc, text, lang, rc.left + m, rc.top + m - scroll, width, rc.top, rc.bottom, font, fg);
    let _ = DeleteObject(font.into());
    text_h
}

/// A thin scroll-position indicator on the right edge of `content_rc`. The text/markdown panes
/// have no OS scrollbar, so without this you can't tell where you are or whether a wheel notch
/// registered. Sized/positioned from the same (scroll, text_h, visible) math as `scroll_text`, so
/// it tracks the real position; hidden when everything already fits.
unsafe fn paint_scroll_thumb(hwnd: HWND, hdc: HDC, content_rc: &RECT, scroll: i32, text_h: i32) {
    let m = crate::win::dpi_scale(hwnd, 12);
    let track_h = (content_rc.bottom - content_rc.top).max(1);
    let visible = (track_h - 2 * m).max(1); // mirrors scroll_text's visible-height math
    let max_scroll = text_h - visible;
    if max_scroll <= 0 {
        return; // content fits — nothing to scroll, so no thumb
    }
    let min_thumb = crate::win::dpi_scale(hwnd, 32);
    let thumb_h = ((visible * track_h) / text_h.max(1)).clamp(min_thumb, track_h);
    let scroll = scroll.clamp(0, max_scroll);
    let thumb_y = content_rc.top + (scroll * (track_h - thumb_h)) / max_scroll;
    let tw = crate::win::dpi_scale(hwnd, 4);
    let pad = crate::win::dpi_scale(hwnd, 3);
    let thumb = RECT {
        left: content_rc.right - pad - tw,
        top: thumb_y,
        right: content_rc.right - pad,
        bottom: thumb_y + thumb_h,
    };
    let brush = CreateSolidBrush(COLORREF(crate::dark::BORDER_STRONG().0));
    FillRect(hdc, &thumb, brush);
    let _ = DeleteObject(brush.into());
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
        -h, 0, 0, 0,
        400, // FW_NORMAL
        0, 0, 0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        DEFAULT_QUALITY,
        Default::default(), // DEFAULT_PITCH | FF_DONTCARE — face name gives monospace
        PCWSTR(face.as_ptr()),
    )
}

/// A **Segoe Fluent Icons** handle at toolbar size (crisp, ClearType-AA native Win11 glyphs —
/// the same font the screenshot tool uses, instead of hand-drawn GDI lines). If the font is
/// absent (older Windows), GDI substitutes and the glyph degrades to a box — acceptable on the
/// Win11-targeted app. Caller owns + deletes it.
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
    let face = crate::win::wide("Segoe Fluent Icons");
    for (i, c) in face.iter().take(lf.lfFaceName.len() - 1).enumerate() {
        lf.lfFaceName[i] = *c;
    }
    CreateFontIndirectW(&lf)
}

/// The Segoe Fluent Icons codepoint for each toolbar button.
pub(super) fn btn_glyph(btn: Btn, pinned: bool) -> u16 {
    match btn {
        Btn::Toc => 0xE8FD,     // BulletedList (outline)
        Btn::PdfPrev => 0xE76B, // ChevronLeft
        Btn::PdfNext => 0xE76C, // ChevronRight
        Btn::Pin if pinned => 0xE840, // Pinned (filled)
        Btn::Pin => 0xE718,     // Pin
        Btn::Copy => 0xE8C8,    // Copy
        Btn::Info => 0xE946,    // Info
        Btn::Upload => 0xE898,  // Upload (up-arrow to line)
        Btn::Open => 0xE8A7,    // OpenInNewWindow
        Btn::OpenWith => 0xE7AC, // OpenWith
        Btn::Close => 0xE711,   // Cancel (X)
    }
}

/// Draw one toolbar button: the hover pill, then its Segoe Fluent icon glyph, in the accent
/// colour when hovered (or when Pin / the outline toggle is active), else the normal text colour.
#[allow(clippy::too_many_arguments)] // owner-draw helper: many positional draw params by nature
pub(super) unsafe fn draw_button(hwnd: HWND, hdc: HDC, btn: Btn, r: &RECT, hot: bool, pinned: bool, toc_open: bool, icon: HFONT) {
    // Hover background pill.
    if hot {
        let hb = CreateSolidBrush(COLORREF(crate::dark::BTN_FACE_HOT().0));
        let pad = crate::win::dpi_scale(hwnd, 3);
        let pr = RECT { left: r.left + pad, top: r.top + pad, right: r.right - pad, bottom: r.bottom - pad };
        FillRect(hdc, &pr, hb);
        let _ = DeleteObject(hb.into());
    }
    let active = (matches!(btn, Btn::Pin) && pinned) || (matches!(btn, Btn::Toc) && toc_open);
    let color = if hot || active { crate::dark::ACCENT().0 } else { crate::dark::DARK_TEXT().0 };
    let old = SelectObject(hdc, icon.into());
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(color));
    let mut buf = [btn_glyph(btn, pinned)];
    let mut rr = *r;
    DrawTextW(hdc, &mut buf, &mut rr, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    SelectObject(hdc, old);
}
