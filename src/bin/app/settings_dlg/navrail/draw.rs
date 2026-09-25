//! Owner-draw: the rail's items with their icons and the pane header above a page.

use super::*;

/// Mix `pct`% of `fg` over `bg` (both 0x00BBGGRR COLORREFs) — for accent tints.
pub(in super::super) fn blend(fg: COLORREF, bg: COLORREF, pct: i32) -> COLORREF {
    let ch = |sh: u32| {
        let a = ((fg.0 >> sh) & 0xFF) as i32;
        let b = ((bg.0 >> sh) & 0xFF) as i32;
        (((a * pct + b * (100 - pct)) / 100) as u32) & 0xFF
    };
    COLORREF(ch(0) | (ch(8) << 8) | (ch(16) << 16))
}

/// Draw a category's line icon (matching the v3 web SVGs) in an `sz`×`sz` box at
/// `(x, y)`, stroked in `color`. Hollow shapes, anti-aliased with round caps/joins via
/// GDI+ (so the diagonals and rounded corners read as clean Fluent line icons instead of
/// the stair-stepped raw-GDI strokes they used to be).
pub(in super::super) unsafe fn draw_cat_icon(
    hdc: HDC,
    ci: usize,
    x: i32,
    y: i32,
    sz: i32,
    color: COLORREF,
) {
    let pw = (sz / 8).max(1);
    // Map the 24-unit SVG space into the box: x-coords via mx, y-coords via my.
    let mx = |v: i32| x + v * sz / 24;
    let my = |v: i32| y + v * sz / 24;
    gdip::with_aa(hdc, |g| {
        let p = gdip::pen_round(color, pw);
        // rounded-rect outline / ellipse outline / polyline, all in the 24-unit SVG space.
        let rr = |a: i32, b: i32, c: i32, d: i32, r: i32| {
            gdip::stroke_round(g, p, mx(a), my(b), mx(c) - mx(a), my(d) - my(b), r);
        };
        let el = |a: i32, b: i32, c: i32, d: i32| {
            gdip::ellipse(g, p, mx(a), my(b), mx(c) - mx(a), my(d) - my(b));
        };
        let ln = |pts: &[(i32, i32)]| {
            let mapped: Vec<(i32, i32)> = pts.iter().map(|&(a, b)| (mx(a), my(b))).collect();
            gdip::polyline(g, p, &mapped);
        };
        match ci {
            0 => {
                // image: framed rect + sun + mountain
                rr(3, 3, 21, 21, sz / 4);
                el(6, 6, 11, 11);
                ln(&[(21, 15), (16, 10), (5, 21)]);
            }
            1 => {
                // appearance: a tile with a corner badge (the format-badge motif)
                rr(3, 3, 18, 18, sz / 6);
                el(13, 13, 21, 21);
            }
            2 => {
                // grid: four rounded squares
                for (gx, gy) in [(3, 3), (13, 3), (3, 13), (13, 13)] {
                    rr(gx, gy, gx + 8, gy + 8, sz / 8);
                }
            }
            3 => {
                // book: cover + spine + page lines (Ebook/comic)
                rr(5, 4, 19, 20, sz / 8);
                ln(&[(8, 4), (8, 20)]);
                ln(&[(11, 9), (16, 9)]);
                ln(&[(11, 13), (16, 13)]);
            }
            4 => {
                // menu: three lines (last shorter)
                for (yy, x2) in [(6, 20), (12, 20), (18, 14)] {
                    ln(&[(4, yy), (x2, yy)]);
                }
            }
            5 => {
                // camera: body + bump + lens
                rr(3, 8, 21, 19, sz / 8);
                ln(&[(8, 8), (9, 6), (15, 6), (16, 8)]);
                el(9, 10, 15, 16);
            }
            6 => {
                // bolt: a lightning shape (Quick action)
                ln(&[
                    (13, 2),
                    (7, 13),
                    (11, 13),
                    (10, 22),
                    (18, 10),
                    (12, 10),
                    (13, 2),
                ]);
            }
            7 => {
                // sliders: two lines, each with a knob (Advanced)
                ln(&[(4, 8), (20, 8)]);
                ln(&[(4, 16), (20, 16)]);
                el(13, 5, 19, 11);
                el(5, 13, 11, 19);
            }
            8 => {
                // eye: a wide almond outline + a round iris (Quick preview)
                el(3, 8, 21, 16);
                el(10, 9, 14, 15);
            }
            9 => {
                // save/backup: a down-arrow into an open tray (Data & Backup)
                ln(&[(12, 3), (12, 14)]);
                ln(&[(8, 10), (12, 14), (16, 10)]);
                ln(&[(4, 16), (4, 21), (20, 21), (20, 16)]);
            }
            _ => {
                // key: a round bow + a shaft with two teeth (Licence)
                el(3, 3, 11, 11);
                ln(&[(9, 9), (21, 21)]);
                ln(&[(15, 15), (18, 12)]);
                ln(&[(18, 18), (21, 15)]);
            }
        }
        gdip::drop_pen(p);
    });
}

pub(in super::super) fn cat_blurb(ci: usize) -> &'static str {
    match ci {
        0 => t("blurb_general"),
        1 => t("blurb_appearance"),
        2 => t("blurb_filetypes"),
        3 => t("blurb_ebook"),
        4 => t("blurb_menu"),
        5 => t("blurb_screenshots"),
        6 => t("blurb_quickaction"),
        7 => t("blurb_advanced"),
        8 => t("blurb_quickpreview"),
        9 => t("blurb_databackup"),
        _ => t("blurb_licence"),
    }
}

/// Owner-draw a nav-rail item: an accent-tinted pill + accent icon + bar when
/// active; a muted icon + plain text otherwise.
pub(in super::super) unsafe fn draw_nav_item(hwnd: HWND, d: &DRAWITEMSTRUCT, active: bool) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    let ci = (d.CtlID as i32 - ID_NAV_BASE) as usize;
    fill(hdc, &rc, DARK_BG());
    if active {
        let tint = blend(ACCENT(), DARK_BG(), 16);
        let (px, py) = (rc.left + dpi_scale(hwnd, 4), rc.top + dpi_scale(hwnd, 3));
        let (pw, ph) = (
            (rc.right - dpi_scale(hwnd, 4)) - px,
            (rc.bottom - dpi_scale(hwnd, 3)) - py,
        );
        gdip::with_aa(hdc, |g| {
            let b = gdip::brush(tint);
            gdip::fill_round(g, b, px, py, pw, ph, dpi_scale(hwnd, 8));
            gdip::drop_brush(b);
        });
        let bar = RECT {
            left: rc.left,
            top: rc.top + dpi_scale(hwnd, 10),
            right: rc.left + dpi_scale(hwnd, 3),
            bottom: rc.bottom - dpi_scale(hwnd, 10),
        };
        fill(hdc, &bar, ACCENT());
    }
    let isz = dpi_scale(hwnd, 17);
    let iy = rc.top + (rc.bottom - rc.top - isz) / 2;
    draw_cat_icon(
        hdc,
        ci,
        rc.left + dpi_scale(hwnd, 16),
        iy,
        isz,
        if active { ACCENT() } else { HEADER_TEXT() },
    );
    // "You changed something here": a small accent dot on the rail row. Answers "where
    // did I change a setting" across nine pages without opening each one. Painted for
    // active and inactive rows alike, so it never reads as part of the selection pill.
    // Clears for good once you've visited the page (see `dot_visible`) — it's a pointer,
    // not a permanent badge.
    if dot_visible(ci) {
        let r = dpi_scale(hwnd, 2);
        let dx = rc.right - dpi_scale(hwnd, 14);
        let dy = rc.top + (rc.bottom - rc.top) / 2 - r;
        gdip::with_aa(hdc, |g| {
            let b = gdip::brush(ACCENT());
            gdip::fill_round(g, b, dx, dy, r * 2, r * 2, r);
            gdip::drop_brush(b);
        });
    }
    SelectObject(hdc, HGDIOBJ(gui_font_for(hwnd).0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, DARK_TEXT());
    let mut label = control_text(d.hwndItem);
    let n = label.len().saturating_sub(1);
    let mut tr = RECT {
        left: rc.left + dpi_scale(hwnd, 44),
        ..rc
    };
    DrawTextW(
        hdc,
        &mut label[..n],
        &mut tr,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
    );
    // Keyboard focus cue: `ODS_FOCUS` is set automatically once `nav_item_subclass`'s
    // WM_GETDLGCODE lets this owner-draw static actually receive focus. Without some visible
    // marker a keyboard user tabbing/arrowing through the rail has no way to see where they are.
    if (d.itemState.0 & ODS_FOCUS.0) != 0 {
        let m = dpi_scale(hwnd, 2);
        gdip::with_aa(hdc, |g| {
            let p = gdip::pen_round(ACCENT(), dpi_scale(hwnd, 1).max(1));
            gdip::stroke_round(
                g,
                p,
                rc.left + m,
                rc.top + m,
                (rc.right - rc.left) - 2 * m,
                (rc.bottom - rc.top) - 2 * m,
                dpi_scale(hwnd, 8),
            );
            gdip::drop_pen(p);
        });
    }
}

/// The cell height one line of the pane-header TITLE needs: `tmHeight` covers ascent + descent,
/// `tmExternalLeading` is the gap the face asks for between lines. Shared by the header draw
/// below and the header-height test so the drawn box and the measured box can't drift apart.
pub(super) fn title_line_height(tm: &TEXTMETRICW) -> i32 {
    tm.tmHeight + tm.tmExternalLeading
}

/// Owner-draw the per-pane header: an accent-tinted icon chip + the active
/// category's bold title + a muted blurb (the v3 page-header look).
pub(in super::super) unsafe fn draw_pane_header(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    fill(hdc, &rc, DARK_BG());
    // The settings-wide search box floats OVER this header, so its rounded field frame has
    // to be drawn here: this owner-draw fills the header's whole rect on every repaint, so
    // anything `paint_chrome` drew on the dialog behind it would be painted out. Same
    // anti-aliased frame every other input on the dialog gets, and the box itself is
    // borderless and fills INPUT_BG, so the two meet with no seam. Coordinates are the
    // HEADER control's client space — hence `d.hwndItem`, not `hwnd`.
    if let Ok(edit) = GetDlgItem(Some(hwnd), ID_SEARCH_GLOBAL) {
        if IsWindowVisible(edit).as_bool() {
            // 5 above / 3 below an 18px edit: a single-line EDIT top-aligns its text, so
            // the ink sits 8px below the edit's top whatever its height. That puts the
            // ink dead centre in the resulting 26px frame.
            super::restyle::draw_rounded_panel(
                d.hwndItem,
                hdc,
                edit,
                INPUT_BG(),
                BORDER(),
                10,
                4,
                5,
                3,
            );
        }
    }
    let ci = NAV.with(|n| n.borrow().active);
    let chip = dpi_scale(hwnd, 34);
    let tint = blend(ACCENT(), DARK_BG(), 16);
    gdip::with_aa(hdc, |g| {
        let b = gdip::brush(tint);
        gdip::fill_round(g, b, rc.left, rc.top, chip, chip, dpi_scale(hwnd, 9));
        gdip::drop_brush(b);
    });
    let isz = dpi_scale(hwnd, 18);
    draw_cat_icon(
        hdc,
        ci,
        rc.left + (chip - isz) / 2,
        rc.top + (chip - isz) / 2,
        isz,
        ACCENT(),
    );
    let tx = rc.left + dpi_scale(hwnd, 46);
    SelectObject(hdc, HGDIOBJ(st2k_appkit::win::gui_font_title(hwnd).0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, DARK_TEXT());
    let mut title = wide(pane_title(ci));
    let tn = title.len().saturating_sub(1);
    // Reserve the right edge for the settings-wide search box that floats over this
    // header — otherwise a long title/blurb runs underneath it. 192 = the box (176) + its
    // frame inflation + the 2px margin that keeps the frame's right round inside the header.
    let text_right = rc.right - dpi_scale(hwnd, 192);
    // Height comes from the FONT, not from a hand-tuned constant. The old box was a flat
    // `dpi_scale(24)`, which happens to sit within a pixel or two of the title font's real cell
    // height — so whether a descender survived came down to rounding, and at 300% scaling the
    // "g" in "Right-Click Menu" lost (issue #26). Every Settings page shares this chrome, which
    // is why the same clipping showed up on Appearance, Quick preview and Data & Backup too.
    //
    // `tmHeight` already covers ascent + descent; `tmExternalLeading` is the gap the face asks
    // for between lines. Taking both means the box grows with any future font change instead of
    // needing the constant re-tuned.
    //
    // Clipping stays ON. DT_NOCLIP was tried and removed: it disables clipping on BOTH axes,
    // so a long localized title would have run straight through the reserved gap and under the
    // floating search box. The measured height is what fixes the descender; DT_END_ELLIPSIS
    // handles the horizontal case properly, truncating with an ellipsis instead of a hard cut.
    let title_top = rc.top - dpi_scale(hwnd, 2);
    let mut tm = TEXTMETRICW::default();
    let line_h = if GetTextMetricsW(hdc, &mut tm).as_bool() {
        title_line_height(&tm)
    } else {
        dpi_scale(hwnd, 26) // metrics unavailable — the old constant, plus the 2px it lost
    };
    let mut tr = RECT {
        left: tx,
        top: title_top,
        right: text_right,
        bottom: title_top + line_h,
    };
    DrawTextW(
        hdc,
        &mut title[..tn],
        &mut tr,
        DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
    );
    SelectObject(hdc, HGDIOBJ(gui_font_for(hwnd).0));
    SetTextColor(hdc, HEADER_TEXT());
    let mut blurb = wide(cat_blurb(ci));
    let bn = blurb.len().saturating_sub(1);
    let mut br = RECT {
        left: tx,
        top: rc.top + dpi_scale(hwnd, 26),
        right: text_right,
        bottom: rc.bottom,
    };
    DrawTextW(
        hdc,
        &mut blurb[..bn],
        &mut br,
        DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
    );
}
