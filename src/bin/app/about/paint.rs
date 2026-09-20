//! Owner-draw for the About box: the version, status and feedback pills, the spinner and the muted statics.

use super::*;

/// Text extent of `text` in the HDC's currently-selected font.
pub(super) unsafe fn measure(hdc: HDC, text: &str) -> i32 {
    let w = wide(text);
    let n = w.len().saturating_sub(1);
    let mut sz = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &w[..n], &mut sz);
    sz.cx
}

pub(super) unsafe fn fill_rc(hdc: HDC, rc: &RECT, color: COLORREF) {
    SetDCBrushColor(hdc, color);
    FillRect(hdc, rc, HBRUSH(GetStockObject(DC_BRUSH).0));
}

/// Paint the rounded pill frame (face + hairline border) into `rc` — full-stadium
/// rounding (ellipse == height).
pub(super) unsafe fn pill_frame(hwnd: HWND, hdc: HDC, rc: &RECT) {
    SelectObject(hdc, GetStockObject(DC_BRUSH));
    SelectObject(hdc, GetStockObject(DC_PEN));
    SetDCBrushColor(hdc, BTN_FACE());
    SetDCPenColor(hdc, BORDER_STRONG());
    let h = rc.bottom - rc.top;
    let inset = s(hwnd, 1);
    let _ = RoundRect(
        hdc,
        rc.left,
        rc.top,
        rc.right - inset,
        rc.bottom - inset,
        h,
        h,
    );
}

/// Blit an opaque bitmap into `dst` at `(x,y)`, `w`×`h`.
pub(super) unsafe fn blit(dst: HDC, hbmp: HBITMAP, x: i32, y: i32, w: i32, h: i32) {
    let mdc = CreateCompatibleDC(Some(dst));
    if mdc.is_invalid() {
        return;
    }
    let old = SelectObject(mdc, HGDIOBJ(hbmp.0));
    let _ = BitBlt(dst, x, y, w, h, Some(mdc), 0, 0, SRCCOPY);
    SelectObject(mdc, old);
    let _ = DeleteDC(mdc);
}

/// Draw text left-aligned + vertically centered starting at `left`.
pub(super) unsafe fn draw_text(hdc: HDC, text: &str, left: i32, rc: &RECT, color: COLORREF) {
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, color);
    let mut buf = wide(text);
    let n = buf.len().saturating_sub(1);
    let mut tr = RECT {
        left,
        top: rc.top,
        right: rc.right,
        bottom: rc.bottom,
    };
    DrawTextW(
        hdc,
        &mut buf[..n],
        &mut tr,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
}

pub(super) unsafe fn draw_ver_pill(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    fill_rc(hdc, &rc, DARK_BG());
    pill_frame(hwnd, hdc, &rc);

    let icon_px = s(hwnd, ICON);
    let gap = s(hwnd, 7);
    let ver = format!("v{}", env!("CARGO_PKG_VERSION"));
    SelectObject(hdc, HGDIOBJ(gui_font_for(hwnd).0));
    let tw = measure(hdc, &ver);
    let group = icon_px + gap + tw;
    let gx = rc.left + ((rc.right - rc.left) - group) / 2;
    let iy = rc.top + ((rc.bottom - rc.top) - icon_px) / 2;
    let st = about_state(hwnd);
    if !st.is_null() {
        if let Some(icon) = (*st).gh_icon {
            blit(hdc, icon, gx, iy, icon_px, icon_px);
        }
    }
    draw_text(hdc, &ver, gx + icon_px + gap, &rc, DARK_TEXT());
}

/// Map the current status to (dot colour, label).
pub(super) unsafe fn status_display(st: *mut About) -> (COLORREF, String) {
    if st.is_null() {
        return (rgb(150, 150, 150), t("about_checking").to_string());
    }
    match &(*st).status {
        Status::Idle => (rgb(150, 150, 150), t("about_check_now").to_string()),
        Status::Checking => (rgb(150, 150, 150), t("about_checking").to_string()),
        Status::UpToDate => (rgb(63, 185, 80), t("about_uptodate").to_string()),
        Status::Available(latest) => (
            rgb(210, 153, 34),
            // Outside this machine's updates window the pill says so in place of the plain
            // "Update to X", so the state is visible before anything is clicked.
            match update::offer_for(&crate::license::snapshot(), Some(latest)) {
                update::Offer::OutsideWindow { .. } => {
                    format!("{} {}", t("about_update_outside"), latest.tag)
                }
                _ => format!("{} {}", t("about_update"), latest.tag),
            },
        ),
        Status::Failed => (rgb(190, 110, 110), t("about_check_failed").to_string()),
    }
}

/// A rotating 270° arc (a classic loading ring) centered at `(cx,cy)`, radius `r`, oriented
/// by `frame` so successive repaints appear to spin. The two radial endpoints only pick the
/// sweep, so the exact angle-sign convention doesn't matter — either direction reads as
/// "spinning". Uses a DPI-scaled pen freed before returning.
pub(super) unsafe fn draw_spinner(
    hwnd: HWND,
    hdc: HDC,
    cx: i32,
    cy: i32,
    r: i32,
    frame: u32,
    color: COLORREF,
) {
    use core::f32::consts::PI;
    let pen_w = s(hwnd, 2).max(1);
    let pen = CreatePen(PS_SOLID, pen_w, color);
    if pen.is_invalid() {
        return;
    }
    let old = SelectObject(hdc, HGDIOBJ(pen.0));
    let t0 = (frame as f32) * 12.0 * PI / 180.0; // ~12°/frame → a smooth, clearly visible spin
    let t1 = t0 + 270.0 * PI / 180.0; // a gapped ring, not a closed circle
    let (rf, cxf, cyf) = (r as f32, cx as f32, cy as f32);
    let sx = (cxf + rf * t0.cos()).round() as i32;
    let sy = (cyf - rf * t0.sin()).round() as i32;
    let ex = (cxf + rf * t1.cos()).round() as i32;
    let ey = (cyf - rf * t1.sin()).round() as i32;
    let _ = Arc(hdc, cx - r, cy - r, cx + r, cy + r, sx, sy, ex, ey);
    SelectObject(hdc, old);
    let _ = DeleteObject(HGDIOBJ(pen.0));
}

pub(super) unsafe fn draw_status_pill(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    fill_rc(hdc, &rc, DARK_BG());
    pill_frame(hwnd, hdc, &rc);

    let st = about_state(hwnd);
    let (dot, text) = status_display(st);
    let checking = !st.is_null() && matches!((*st).status, Status::Checking);
    let frame = if st.is_null() { 0 } else { (*st).spin_frame };
    let dotd = s(hwnd, 10);
    let gap = s(hwnd, 8);
    SelectObject(hdc, HGDIOBJ(gui_font_for(hwnd).0));
    let tw = measure(hdc, &text);
    let group = dotd + gap + tw;
    let gx = rc.left + ((rc.right - rc.left) - group) / 2;
    let dy = rc.top + ((rc.bottom - rc.top) - dotd) / 2;
    if checking {
        // Spinning ring in the dot's slot — the moving "faux" activity while we check.
        let r = (dotd / 2 - s(hwnd, 1)).max(2);
        draw_spinner(hwnd, hdc, gx + dotd / 2, dy + dotd / 2, r, frame, dot);
    } else {
        // Resting status dot.
        SelectObject(hdc, GetStockObject(DC_BRUSH));
        SelectObject(hdc, GetStockObject(DC_PEN));
        SetDCBrushColor(hdc, dot);
        SetDCPenColor(hdc, dot);
        let _ = Ellipse(hdc, gx, dy, gx + dotd, dy + dotd);
    }
    draw_text(hdc, &text, gx + dotd + gap, &rc, DARK_TEXT());
}

/// The "Send feedback" pill: the same stadium as its neighbours, with the label
/// centered. No icon or dot — those two carry state (a version, a check result);
/// this one is a plain action, so centered text is what reads as "click me".
pub(super) unsafe fn draw_feedback_pill(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    fill_rc(hdc, &rc, DARK_BG());
    pill_frame(hwnd, hdc, &rc);

    let label = t("fb_pill");
    SelectObject(hdc, HGDIOBJ(gui_font_for(hwnd).0));
    let tw = measure(hdc, label);
    let gx = rc.left + ((rc.right - rc.left) - tw) / 2;
    draw_text(hdc, label, gx, &rc, DARK_TEXT());
}

pub(super) unsafe fn ctlcolor_text(hdc: HDC, color: COLORREF) -> LRESULT {
    SetTextColor(hdc, color);
    SetBkColor(hdc, DARK_BG());
    SetBkMode(hdc, TRANSPARENT);
    LRESULT(dark_bg_brush().0 as isize)
}

/// Muted on-surface colours for the subtitle / license / copyright statics, handled BEFORE
/// the generic static colouring so they don't get the default text colour. `None` if `id`
/// names none of them.
pub(super) unsafe fn muted_static_color(wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    let id = GetDlgCtrlID(HWND(lparam.0 as *mut c_void));
    let hdc = HDC(wparam.0 as *mut c_void);
    let muted = match id {
        ID_SUBTITLE | ID_LICENSE | ID_LICENCE_STATE => Some(HEADER_TEXT()),
        ID_COPYRIGHT => Some(DISABLED_TEXT()),
        _ => None,
    };
    muted.map(|c| ctlcolor_text(hdc, c))
}
