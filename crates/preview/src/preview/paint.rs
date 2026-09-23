//! All viewer painting: content arms, text/code, toolbar glyphs.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreatePen, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect, GetStockObject, LineTo,
    MoveToEx, SelectObject, SetBkMode, SetDCBrushColor, SetTextColor, DC_BRUSH, DRAW_TEXT_FORMAT,
    DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, HBRUSH,
    HDC, HFONT, HGDIOBJ, PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::*;
mod contentpaint;
use contentpaint::*;
mod captionpaint;
use captionpaint::*;

use super::content::{self};
use super::selection::sel_range;
use super::toolbar::{button_rects, live_focus, FocusTarget};
use super::transport::{draw_scrub_strip, scrub_rect, video_rect, TBTNS};
use super::window::{
    clamp_text_scroll, file_leaf_name, state, text_scrollbar, Btn, ContentKind, ViewerState, BTNS,
    CAPTION_H, PAD,
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
    // Re-point the tooltip rects at the layout this paint is about to draw, BEFORE `BeginPaint`
    // — the sync sends messages to the tooltip control, and nothing should do that between
    // BeginPaint and EndPaint. It is a `Vec` compare and returns immediately unless a button
    // actually moved. See `toolbar::update_tooltips`.
    let st = &*state(hwnd);
    super::toolbar::update_tooltips(hwnd, st.tip.get());
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
    let cap = st2k_appkit::win::dpi_scale(hwnd, CAPTION_H);
    let caption_rc = RECT {
        left: 0,
        top: 0,
        right: rc.right,
        bottom: cap,
    };
    // Same rect `content_rect` computes, kept in step with it: the find bar, when open, takes a
    // strip off the top and everything below lays out inside what is left.
    let content_rc = super::window::content_rect(hwnd);

    let content_bg = st2k_appkit::dark::SURFACE().0;
    let cap_bg = st2k_appkit::dark::DARK_BG().0;
    let text = st2k_appkit::dark::DARK_TEXT().0;
    let subtle = st2k_appkit::dark::HEADER_TEXT().0;

    // Content.
    paint_content(hwnd, hdc, st, &content_rc, content_bg, text, subtle);

    // Scroll-position thumb for the text + markdown panes (they have no OS scrollbar). Drawn on top
    // of the content, only when it's taller than the viewport, so you can see where you are.
    if matches!(st.kind.get(), ContentKind::Text | ContentKind::Markdown) {
        paint_scroll_thumb(hwnd, hdc);
    }

    // Find bar, between the caption and the content (a no-op when closed).
    super::find::paint(hwnd, hdc, text, subtle);

    paint_caption(hwnd, hdc, st, &rc, &caption_rc, cap_bg, text, subtle);
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
    let f = st2k_appkit::win::gui_font_for(hwnd);
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

/// Draw the Markdown outline (table-of-contents) sidebar into `rc`: a localized header (English
/// default: CONTENTS) + one row
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
    let sc = |v: i32| st2k_appkit::win::dpi_scale(hwnd, v);
    let bg = st2k_appkit::dark::DARK_BG().0;
    let fg = st2k_appkit::dark::DARK_TEXT().0;
    let muted = st2k_appkit::dark::HEADER_TEXT().0;
    let accent = st2k_appkit::dark::ACCENT().0;

    fill(hdc, rc, bg);
    // right-edge separator
    let pen = CreatePen(PS_SOLID, 1, COLORREF(st2k_appkit::dark::BORDER().0));
    let op = SelectObject(hdc, HGDIOBJ(pen.0));
    let _ = MoveToEx(hdc, rc.right - 1, rc.top, None);
    let _ = LineTo(hdc, rc.right - 1, rc.bottom);
    SelectObject(hdc, op);
    let _ = DeleteObject(HGDIOBJ(pen.0));

    let f = st2k_appkit::win::gui_font_for(hwnd);
    let old = SelectObject(hdc, f.into());
    SetBkMode(hdc, TRANSPARENT);
    let pad = sc(14);
    let row_h = sc(22);
    let mut y = rc.top + pad;

    SetTextColor(hdc, COLORREF(muted));
    // Localized (audit F29, 2026-09-06): pre-fix this fed the bare word CONTENTS straight to
    // encode_utf16, so a portable install with the language set to anything but English showed
    // an English header on top of an otherwise fully translated app.
    let mut hdr: Vec<u16> = st2k_appkit::win::t("preview_outline_header")
        .encode_utf16()
        .collect();
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
    let m = st2k_appkit::win::dpi_scale(hwnd, 12);
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
        st2k_appkit::dark::ACCENT().0
    } else if st.scroll_hot.get() {
        st2k_appkit::dark::HEADER_TEXT().0
    } else {
        st2k_appkit::dark::BORDER_STRONG().0
    };
    fill(hdc, &sb.thumb, color);
}

/// A ~13px Consolas monospace font for the text preview (Consolas ships on every Win10/11;
/// the face name drives the monospace look, so pitch-and-family is left at its default).
pub(super) unsafe fn mono_font(hwnd: HWND) -> HFONT {
    use windows::Win32::Graphics::Gdi::{
        CreateFontW, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_QUALITY, OUT_DEFAULT_PRECIS,
    };
    let h = st2k_appkit::win::dpi_scale(hwnd, 13);
    let face = st2k_appkit::win::wide("Consolas");
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

/// The caption toolbar's icon-font em, in device px. Named because the hand-drawn OCR mark
/// has to be sized off the SAME number as the font glyphs beside it (see
/// [`st2k_appkit::gdip::ocr_glyph`]) — passing the button rect instead is what made that one mark
/// come out half again as big as everything else.
///
/// **16, not 15, and the difference is measured.** These outlines are unhinted (Material
/// Symbols ships no instructions and autohinting them was tried and made things very slightly
/// WORSE), so nothing snaps the strokes to the pixel grid and how much of a glyph lands on
/// whole pixels depends on where its stem widths happen to fall at a given size. Sweeping em
/// 13..24 through real GDI, the share of fully-covered pixels goes 40, 50, 52, **58**, 48, 52,
/// 53, 55, 58, 71: 16 is a local peak and 17 falls off a cliff. It holds at the scaled sizes
/// too (125% -> em 20 at 55%, 150% -> em 24 at 71%, both ahead of what 15 scales to).
///
/// It also collapses a difference that had no reason to exist: the screenshot editor's action
/// bar already drew at 16, so both toolbars now share one em, and `ocr_glyph`'s `7/4 em` lands
/// its cell on exactly the 28 px that mark was designed against.
pub(super) fn icon_em(hwnd: HWND) -> i32 {
    st2k_appkit::win::dpi_scale(hwnd, 16)
}

/// An icon-font handle at toolbar size. The FACE is whichever icon font this machine actually
/// has - `st2k_appkit::win::icon_font_face` - because `Segoe Fluent Icons` is Windows 11 only and its
/// absence is silent: GDI substitutes a text font and every glyph becomes an empty box, which
/// is precisely what Windows 10 users saw (issue #21). Caller owns + deletes it.
pub(super) unsafe fn icon_font(hwnd: HWND) -> HFONT {
    st2k_appkit::win::icon_font(icon_em(hwnd))
}

#[cfg(test)]
mod tests;
