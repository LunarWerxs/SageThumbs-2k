//! Painting the caption: the title and the toolbar buttons with their glyphs and focus ring.

use super::*;

/// The caption strip: background, hairline, file-name title (+ PDF page indicator), toolbar
/// glyphs. Split out of [`paint_into`] to keep that coordinator thin.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn paint_caption(
    hwnd: HWND,
    hdc: HDC,
    st: &ViewerState,
    rc: &RECT,
    caption_rc: &RECT,
    cap_bg: u32,
    text: u32,
    subtle: u32,
) {
    fill(hdc, caption_rc, cap_bg);
    // Hairline under the caption.
    let pen = CreatePen(PS_SOLID, 1, COLORREF(st2k_appkit::dark::BORDER().0));
    let old = SelectObject(hdc, HGDIOBJ(pen.0));
    let _ = MoveToEx(hdc, 0, caption_rc.bottom - 1, None);
    let _ = LineTo(hdc, rc.right, caption_rc.bottom - 1);
    SelectObject(hdc, old);
    let _ = DeleteObject(HGDIOBJ(pen.0));

    let buttons = button_rects(hwnd);
    SetBkMode(hdc, TRANSPARENT);
    paint_caption_title(hwnd, hdc, st, rc, caption_rc, text, subtle, &buttons);
    paint_caption_toolbar(hwnd, hdc, st, &buttons);
}

/// Title (file name), left-aligned in the caption, plus the "N / M" PDF page indicator when
/// applicable, right-aligned just before the toolbar buttons.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn paint_caption_title(
    hwnd: HWND,
    hdc: HDC,
    st: &ViewerState,
    rc: &RECT,
    caption_rc: &RECT,
    text: u32,
    subtle: u32,
    buttons: &[(Btn, RECT)],
) {
    let title_right = buttons
        .iter()
        .map(|(_, r)| r.left)
        .min()
        .unwrap_or(rc.right)
        - st2k_appkit::win::dpi_scale(hwnd, PAD);
    SetTextColor(hdc, COLORREF(text));
    let tf = st2k_appkit::win::gui_font_for(hwnd);
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
        st2k_appkit::win::dpi_scale(hwnd, 72)
    } else {
        0
    };
    let mut title = file_leaf_name(st)
        .unwrap_or_default()
        .encode_utf16()
        .collect::<Vec<u16>>();
    let mut trc = RECT {
        left: st2k_appkit::win::dpi_scale(hwnd, PAD + 4),
        top: 0,
        right: title_right - label_w,
        bottom: caption_rc.bottom,
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
            bottom: caption_rc.bottom,
        };
        DrawTextW(
            hdc,
            &mut w,
            &mut lr,
            DT_RIGHT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
    }
    SelectObject(hdc, oldf);
}

/// Toolbar glyphs. `buttons` is laid out right-to-left, but `st.hot` is a BTNS index (what
/// `hit_button` returns), so resolve each drawn button back to its BTNS index to match — else
/// the highlight mirrors (hover right, light left). One shared icon-font handle for the whole
/// toolbar (grayscale-antialiased, face chosen by `icon_font_face`, like the screenshot tool).
pub(super) unsafe fn paint_caption_toolbar(
    hwnd: HWND,
    hdc: HDC,
    st: &ViewerState,
    buttons: &[(Btn, RECT)],
) {
    let icon = icon_font(hwnd);
    let focus_i = match live_focus(st.focus.get(), st.focus_gen.get(), st.decode_gen.get()) {
        Some(FocusTarget::Caption(i)) => Some(i),
        _ => None,
    };
    for (idx, (b, r)) in buttons.iter().enumerate() {
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
        if focus_i == Some(idx) {
            draw_toolbar_focus_ring(hwnd, hdc, r);
        }
    }
    let _ = DeleteObject(icon.into());
}

/// `r` shrunk by 3 DPI-scaled px on each side: the hover-pill / focus-ring inset of the caption
/// toolbar buttons.
pub(super) fn inset_rect(hwnd: HWND, r: &RECT) -> RECT {
    let pad = st2k_appkit::win::dpi_scale(hwnd, 3);
    RECT {
        left: r.left + pad,
        top: r.top + pad,
        right: r.right - pad,
        bottom: r.bottom - pad,
    }
}

/// Keyboard-focus ring for one caption toolbar button: a 1px accent frame drawn just inside the
/// button's hover-pill rect (same inset [`draw_button`] uses for the hover pill itself), so
/// focus reads as an outline around that pill rather than a second hover state, and never
/// overlaps a neighbouring button.
pub(super) unsafe fn draw_toolbar_focus_ring(hwnd: HWND, hdc: HDC, r: &RECT) {
    let pr = inset_rect(hwnd, r);
    let b = CreateSolidBrush(COLORREF(st2k_appkit::dark::ACCENT().0));
    FrameRect(hdc, &pr, b);
    let _ = DeleteObject(b.into());
}

/// The icon-font codepoint for each toolbar button (the bundled face maps Material Symbols
/// onto these Segoe codepoints — see `scripts/build-icon-font.py`).
pub(in super::super) fn btn_glyph(btn: Btn, pinned: bool) -> u16 {
    match btn {
        Btn::Toc => 0xE8FD,      // BulletedList (outline)
        Btn::MdImages => 0xEB9F, // Picture (web images on/off)
        Btn::Source => 0xE943,   // Code (`</>`) — view source
        Btn::PdfPrev => 0xE76B,  // ChevronLeft
        Btn::PdfNext => 0xE76C,  // ChevronRight
        // The theme button shows the theme it will switch TO, which is the convention every
        // light/dark toggle uses: a sun while you are in the dark skin, a moon while light.
        // Drawing the CURRENT state instead reads as a status light, and users click it
        // expecting the mode already shown.
        Btn::Theme if st2k_appkit::dark::is_dark() => 0xE706, // light_mode (sun)
        Btn::Theme => 0xE708,                                 // dark_mode (moon)
        Btn::Settings => 0xE713,                              // gear
        Btn::Pin if pinned => 0xE840,                         // Pinned (filled)
        Btn::Pin => 0xE718,                                   // Pin
        Btn::Copy => 0xE8C8,                                  // Copy
        Btn::SavePage => 0xE74E,                              // Save
        // Never reached: `draw_button` short-circuits Ocr to the vector mark above. Kept so
        // this match stays exhaustive (and harmless if someone routes it back through a font).
        Btn::Ocr => 0xE8D2,      // Font ("A")
        Btn::Info => 0xE946,     // Info
        Btn::Upload => 0xE898,   // Upload (up-arrow to line)
        Btn::Open => 0xE8A7,     // OpenInNewWindow
        Btn::OpenWith => 0xE7AC, // OpenWith
        Btn::Print => 0xE749,    // Print
        Btn::Close => 0xE711,    // Cancel (X)
    }
}

/// Draw one toolbar button: the hover pill, then its Segoe Fluent icon glyph, in the accent
/// colour when hovered (or when Pin / the outline toggle is active), else the normal text colour.
#[allow(clippy::too_many_arguments)] // owner-draw helper: many positional draw params by nature
pub(in super::super) unsafe fn draw_button(
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
        let pr = inset_rect(hwnd, r);
        fill(hdc, &pr, st2k_appkit::dark::BTN_FACE_HOT().0);
    }
    let active = (matches!(btn, Btn::Pin) && pinned)
        || (matches!(btn, Btn::Toc) && toc_open)
        || (matches!(btn, Btn::Source) && src_view);
    let color = if hot || active {
        st2k_appkit::dark::ACCENT().0
    } else {
        st2k_appkit::dark::DARK_TEXT().0
    };
    // OCR has no icon-font glyph — it's the shared vector mark, so it matches the same
    // button in the screenshot editor's action bar exactly.
    if matches!(btn, Btn::Ocr) {
        st2k_appkit::gdip::ocr_glyph(hdc, *r, COLORREF(color), icon_em(hwnd));
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
