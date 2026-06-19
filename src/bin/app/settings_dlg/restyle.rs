//! Dark-mode-only painting that gives the Settings window its 2026 look: blue
//! rounded checkboxes, solid-accent + outlined push buttons, uppercase section
//! headers with a divider line, a zebra-striped format list with an accent
//! extension column, and the column/footer hairlines. Every entry point here is
//! reached only behind an `is_dark()` guard in `wndproc`, so light mode stays the
//! untouched native dialog.

use super::*;

/// Owner-draw a section header: a muted, uppercase label followed by a hairline
/// that runs from after the text to the control's right edge.
pub(super) unsafe fn draw_section_header(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    fill(hdc, &rc, DARK_BG());

    // Drop a trailing ':' and uppercase (ASCII; non-Latin scripts are unchanged).
    let raw = control_text(d.hwndItem);
    let n = raw.len().saturating_sub(1);
    let mut label = String::from_utf16_lossy(&raw[..n]);
    if label.ends_with(':') {
        label.pop();
    }
    let mut text = wide(&label.to_uppercase());
    let tn = text.len().saturating_sub(1);

    SelectObject(hdc, HGDIOBJ(gui_font_header(hwnd).0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, HEADER_TEXT());
    let mut sz = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &text[..tn], &mut sz);
    // Letter-spacing (tracking) so the uppercase label reads as a header and
    // doesn't look cramped. Applied for the draw only, then reset.
    let track = s(hwnd, 1);
    SetTextCharacterExtra(hdc, track);
    let mut tr = rc;
    DrawTextW(hdc, &mut text[..tn], &mut tr, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
    SetTextCharacterExtra(hdc, 0);

    // Divider, vertically centered, from just after the text (incl. tracking) to
    // the right edge.
    let lx = rc.left + sz.cx + tn as i32 * track + s(hwnd, 12);
    let ly = (rc.top + rc.bottom) / 2;
    if lx < rc.right {
        let line = RECT { left: lx, top: ly, right: rc.right, bottom: ly + s(hwnd, 1).max(1) };
        fill(hdc, &line, BORDER());
    }
}

/// Route a button-class NM_CUSTOMDRAW to the checkbox or push-button painter,
/// returning the CDRF_* result for the wndproc to hand back.
pub(super) unsafe fn draw_button_cd(hwnd: HWND, nmcd: *const NMCUSTOMDRAW) -> isize {
    let from = (*nmcd).hdr.hwndFrom;
    let kind = GetWindowLongW(from, GWL_STYLE) as u32 & 0xF;
    if kind == BS_AUTOCHECKBOX as u32 || kind == BS_CHECKBOX as u32 {
        draw_checkbox(hwnd, nmcd)
    } else {
        draw_pushbutton(hwnd, nmcd)
    }
}

/// Draw the rounded check-box glyph — a `g`×`g` box at `x`, vertically centered
/// in `top..bottom`: accent fill + white tick when `on`, outlined otherwise.
/// Shared by the panel checkboxes and the per-row list checkboxes so both match.
#[allow(clippy::too_many_arguments)] // owner-draw helper: many positional draw params by nature
unsafe fn draw_check_glyph(
    hwnd: HWND,
    hdc: HDC,
    x: i32,
    top: i32,
    bottom: i32,
    g: i32,
    rad: i32,
    on: bool,
    active: bool,
) {
    let y = top + (bottom - top - g) / 2;
    SelectObject(hdc, GetStockObject(DC_BRUSH));
    SelectObject(hdc, GetStockObject(DC_PEN));
    if on {
        SetDCBrushColor(hdc, ACCENT());
        SetDCPenColor(hdc, if active { ACCENT_HOT() } else { ACCENT() });
    } else {
        SetDCBrushColor(hdc, CHECK_BG());
        SetDCPenColor(hdc, if active { ACCENT() } else { BORDER_STRONG() });
    }
    let _ = RoundRect(hdc, x, y, x + g, y + g, rad, rad);
    if on {
        let pen = CreatePen(PS_SOLID, s(hwnd, 2).max(1), ON_ACCENT());
        let old = SelectObject(hdc, HGDIOBJ(pen.0));
        let pts = [
            POINT { x: x + g * 27 / 100, y: y + g * 52 / 100 },
            POINT { x: x + g * 43 / 100, y: y + g * 68 / 100 },
            POINT { x: x + g * 73 / 100, y: y + g * 33 / 100 },
        ];
        let _ = Polyline(hdc, &pts);
        SelectObject(hdc, old);
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }
}

/// A flat rounded checkbox: an 18-px box (accent fill + white tick when checked,
/// outlined otherwise) followed by the label. The control stays a
/// BS_AUTOCHECKBOX, so its checked state, keyboard toggle and the
/// `check()`/`checked()` helpers keep working — we only repaint it.
unsafe fn draw_checkbox(hwnd: HWND, nmcd: *const NMCUSTOMDRAW) -> isize {
    let cd = &*nmcd;
    let hdc = cd.hdc;
    let from = cd.hdr.hwndFrom;
    let rc = cd.rc;
    let active = (cd.uItemState.0 & (CDIS_HOT.0 | CDIS_FOCUS.0)) != 0;
    let on = checked(hwnd, GetDlgCtrlID(from));

    fill(hdc, &rc, DARK_BG());

    let g = s(hwnd, 18);
    let gx = rc.left + s(hwnd, 1);
    draw_check_glyph(hwnd, hdc, gx, rc.top, rc.bottom, g, s(hwnd, 5), on, active);

    let tx = gx + g + s(hwnd, 10);
    let mut tr = RECT { left: tx, top: rc.top, right: rc.right, bottom: rc.bottom };
    SelectObject(hdc, HGDIOBJ(gui_font_for(hwnd).0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, DARK_TEXT());
    let mut label = control_text(from);
    let n = label.len().saturating_sub(1);
    DrawTextW(
        hdc,
        &mut label[..n],
        &mut tr,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
    );
    CDRF_SKIPDEFAULT as isize
}

/// A rounded push button: solid accent for the primary actions (Select all /
/// Save), an outlined dark face for the rest, with hover/press shading.
unsafe fn draw_pushbutton(hwnd: HWND, nmcd: *const NMCUSTOMDRAW) -> isize {
    let cd = &*nmcd;
    let hdc = cd.hdc;
    let from = cd.hdr.hwndFrom;
    let rc = cd.rc;
    let id = GetDlgCtrlID(from);
    let hot = (cd.uItemState.0 & CDIS_HOT.0) != 0;
    let pressed = (cd.uItemState.0 & CDIS_SELECTED.0) != 0;
    let focus = (cd.uItemState.0 & CDIS_FOCUS.0) != 0;
    let disabled = (cd.uItemState.0 & windows::Win32::UI::Controls::CDIS_DISABLED.0) != 0;
    let accent = id == ID_SELECT_ALL || id == IDOK;

    fill(hdc, &rc, DARK_BG());

    let (face, border, text) = if disabled {
        // Greyed (flat face, dim border + text) — e.g. Restart hotkey service while the
        // hotkey is off. Native Win32 greys disabled buttons; our owner-draw must too.
        (BTN_FACE(), BORDER(), DISABLED_TEXT())
    } else if accent {
        let f = if pressed { ACCENT_PRESS() } else if hot { ACCENT_HOT() } else { ACCENT() };
        (f, f, ON_ACCENT())
    } else {
        let f = if pressed { BTN_FACE_PRESS() } else if hot { BTN_FACE_HOT() } else { BTN_FACE() };
        (f, if hot || focus { BORDER_STRONG() } else { BORDER() }, DARK_TEXT())
    };
    let rad = s(hwnd, 8);
    SelectObject(hdc, GetStockObject(DC_BRUSH));
    SelectObject(hdc, GetStockObject(DC_PEN));
    SetDCBrushColor(hdc, face);
    SetDCPenColor(hdc, border);
    let inset = s(hwnd, 1);
    let _ = RoundRect(hdc, rc.left, rc.top, rc.right - inset, rc.bottom - inset, rad, rad);

    SelectObject(hdc, HGDIOBJ(gui_font_for(hwnd).0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, text);
    let mut label = control_text(from);
    let n = label.len().saturating_sub(1);
    let mut tr = rc;
    DrawTextW(
        hdc,
        &mut label[..n],
        &mut tr,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
    );
    CDRF_SKIPDEFAULT as isize
}

/// Format-list custom draw: zebra rows, an accent selection, an accent extension
/// column and a muted category column. (The per-row checkbox glyph is still the
/// control's — it's the enable/disable switch for each format.)
pub(super) unsafe fn draw_list_item(p: *mut NMLVCUSTOMDRAW) -> isize {
    let lv = &mut *p;
    let list = lv.nmcd.hdr.hwndFrom;
    let stage = lv.nmcd.dwDrawStage.0;
    if stage == CDDS_PREPAINT.0 {
        return CDRF_NOTIFYITEMDRAW as isize;
    }
    if stage == CDDS_ITEMPREPAINT.0 {
        let row = lv.nmcd.dwItemSpec as i32;
        let selected = (lv.nmcd.uItemState.0 & CDIS_SELECTED.0) != 0;
        lv.clrTextBk = if selected {
            SEL_BG()
        } else if row % 2 == 1 {
            ZEBRA()
        } else {
            SURFACE()
        };
        lv.clrText = DARK_TEXT();
        // Also want a post-paint pass to restyle the row's checkbox.
        return (CDRF_NOTIFYSUBITEMDRAW | CDRF_NOTIFYPOSTPAINT) as isize;
    }
    if stage == (CDDS_ITEMPREPAINT.0 | CDDS_SUBITEM.0) {
        lv.clrText = match lv.iSubItem {
            0 => ACCENT_TEXT(), // extension (.jpg …) in accent
            1 => HEADER_TEXT(), // category, muted
            _ => DARK_TEXT(),   // description
        };
        return CDRF_NEWFONT as isize;
    }
    if stage == CDDS_ITEMPOSTPAINT.0 {
        // Replace the native square system-accent checkbox with our rounded
        // accent glyph, so the per-row switch matches the panel checkboxes.
        let hdc = lv.nmcd.hdc;
        let row = lv.nmcd.dwItemSpec as i32;
        let selected = (lv.nmcd.uItemState.0 & CDIS_SELECTED.0) != 0;
        let bg = if selected {
            SEL_BG()
        } else if row % 2 == 1 {
            ZEBRA()
        } else {
            SURFACE()
        };
        let mut rr = RECT { left: 0 /* LVIR_BOUNDS */, ..Default::default() };
        SendMessageW(
            list,
            LVM_GETITEMRECT,
            Some(WPARAM(row as usize)),
            Some(LPARAM(&mut rr as *mut _ as isize)),
        );
        // Erase the native checkbox gutter, then draw ours centered in it.
        let gutter = RECT { left: rr.left, top: rr.top, right: rr.left + s(list, 20), bottom: rr.bottom };
        fill(hdc, &gutter, bg);
        let on = is_checked(list, row);
        draw_check_glyph(list, hdc, rr.left + s(list, 4), rr.top, rr.bottom, s(list, 14), s(list, 4), on, false);
        return CDRF_DODEFAULT as isize;
    }
    CDRF_DODEFAULT as isize
}

/// A rounded "field"/"card" frame drawn on the dialog background just behind a
/// child control: fills the control's rect (inflated by `infl` design px) with
/// `fill_c` and outlines it with `border_c`, corners rounded to `ell` design px.
/// The control (same fill color) paints on top, so it reads as one soft-edged
/// inset field / raised card.
#[allow(clippy::too_many_arguments)] // owner-draw helper: many positional draw params by nature
unsafe fn draw_rounded_panel(
    hwnd: HWND,
    hdc: HDC,
    ctrl: HWND,
    fill_c: COLORREF,
    border_c: COLORREF,
    ell: i32,
    ix: i32,
    iy_top: i32,
    iy_bottom: i32,
) {
    let mut wr = RECT::default();
    if GetWindowRect(ctrl, &mut wr).is_err() {
        return;
    }
    let mut tl = POINT { x: wr.left, y: wr.top };
    let mut br = POINT { x: wr.right, y: wr.bottom };
    let _ = ScreenToClient(hwnd, &mut tl);
    let _ = ScreenToClient(hwnd, &mut br);
    let (ix, iy_top, iy_bottom, e) =
        (s(hwnd, ix), s(hwnd, iy_top), s(hwnd, iy_bottom), s(hwnd, ell));
    SelectObject(hdc, GetStockObject(DC_BRUSH));
    SelectObject(hdc, GetStockObject(DC_PEN));
    SetDCBrushColor(hdc, fill_c);
    SetDCPenColor(hdc, border_c);
    let _ = RoundRect(hdc, tl.x - ix, tl.y - iy_top, br.x + ix, br.y + iy_bottom, e, e);
}

/// Paint the dialog "chrome": the rounded file-list card + the rounded input /
/// dropdown fields (behind their controls), then the column + footer hairlines.
pub(super) unsafe fn paint_chrome(hwnd: HWND, hdc: HDC) {
    if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
        draw_rounded_panel(hwnd, hdc, list, SURFACE(), BORDER(), 16, 3, 3, 3);
    }
    // Field frames hug the controls. The snug (18px) edits hold digits/short text,
    // whose font cell centers but whose ink rides high (no descender), so the frame
    // is biased UP — 6px above, 2px below — measured to put the visible ink dead
    // center (gap above == gap below). The auto-height combos draw their own text
    // with DT_VCENTER, so they keep a tight symmetric frame.
    // Only frame controls that are actually visible. A left-column field scrolled
    // out of the viewport is SW_HIDE'd; drawing its panel anyway leaks a faint
    // rounded rect below the clip mask (it was bleeding around the footer credit).
    for id in [ID_MAXSIZE, ID_SIZE, ID_JPEG, ID_PNG, ID_SEARCH] {
        if let Ok(c) = GetDlgItem(Some(hwnd), id) {
            if IsWindowVisible(c).as_bool() {
                draw_rounded_panel(hwnd, hdc, c, INPUT_BG(), BORDER(), 10, 4, 6, 2);
            }
        }
    }
    for id in [ID_MENU_PREVIEW, ID_LANG, ID_SHOT_HOTKEY] {
        if let Ok(c) = GetDlgItem(Some(hwnd), id) {
            if IsWindowVisible(c).as_bool() {
                draw_rounded_panel(hwnd, hdc, c, INPUT_BG(), BORDER(), 10, 4, 2, 2);
            }
        }
    }
    // (The horizontal rule above the banner is drawn by the left mask's owner-draw.)
}

/// Apply the combo overpaint subclass — both themes (it owner-paints the closed
/// face with the theme-aware palette).
pub(super) unsafe fn dark_combo_subclass(combo: HWND, id: i32) {
    let _ = SetWindowSubclass(combo, Some(combo_subclass), id as usize, 0);
}

/// A CBS_DROPDOWNLIST combo's themed dark paint still leaves a light inner edit
/// border and a white dropdown button. Rather than patch those, this subclass
/// fully owner-draws the closed combo: a flat dark fill (matching the rounded
/// field frame behind it), the current selection text, and our own chevron. The
/// dropdown list popup stays themed dark via `dark_theme_combo`.
unsafe extern "system" fn combo_subclass(
    h: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    uid: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(h, Some(combo_subclass), uid);
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(h, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(h, &mut rc);
            fill(hdc, &rc, INPUT_BG());
            // Dim the text + chevron when the combo is disabled (e.g. the quick-save
            // picker while "instant screenshot" is off). Native Win32 greys a disabled
            // combo automatically; our owner-draw must do it explicitly.
            let enabled = windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(h).as_bool();
            let text_col = if enabled { DARK_TEXT() } else { DISABLED_TEXT() };
            let chevron_col = if enabled { HEADER_TEXT() } else { DISABLED_TEXT() };
            let bw = s(h, 18); // reserved chevron column on the right

            // Current selection text, left-aligned.
            let sel = SendMessageW(h, CB_GETCURSEL, None, None).0;
            if sel >= 0 {
                let len = SendMessageW(h, CB_GETLBTEXTLEN, Some(WPARAM(sel as usize)), None).0;
                if len > 0 {
                    let mut buf = vec![0u16; len as usize + 1];
                    SendMessageW(
                        h,
                        CB_GETLBTEXT,
                        Some(WPARAM(sel as usize)),
                        Some(LPARAM(buf.as_mut_ptr() as isize)),
                    );
                    SelectObject(hdc, HGDIOBJ(gui_font_for(h).0));
                    SetBkMode(hdc, TRANSPARENT);
                    SetTextColor(hdc, text_col);
                    let mut tr =
                        RECT { left: rc.left + s(h, 9), top: rc.top, right: rc.right - bw, bottom: rc.bottom };
                    DrawTextW(
                        hdc,
                        &mut buf[..len as usize],
                        &mut tr,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
                    );
                }
            }

            // Chevron, centered in the reserved column.
            let cx = rc.right - bw / 2;
            let cy = (rc.top + rc.bottom) / 2;
            let d = s(h, 3);
            let pen = CreatePen(PS_SOLID, s(h, 2).max(1), chevron_col);
            let old = SelectObject(hdc, HGDIOBJ(pen.0));
            let pts = [
                POINT { x: cx - d, y: cy - d / 2 },
                POINT { x: cx, y: cy + d / 2 },
                POINT { x: cx + d, y: cy - d / 2 },
            ];
            let _ = Polyline(hdc, &pts);
            SelectObject(hdc, old);
            let _ = DeleteObject(HGDIOBJ(pen.0));
            let _ = EndPaint(h, &ps);
            return LRESULT(0);
        }
        WM_ENABLE => {
            // Repaint with the new enabled/disabled text colour when the combo is
            // enabled/disabled (e.g. the quick-save picker toggling). Fall through to
            // DefSubclassProc for the default enable handling.
            let _ = InvalidateRect(Some(h), None, false);
        }
        WM_MOUSEWHEEL => {
            // A hovered/focused CLOSED dropdown normally eats the wheel and CHANGES
            // its selection — a maddening way to accidentally flip Language/hotkey
            // while scrolling the page. Forward the wheel to the parent dialog so the
            // PAGE scrolls instead. (An OPEN dropdown's list is a separate window, so
            // it still scrolls normally when actually browsing it.)
            if let Ok(parent) = GetParent(h) {
                return SendMessageW(parent, msg, Some(w), Some(l));
            }
            return LRESULT(0);
        }
        _ => {}
    }
    DefSubclassProc(h, msg, w, l)
}

/// Forward `WM_MOUSEWHEEL` to the parent dialog. Standard child controls
/// (checkbox / static / edit) consume the wheel and never bubble it up, so without
/// this the dark-mode left column won't scroll while the cursor is over a control —
/// i.e. over most of the column. `scroll::init_scroll` applies this to every
/// left-column child so the wheel scrolls the page no matter what it's over.
pub(super) unsafe extern "system" fn wheel_forward_subclass(
    h: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    uid: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(h, Some(wheel_forward_subclass), uid);
        }
        WM_MOUSEWHEEL => {
            if let Ok(parent) = GetParent(h) {
                return SendMessageW(parent, msg, Some(w), Some(l));
            }
            return LRESULT(0);
        }
        _ => {}
    }
    DefSubclassProc(h, msg, w, l)
}

/// Owner-draw the left scrollbar so its track blends with the column background
/// (no contrasting groove) — just a rounded thumb sized/positioned from the
/// scroll info. Hit-testing/arrows still work (the control handles those).
pub(super) unsafe extern "system" fn scrollbar_subclass(
    h: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    uid: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(h, Some(scrollbar_subclass), uid);
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(h, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(h, &mut rc);
            fill(hdc, &rc, DARK_BG()); // track = column background
            let mut si = SCROLLINFO {
                cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                fMask: SIF_ALL,
                ..Default::default()
            };
            let _ = GetScrollInfo(h, SB_CTL, &mut si);
            let range = (si.nMax - si.nMin + 1).max(1);
            let track_h = (rc.bottom - rc.top).max(1);
            let page = (si.nPage as i32).max(1);
            let thumb_h = ((page * track_h) / range).clamp(s(h, 28), track_h);
            let max_pos = (range - page).max(1);
            let pos = si.nPos.clamp(0, max_pos);
            let thumb_y = (pos * (track_h - thumb_h)) / max_pos;
            let pad = s(h, 4); // thinner thumb (~6px) to match the list's native scrollbar
            SelectObject(hdc, GetStockObject(DC_BRUSH));
            SelectObject(hdc, GetStockObject(DC_PEN));
            SetDCBrushColor(hdc, BORDER_STRONG());
            SetDCPenColor(hdc, BORDER_STRONG());
            let rad = s(h, 4);
            let _ = RoundRect(hdc, rc.left + pad, rc.top + thumb_y, rc.right - pad, rc.top + thumb_y + thumb_h, rad, rad);
            let _ = EndPaint(h, &ps);
            return LRESULT(0);
        }
        _ => {}
    }
    DefSubclassProc(h, msg, w, l)
}
